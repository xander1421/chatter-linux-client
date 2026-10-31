use super::*;

impl UnifiedMessageRepository {
    // ========================================================================
    // Reactions (unified_reactions table)
    // ========================================================================

    /// Save only decoded authenticated reaction intent before acknowledging transport.
    #[allow(clippy::too_many_arguments)]
    pub fn hold_received_reaction(
        &self,
        scope: &ReactionScope,
        author: &UserId,
        target_ts: u64,
        actor: &UserId,
        emoji: &str,
        remove: bool,
        action_ts: u64,
    ) -> Result<(), StorageError> {
        super::super::message_modifier_repo::MessageModifierRepository::new(self.db.clone())
            .queue_reaction(
                author,
                target_ts,
                actor,
                emoji,
                remove,
                scope,
                action_ts,
                crate::util::time::TimestampNanos::now().as_u64(),
            )
    }

    /// Consume a durable handoff only after target arrival and roster authority.
    /// Unresolved legacy ownership keeps its intent without changing history.
    pub fn apply_held_reaction_scope(
        &self,
        scope: &ReactionScope,
        author: &UserId,
        target_ts: u64,
    ) -> Result<bool, StorageError> {
        let mut conn = self.db.connection()?;
        let tx = conn.transaction()?;
        let changed = self.apply_held_reaction_scope_with_conn(&tx, scope, author, target_ts)?;
        tx.commit()?;
        Ok(changed)
    }

    fn apply_held_reaction_scope_with_conn(
        &self,
        tx: &rusqlite::Connection,
        scope: &ReactionScope,
        author: &UserId,
        target_ts: u64,
    ) -> Result<bool, StorageError> {
        let target=tx.query_row(&format!("SELECT {} FROM messages WHERE conversation_id=?1 AND conversation_type=?2 AND sender_id=?3 AND sent_at=?4 AND is_deleted=0",message_columns("messages")),params![scope.id.to_string(),scope.kind.as_str(),author.to_string(),target_ts as i64],row_to_message).optional()?.transpose()?;
        let Some(target) = target else {
            return Ok(false);
        };
        let own: Option<String> = tx
            .query_row(
                "SELECT value FROM settings WHERE key='auth.current_user_id'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        let own = own
            .map(|v| {
                serde_json::from_str::<String>(&v).map_err(|_| {
                    StorageError::InvalidData("Invalid reaction account identity".into())
                })
            })
            .transpose()?;
        let mut stmt=tx.prepare("SELECT actor_id,emoji,remove,action_ts FROM pending_scoped_reactions WHERE conversation_id=?1 AND conversation_type=?2 AND author_id=?3 AND target_sent_at=?4 UNION ALL SELECT actor_id,emoji,0,action_ts FROM pending_reaction_add_selections WHERE conversation_id=?1 AND conversation_type=?2 AND author_id=?3 AND target_sent_at=?4 ORDER BY action_ts,remove,emoji")?;
        let rows = stmt
            .query_map(
                params![
                    scope.id.to_string(),
                    scope.kind.as_str(),
                    author.to_string(),
                    target_ts as i64
                ],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, bool>(2)?,
                        r.get::<_, i64>(3)? as u64,
                    ))
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;
        drop(stmt);
        let mut accepted = Vec::new();
        for (actor, emoji, remove, action_ts) in rows {
            if super::super::message_modifier_repo::may_modify(
                &tx,
                &target,
                &actor,
                own.as_deref() == Some(actor.as_str()),
            )? {
                let reactor = actor
                    .parse()
                    .map_err(|_| StorageError::InvalidData("Invalid held reaction actor".into()))?;
                tx.execute("DELETE FROM pending_scoped_reactions WHERE conversation_id=?1 AND conversation_type=?2 AND author_id=?3 AND target_sent_at=?4 AND actor_id=?5 AND emoji=?6",params![scope.id.to_string(),scope.kind.as_str(),author.to_string(),target_ts as i64,actor,emoji])?;
                tx.execute("DELETE FROM pending_reaction_add_selections WHERE conversation_id=?1 AND conversation_type=?2 AND author_id=?3 AND target_sent_at=?4 AND actor_id=?5",params![scope.id.to_string(),scope.kind.as_str(),author.to_string(),target_ts as i64,actor])?;
                accepted.push(super::super::message_modifier_repo::PendingReaction {
                    reactor,
                    emoji,
                    remove,
                    action_ts,
                });
            }
        }
        self.apply_held_reactions(tx, &target, &accepted)
    }

    /// Bind this profile and exact mailbox before staged Signal receive.
    pub(crate) fn reaction_receive_key(
        &self,
        transport: &str,
        scope: ChannelId,
        envelope: &str,
        timestamp: u64,
    ) -> Result<crate::storage::receive_artifact::ReceiveKey, StorageError> {
        let conn = self.db.read_connection()?;
        let (account, device) = reaction_receive_binding(&conn)?;
        let key = crate::storage::receive_artifact::ReceiveKey {
            local_account: account,
            local_device: device,
            transport: transport.into(),
            scope: scope.to_string(),
            envelope_id: envelope.into(),
            server_timestamp_ns: timestamp,
        };
        key.validate()?;
        Ok(key)
    }

    /// Transaction B: authenticated exact artifact, scoped intent/projections,
    /// and applied flag commit together. Replays never reapply an applied vote.
    pub(crate) fn apply_reaction_artifact(
        &self,
        artifact: &crate::storage::receive_artifact::ReceiveArtifact,
    ) -> Result<Option<(ReactionScope, UserId, u64)>, StorageError> {
        use crate::storage::receive_artifact::{self, ReceiveKind};
        if artifact.kind != ReceiveKind::Reaction {
            return Err(StorageError::InvalidData("receive kind mismatch".into()));
        }
        let mut conn = self.db.connection()?;
        let tx = conn.transaction()?;
        let (account, device) = reaction_receive_binding(&tx)?;
        if account != artifact.key.local_account || device != artifact.key.local_device {
            return Err(StorageError::InvalidData(
                "Reaction receive account changed".into(),
            ));
        }
        let stored =
            receive_artifact::load(&tx, &artifact.key, &artifact.digest)?.ok_or_else(|| {
                StorageError::InvalidData("Authenticated receive artifact missing".into())
            })?;
        let mut expected = artifact.clone();
        expected.applied = stored.applied;
        expected.acknowledged = stored.acknowledged;
        if stored.acknowledged {
            expected.content.clear();
        }
        if stored != expected {
            return Err(StorageError::InvalidData(
                "Authenticated receive artifact mismatch".into(),
            ));
        }
        if stored.applied {
            return Ok(None);
        }
        let mut scope = ReactionScope::new(
            stored
                .key
                .scope
                .parse()
                .map_err(|_| StorageError::InvalidData("Invalid receive scope".into()))?,
            match stored.key.transport.as_str() {
                "anon_dm" => ConversationType::AnonDm,
                "anon_group" => ConversationType::AnonGroup,
                _ => {
                    return Err(StorageError::InvalidData(
                        "Invalid receive transport".into(),
                    ));
                }
            },
        );
        // Invalid payload destination is authenticated rejected input: preserve
        // its journal evidence but never let it choose a foreign target.
        let intent = crate::inbox::reaction_handoff::decode_artifact_reaction(&stored, &scope);
        let mut applied = None;
        if let Some((author, reaction)) = intent {
            // Only server channels send a `channel:` target, and their rows
            // are always stored as Channel, whichever transport carried it.
            if reaction.target_author_aci.starts_with("channel:") {
                scope.kind = ConversationType::Channel;
            }
            let actor: UserId = stored.sender.parse().map_err(|_| {
                StorageError::InvalidData("Invalid certified reaction sender".into())
            })?;
            let own = actor.to_string() == account;
            // Same oriented durable friendship block used by Gateway; this
            // artifact path bypasses its nonreaction router, not its block policy.
            let blocked:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM friendships WHERE user_id=?1 AND friend_id=?2 AND status=2)",params![account,actor.to_string()],|r|r.get(0))?;
            let allowed=!blocked && (own || match scope.kind {
                ConversationType::AnonDm=>tx.query_row("SELECT EXISTS(SELECT 1 FROM anon_dm_conversations WHERE id=?1 AND peer_id=?2 AND peer_device_id=?3 AND status=1 AND length(peer_mailbox_pk)=32) AND NOT EXISTS(SELECT 1 FROM anon_dm_blocked_users WHERE user_id=?2)",params![scope.id.to_string(),actor.to_string(),stored.sender_device],|r|r.get::<_,bool>(0))?,
                ConversationType::Channel=>tx.query_row("SELECT EXISTS(SELECT 1 FROM memberships WHERE channel_id=?1 AND user_id=?2)",params![scope.id.to_string(),actor.to_string()],|r|r.get::<_,bool>(0))?,
                ConversationType::AnonGroup=>tx.query_row("SELECT EXISTS(SELECT 1 FROM anon_group_members g JOIN anon_groups a ON a.id=g.group_id WHERE g.group_id=?1 AND g.user_id=?2 AND g.is_active=1 AND a.status<>2) AND NOT EXISTS(SELECT 1 FROM anon_group_blocked_users WHERE group_id=?1 AND user_id=?2)",params![scope.id.to_string(),actor.to_string()],|r|r.get::<_,bool>(0))?,
                _=>false,
            });
            if !allowed && !blocked {
                let refused:bool=match scope.kind {
                    ConversationType::AnonDm=>tx.query_row("SELECT EXISTS(SELECT 1 FROM anon_dm_blocked_users WHERE user_id=?2) OR EXISTS(SELECT 1 FROM anon_dm_conversations WHERE id=?1 AND (status IN (2,3) OR peer_id<>?2 OR (status=1 AND peer_device_id IS NOT ?3)))",params![scope.id.to_string(),actor.to_string(),stored.sender_device],|r|r.get(0))?,
                    ConversationType::AnonGroup=>tx.query_row("SELECT EXISTS(SELECT 1 FROM anon_group_blocked_users WHERE group_id=?1 AND user_id=?2) OR EXISTS(SELECT 1 FROM anon_groups WHERE id=?1 AND status=2)",params![scope.id.to_string(),actor.to_string()],|r|r.get(0))?,
                    _=>false,
                };
                if !refused {
                    return Err(StorageError::InvalidData(
                        "Reaction roster/readiness unavailable; authenticated artifact retained"
                            .into(),
                    ));
                }
            }
            if allowed {
                let action = if reaction.timestamp > 0 {
                    reaction.timestamp
                } else {
                    stored.key.server_timestamp_ns
                };
                super::super::message_modifier_repo::queue_reaction_with_conn(
                    &tx,
                    &author,
                    reaction.target_sent_timestamp,
                    &actor,
                    &reaction.emoji,
                    reaction.remove,
                    &scope,
                    action,
                    crate::util::time::TimestampNanos::now().as_u64(),
                )?;
                if self.apply_held_reaction_scope_with_conn(
                    &tx,
                    &scope,
                    &author,
                    reaction.target_sent_timestamp,
                )? {
                    applied = Some((scope, author, reaction.target_sent_timestamp));
                }
            }
        }
        receive_artifact::mark_applied(&tx, &stored)?;
        tx.commit()?;
        Ok(applied)
    }

    pub(crate) fn acknowledge_receive_artifact(
        &self,
        key: &crate::storage::receive_artifact::ReceiveKey,
        digest: &[u8],
    ) -> Result<(), StorageError> {
        use crate::storage::receive_artifact;
        let mut conn = self.db.connection()?;
        let tx = conn.transaction()?;
        receive_artifact::require_current_binding(&tx, key)?;
        match receive_artifact::load(&tx, key, digest)? {
            Some(artifact) if artifact.kind.is_content() => {
                receive_artifact::mark_content_upstream_acked(&tx, key, digest)?;
            }
            Some(_) => receive_artifact::mark_acknowledged(&tx, key, digest)?,
            None => {}
        }
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn receive_artifact_kind(
        &self,
        key: &crate::storage::receive_artifact::ReceiveKey,
        digest: &[u8],
    ) -> Result<Option<crate::storage::receive_artifact::ReceiveKind>, StorageError> {
        let conn = self.db.read_connection()?;
        Ok(crate::storage::receive_artifact::load(&conn, key, digest)?.map(|a| a.kind))
    }

    pub(crate) fn finish_content_artifact(
        &self,
        artifact: &crate::storage::receive_artifact::ReceiveArtifact,
    ) -> Result<(), StorageError> {
        let mut conn = self.db.connection()?;
        let tx = conn.transaction()?;
        crate::storage::receive_artifact::mark_content_applied(&tx, artifact)?;
        tx.commit()?;
        Ok(())
    }

    /// A replay batch is a snapshot: live application can retire a row before
    /// replay reaches it. Only its exact pending binding is safe to apply.
    pub(crate) fn content_artifact_still_pending(
        &self,
        artifact: &crate::storage::receive_artifact::ReceiveArtifact,
    ) -> Result<bool, StorageError> {
        let conn = self.db.read_connection()?;
        crate::storage::receive_artifact::require_current_binding(&conn, &artifact.key)?;
        match crate::storage::receive_artifact::load_by_id(&conn, &artifact.id)? {
            None => Ok(false),
            Some(current) if current.key != artifact.key || current.digest != artifact.digest => {
                Err(StorageError::InvalidData("receive artifact changed".into()))
            }
            Some(current) => Ok(!current.applied && !current.acknowledged),
        }
    }

    pub(crate) fn has_earlier_pending_receive_artifact(
        &self,
        artifact: &crate::storage::receive_artifact::ReceiveArtifact,
    ) -> Result<bool, StorageError> {
        let conn = self.db.read_connection()?;
        crate::storage::receive_artifact::require_current_binding(&conn, &artifact.key)?;
        let count: i64 = conn.query_row(
            "SELECT count(*) FROM reaction_receive_artifacts WHERE local_account=?1 AND local_device=?2 AND transport=?3 AND scope=?4 AND kind!='reaction' AND state='pending' AND (server_timestamp_ns,id)<(?5,?6)",
            params![artifact.key.local_account, artifact.key.local_device, artifact.key.transport,
                artifact.key.scope, artifact.key.server_timestamp_ns as i64, artifact.id],
            |row| row.get(0),
        )?;
        Ok(count != 0)
    }

    pub(crate) fn pending_reaction_artifacts_after(
        &self,
        cursor: Option<&(u64, String)>,
    ) -> Result<Vec<crate::storage::receive_artifact::ReceiveArtifact>, StorageError> {
        use crate::storage::receive_artifact;
        let conn = self.db.read_connection()?;
        let (account, device) = reaction_receive_binding(&conn)?;
        let mut stmt=conn.prepare("SELECT id FROM reaction_receive_artifacts WHERE local_account=?1 AND local_device=?2 AND kind='reaction' AND state='pending' AND (?3 IS NULL OR (server_timestamp_ns,id)>(?3,?4)) ORDER BY server_timestamp_ns,id LIMIT ?5")?;
        let ids = stmt
            .query_map(
                params![
                    account,
                    device,
                    cursor.map(|v| v.0 as i64),
                    cursor.map(|v| v.1.as_str()),
                    receive_artifact::MAX_RECOVERY_BATCH as i64
                ],
                |r| r.get::<_, String>(0),
            )?
            .collect::<Result<Vec<_>, _>>()?;
        let mut result = Vec::new();
        for id in ids {
            result.push(receive_artifact::load_by_id(&conn, &id)?.ok_or_else(|| {
                StorageError::InvalidData("Receive artifact vanished during recovery read".into())
            })?);
        }
        Ok(result)
    }

    pub(crate) fn pending_content_artifacts_after(
        &self,
        cursor: Option<&(u64, String)>,
    ) -> Result<Vec<crate::storage::receive_artifact::ReceiveArtifact>, StorageError> {
        use crate::storage::receive_artifact;
        let conn = self.db.read_connection()?;
        let (account, device) = reaction_receive_binding(&conn)?;
        let mut stmt = conn.prepare("SELECT id FROM reaction_receive_artifacts WHERE local_account=?1 AND local_device=?2 AND kind!='reaction' AND state='pending' AND (?3 IS NULL OR (server_timestamp_ns,id)>(?3,?4)) ORDER BY server_timestamp_ns,id LIMIT ?5")?;
        let ids = stmt
            .query_map(
                params![
                    account,
                    device,
                    cursor.map(|v| v.0 as i64),
                    cursor.map(|v| v.1.as_str()),
                    receive_artifact::MAX_RECOVERY_BATCH as i64
                ],
                |r| r.get::<_, String>(0),
            )?
            .collect::<Result<Vec<_>, _>>()?;
        let mut result = Vec::new();
        for id in ids {
            result.push(receive_artifact::load_by_id(&conn, &id)?.ok_or_else(|| {
                StorageError::InvalidData("Receive artifact vanished during recovery read".into())
            })?);
        }
        Ok(result)
    }

    /// Bounded restart-safe replay for handoffs whose target is already stored.
    pub fn replay_held_reactions(&self) -> Result<Vec<(ReactionScope, UserId, u64)>, StorageError> {
        let targets = {
            let conn = self.db.read_connection()?;
            let own: Option<String> = conn
                .query_row(
                    "SELECT value FROM settings WHERE key='auth.current_user_id'",
                    [],
                    |r| r.get(0),
                )
                .optional()?;
            let own = own
                .map(|v| {
                    serde_json::from_str::<String>(&v).map_err(|_| {
                        StorageError::InvalidData("Invalid reaction replay account".into())
                    })
                })
                .transpose()?;
            // Exact eligibility excludes readiness heads before the work limit.
            // Such rows remain durable and qualify after metadata arrives.
            let mut stmt=conn.prepare("SELECT DISTINCT p.conversation_id,p.conversation_type,p.author_id,p.target_sent_at
                FROM pending_scoped_reactions p JOIN messages m ON m.conversation_id=p.conversation_id AND m.conversation_type=p.conversation_type AND m.sender_id=p.author_id AND m.sent_at=p.target_sent_at
                WHERE m.is_deleted=0
                AND NOT EXISTS(SELECT 1 FROM friendships f WHERE f.user_id=?1 AND f.friend_id=p.actor_id AND f.status=2)
                AND (p.actor_id=?1
                    OR (p.conversation_type='server_dm' AND p.conversation_id=p.actor_id)
                    OR (p.conversation_type='channel' AND EXISTS(SELECT 1 FROM memberships v WHERE v.channel_id=p.conversation_id AND v.user_id=p.actor_id))
                    OR (p.conversation_type='anon_dm' AND EXISTS(SELECT 1 FROM anon_dm_conversations d WHERE d.id=p.conversation_id AND d.peer_id=p.actor_id AND d.status=1 AND length(d.peer_mailbox_pk)=32) AND NOT EXISTS(SELECT 1 FROM anon_dm_blocked_users b WHERE b.user_id=p.actor_id))
                    OR (p.conversation_type='anon_group' AND EXISTS(SELECT 1 FROM anon_group_members g JOIN anon_groups a ON a.id=g.group_id WHERE g.group_id=p.conversation_id AND g.user_id=p.actor_id AND g.is_active=1 AND a.status<>2) AND NOT EXISTS(SELECT 1 FROM anon_group_blocked_users b WHERE b.group_id=p.conversation_id AND b.user_id=p.actor_id)))
                ORDER BY p.conversation_type,p.conversation_id,p.author_id,p.target_sent_at LIMIT 1000")?;
            stmt.query_map(params![own], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?
        };
        let mut applied = Vec::new();
        for (id, kind, author, ts) in targets {
            let scope = ReactionScope::new(
                id.parse().map_err(|_| {
                    StorageError::InvalidData("Invalid handoff conversation".into())
                })?,
                ConversationType::from_str_value(&kind)?,
            );
            let author = author
                .parse()
                .map_err(|_| StorageError::InvalidData("Invalid handoff author".into()))?;
            match self.apply_held_reaction_scope(&scope, &author, ts as u64) {
                Ok(true) => applied.push((scope, author, ts as u64)),
                Ok(false) => {}
                Err(_) => {
                    tracing::warn!("Independent pending reaction target awaits storage recovery")
                }
            }
        }
        Ok(applied)
    }

    pub fn get_by_reaction_scope(
        &self,
        scope: &ReactionScope,
        author: &UserId,
        sent_at: u64,
    ) -> Result<Option<Message>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(&format!("SELECT {} FROM messages WHERE conversation_id=?1 AND conversation_type=?2 AND sender_id=?3 AND sent_at=?4 AND is_deleted=0", message_columns("messages")))?;
        let row = stmt
            .query_row(
                params![
                    scope.id.to_string(),
                    scope.kind.as_str(),
                    author.to_string(),
                    sent_at as i64
                ],
                row_to_message,
            )
            .optional()?;
        row.transpose()
    }

    /// Replace a verified complete server snapshot and both projections atomically.
    /// A nonempty clockless snapshot cannot create ordered actor evidence: each
    /// desired vote must agree with retained add/selection authority.
    pub fn replace_reaction_snapshot(
        &self,
        scope: &ReactionScope,
        author: &UserId,
        sent_at: u64,
        current: &UserId,
        desired: &[(UserId, String)],
    ) -> Result<(), StorageError> {
        let mut conn = self.db.connection()?;
        let tx = conn.transaction()?;
        let exists:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM messages WHERE conversation_id=?1 AND conversation_type=?2 AND sender_id=?3 AND sent_at=?4 AND is_deleted=0)",params![scope.id.to_string(),scope.kind.as_str(),author.to_string(),sent_at as i64],|r|r.get(0))?;
        if !exists {
            return Err(StorageError::InvalidData(
                "Reaction snapshot target is no longer stored".into(),
            ));
        }
        let mut actors = std::collections::HashSet::new();
        for (actor, emoji) in desired {
            if !actors.insert(actor) {
                return Err(StorageError::InvalidData(
                    "Reaction snapshot contains several votes for one actor".into(),
                ));
            }
            let ordered: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM reaction_actor_selections s JOIN reaction_action_versions v ON v.conversation_id=s.conversation_id AND v.conversation_type=s.conversation_type AND v.author_id=s.author_id AND v.target_sent_at=s.target_sent_at AND v.actor_id=s.actor_id AND v.emoji=s.emoji WHERE s.conversation_id=?1 AND s.conversation_type=?2 AND s.author_id=?3 AND s.target_sent_at=?4 AND s.actor_id=?5 AND s.emoji=?6 AND v.remove=0 AND v.action_ts=s.action_ts)",params![scope.id.to_string(),scope.kind.as_str(),author.to_string(),sent_at as i64,actor.to_string(),emoji],|r|r.get(0))?;
            if !ordered {
                return Err(StorageError::InvalidData(
                    "Reaction snapshot lacks compatible ordered actor evidence".into(),
                ));
            }
        }
        tx.execute("DELETE FROM unified_reactions WHERE conversation_id=?1 AND conversation_type=?2 AND sender_id=?3 AND sent_at_ns=?4",params![scope.id.to_string(),scope.kind.as_str(),author.to_string(),sent_at as i64])?;
        for (user, emoji) in desired {
            tx.execute("INSERT OR IGNORE INTO unified_reactions(id,conversation_id,conversation_type,sender_id,sent_at_ns,user_id,emoji,created_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",params![ReactionId::new().to_string(),scope.id.to_string(),scope.kind.as_str(),author.to_string(),sent_at as i64,user.to_string(),emoji,Utc::now().to_rfc3339()])?;
        }
        self.rebuild_reaction_blobs_with_conn(&tx, scope, author, sent_at, current)?;
        tx.commit()?;
        Ok(())
    }

    /// Add a reaction using Signal-style (sender_id, sent_at_ns) message identification.
    /// Inserts into `unified_reactions` and rebuilds the per-user blob cache on
    /// the message row using `current_user_id` (the locally-logged-in user) so
    /// the blob remains a view of *this device's* user, regardless of who the
    /// reactor was.
    ///
    /// Returns `true` if a new reaction was added.
    pub fn add_reaction(
        &self,
        scope: &ReactionScope,
        sender_id: &UserId,
        sent_at_ns: u64,
        reactor_id: &UserId,
        current_user_id: &UserId,
        emoji: &str,
    ) -> Result<bool, StorageError> {
        self.add_reaction_at(
            scope,
            sender_id,
            sent_at_ns,
            reactor_id,
            current_user_id,
            emoji,
            self.allocate_reaction_clock(reactor_id)?,
        )
    }

    pub fn add_reaction_at(
        &self,
        scope: &ReactionScope,
        sender_id: &UserId,
        sent_at_ns: u64,
        reactor_id: &UserId,
        current_user_id: &UserId,
        emoji: &str,
        action_ts: u64,
    ) -> Result<bool, StorageError> {
        self.add_reaction_outcome_at(
            scope,
            sender_id,
            sent_at_ns,
            reactor_id,
            current_user_id,
            emoji,
            action_ts,
        )
        .map(|outcome| outcome.changed)
    }

    pub(crate) fn add_reaction_outcome_at(
        &self,
        scope: &ReactionScope,
        sender_id: &UserId,
        sent_at_ns: u64,
        reactor_id: &UserId,
        current_user_id: &UserId,
        emoji: &str,
        action_ts: u64,
    ) -> Result<ReactionWriteOutcome, StorageError> {
        self.add_reaction_outcome_at_admitted(scope, sender_id, sent_at_ns, reactor_id, current_user_id, emoji, action_ts, None)
    }

    pub(crate) fn add_reaction_outcome_at_admitted(
        &self,
        scope: &ReactionScope,
        sender_id: &UserId,
        sent_at_ns: u64,
        reactor_id: &UserId,
        current_user_id: &UserId,
        emoji: &str,
        action_ts: u64,
        session: Option<&crate::state::receive_session::ReceiveSession>,
    ) -> Result<ReactionWriteOutcome, StorageError> {
        let mut conn = self.db.connection()?;
        let tx = conn.transaction()?;
        if let Some(session) = session { session.check_owner(&tx)?; }
        let conn = &tx;
        let exists: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM messages WHERE conversation_id=?1 AND conversation_type=?2 AND sender_id=?3 AND sent_at=?4 AND is_deleted=0)",params![scope.id.to_string(),scope.kind.as_str(),sender_id.to_string(),sent_at_ns as i64],|row|row.get(0))?;
        if !exists {
            return Err(StorageError::InvalidData(
                "Reaction target no longer stored in this scope".into(),
            ));
        }
        if !accept_reaction_action(
            conn, scope, sender_id, sent_at_ns, reactor_id, emoji, action_ts, false,
        )? {
            self.rebuild_reaction_blobs_with_conn(
                conn,
                scope,
                sender_id,
                sent_at_ns,
                current_user_id,
            )?;
            let completed = reaction_action_completed(
                conn, scope, sender_id, sent_at_ns, reactor_id, emoji, action_ts, false,
            )?;
            if let Some(session) = session { session.check()?; }
            tx.commit()?;
            return Ok(ReactionWriteOutcome {
                changed: false,
                completed,
            });
        }
        let reaction_id = ReactionId::new();
        let now = Utc::now().to_rfc3339();

        let inserted = conn.execute(
            "INSERT OR IGNORE INTO unified_reactions (id, sender_id, sent_at_ns, user_id, emoji, created_at, conversation_id, conversation_type)
             SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8 WHERE EXISTS (SELECT 1 FROM messages WHERE conversation_id=?7 AND conversation_type=?8 AND sender_id=?2 AND sent_at=?3 AND is_deleted=0)",
            params![
                reaction_id.to_string(),
                sender_id.to_string(),
                sent_at_ns as i64,
                reactor_id.to_string(),
                emoji,
                now,
                scope.id.to_string(), scope.kind.as_str(),
            ],
        )?;

        self.rebuild_reaction_blobs_with_conn(conn, scope, sender_id, sent_at_ns, current_user_id)?;
        let completed = reaction_action_completed(
            conn, scope, sender_id, sent_at_ns, reactor_id, emoji, action_ts, false,
        )?;
        if let Some(session) = session { session.check()?; }
        tx.commit()?;
        Ok(ReactionWriteOutcome {
            changed: inserted > 0,
            completed,
        })
    }

    /// Remove a reaction by (sender_id, sent_at_ns, reactor_id, emoji).
    /// Rebuilds the per-user blob for `current_user_id` (see `add_reaction`).
    ///
    /// Returns `true` if a reaction was actually removed.
    pub fn remove_reaction(
        &self,
        scope: &ReactionScope,
        sender_id: &UserId,
        sent_at_ns: u64,
        reactor_id: &UserId,
        current_user_id: &UserId,
        emoji: &str,
    ) -> Result<bool, StorageError> {
        self.remove_reaction_at(
            scope,
            sender_id,
            sent_at_ns,
            reactor_id,
            current_user_id,
            emoji,
            self.allocate_reaction_clock(reactor_id)?,
        )
    }

    pub fn remove_reaction_at(
        &self,
        scope: &ReactionScope,
        sender_id: &UserId,
        sent_at_ns: u64,
        reactor_id: &UserId,
        current_user_id: &UserId,
        emoji: &str,
        action_ts: u64,
    ) -> Result<bool, StorageError> {
        self.remove_reaction_outcome_at(
            scope,
            sender_id,
            sent_at_ns,
            reactor_id,
            current_user_id,
            emoji,
            action_ts,
        )
        .map(|outcome| outcome.changed)
    }

    pub(crate) fn remove_reaction_outcome_at(
        &self,
        scope: &ReactionScope,
        sender_id: &UserId,
        sent_at_ns: u64,
        reactor_id: &UserId,
        current_user_id: &UserId,
        emoji: &str,
        action_ts: u64,
    ) -> Result<ReactionWriteOutcome, StorageError> {
        self.remove_reaction_outcome_at_admitted(scope, sender_id, sent_at_ns, reactor_id, current_user_id, emoji, action_ts, None)
    }

    pub(crate) fn remove_reaction_outcome_at_admitted(
        &self,
        scope: &ReactionScope,
        sender_id: &UserId,
        sent_at_ns: u64,
        reactor_id: &UserId,
        current_user_id: &UserId,
        emoji: &str,
        action_ts: u64,
        session: Option<&crate::state::receive_session::ReceiveSession>,
    ) -> Result<ReactionWriteOutcome, StorageError> {
        let mut conn = self.db.connection()?;
        let tx = conn.transaction()?;
        if let Some(session) = session { session.check_owner(&tx)?; }
        let conn = &tx;
        let exists: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM messages WHERE conversation_id=?1 AND conversation_type=?2 AND sender_id=?3 AND sent_at=?4 AND is_deleted=0)",params![scope.id.to_string(),scope.kind.as_str(),sender_id.to_string(),sent_at_ns as i64],|row|row.get(0))?;
        if !exists {
            return Err(StorageError::InvalidData(
                "Reaction target no longer stored in this scope".into(),
            ));
        }
        if !accept_reaction_action(
            conn, scope, sender_id, sent_at_ns, reactor_id, emoji, action_ts, true,
        )? {
            self.rebuild_reaction_blobs_with_conn(
                conn,
                scope,
                sender_id,
                sent_at_ns,
                current_user_id,
            )?;
            let completed = reaction_action_completed(
                conn, scope, sender_id, sent_at_ns, reactor_id, emoji, action_ts, true,
            )?;
            if let Some(session) = session { session.check()?; }
            tx.commit()?;
            return Ok(ReactionWriteOutcome {
                changed: false,
                completed,
            });
        }
        let deleted = conn.execute(
            "DELETE FROM unified_reactions
             WHERE sender_id = ?1 AND sent_at_ns = ?2 AND user_id = ?3 AND emoji = ?4 AND conversation_id=?5 AND conversation_type=?6",
            params![
                sender_id.to_string(),
                sent_at_ns as i64,
                reactor_id.to_string(),
                emoji, scope.id.to_string(), scope.kind.as_str()
            ],
        )?;

        self.rebuild_reaction_blobs_with_conn(conn, scope, sender_id, sent_at_ns, current_user_id)?;
        let completed = reaction_action_completed(
            conn, scope, sender_id, sent_at_ns, reactor_id, emoji, action_ts, true,
        )?;
        if let Some(session) = session { session.check()?; }
        tx.commit()?;
        Ok(ReactionWriteOutcome {
            changed: deleted > 0,
            completed,
        })
    }

    /// Check whether the current user has reacted with a given emoji on a message.
    ///
    /// Reads the per-user cache on the message row (`current_user_reactions_blob`).
    /// The blob is maintained to reflect the locally-logged-in user's reactions
    /// across all conversation types (anon DM/group rebuild the blob for the
    /// logged-in user; regular DM/channel receive it from the server via proto).
    pub fn user_has_reacted(
        &self,
        scope: &ReactionScope,
        sender_id: &UserId,
        sent_at_ns: u64,
        emoji: &str,
    ) -> Result<bool, StorageError> {
        let conn = self.db.read_connection()?;
        let blob: Option<Vec<u8>> = conn
            .query_row(
                "SELECT current_user_reactions_blob FROM messages
                 WHERE sender_id = ?1 AND sent_at = ?2 AND conversation_id=?3 AND conversation_type=?4",
                params![sender_id.to_string(), sent_at_ns as i64, scope.id.to_string(), scope.kind.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        let reactions: Vec<String> = blob
            .map(|b| db_deserialize(&b))
            .transpose()?
            .unwrap_or_default();
        Ok(reactions.iter().any(|e| e == emoji))
    }

    /// Get all reactions on a message identified by (sender_id, sent_at_ns).
    pub fn get_reactions(
        &self,
        scope: &ReactionScope,
        sender_id: &UserId,
        sent_at_ns: u64,
    ) -> Result<Vec<Reaction>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, sender_id, sent_at_ns, user_id, emoji, created_at
             FROM unified_reactions WHERE sender_id = ?1 AND sent_at_ns = ?2 AND conversation_id=?3 AND conversation_type=?4
             ORDER BY created_at ASC",
        )?;
        let mut rows = stmt.query(params![
            sender_id.to_string(),
            sent_at_ns as i64,
            scope.id.to_string(),
            scope.kind.as_str()
        ])?;
        let mut reactions = Vec::new();
        while let Some(row) = rows.next()? {
            reactions.push(row_to_reaction(row)?);
        }
        Ok(reactions)
    }

    /// Get reaction counts per emoji for a message (from unified_reactions, not blob).
    pub fn get_reaction_counts(
        &self,
        scope: &ReactionScope,
        sender_id: &UserId,
        sent_at_ns: u64,
    ) -> Result<Vec<ReactionCount>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT emoji, COUNT(*) FROM unified_reactions
             WHERE sender_id = ?1 AND sent_at_ns = ?2 AND conversation_id=?3 AND conversation_type=?4 GROUP BY emoji ORDER BY COUNT(*) DESC",
        )?;
        let mut rows = stmt.query(params![
            sender_id.to_string(),
            sent_at_ns as i64,
            scope.id.to_string(),
            scope.kind.as_str()
        ])?;
        let mut counts = Vec::new();
        while let Some(row) = rows.next()? {
            let emoji_str: String = row.get(0)?;
            let count: u32 = row.get(1)?;
            counts.push(ReactionCount::new(emoji_str, count));
        }
        Ok(counts)
    }

    /// Get reaction counts for all messages in a conversation.
    ///
    /// Returns a map of `sent_at` -> `Vec<(emoji, count)>` for messages that have reactions.
    pub fn get_reaction_counts_for_conversation(
        &self,
        conversation_id: &ChannelId,
        conversation_type: &ConversationType,
    ) -> Result<std::collections::HashMap<u64, Vec<(String, u32)>>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT r.sent_at_ns, r.emoji, COUNT(*) as cnt
             FROM unified_reactions r
             INNER JOIN messages m ON r.sender_id = m.sender_id AND r.sent_at_ns = m.sent_at AND r.conversation_id=m.conversation_id AND r.conversation_type=m.conversation_type
             WHERE m.conversation_id = ?1 AND m.conversation_type = ?2 AND m.is_deleted = 0
             GROUP BY r.sent_at_ns, r.emoji
             ORDER BY r.sent_at_ns, cnt DESC",
        )?;
        let mut rows = stmt.query(params![
            conversation_id.to_string(),
            conversation_type.as_str()
        ])?;
        let mut map: std::collections::HashMap<u64, Vec<(String, u32)>> =
            std::collections::HashMap::new();
        while let Some(row) = rows.next()? {
            let sent_at: i64 = row.get(0)?;
            let emoji: String = row.get(1)?;
            let count: u32 = row.get(2)?;
            map.entry(sent_at as u64).or_default().push((emoji, count));
        }
        Ok(map)
    }

}

pub(super) fn accept_reaction_action(
    conn: &rusqlite::Connection,
    scope: &ReactionScope,
    author: &UserId,
    target: u64,
    actor: &UserId,
    emoji: &str,
    action: u64,
    remove: bool,
) -> Result<bool, StorageError> {
    if action == 0 || action > i64::MAX as u64 {
        return Err(StorageError::InvalidData(
            "Invalid reaction action clock".into(),
        ));
    }
    // Preserve per-emoji evidence even when a newer cross-emoji add owns the
    // projection. At equal clocks a removal wins, independent of delivery order.
    let accepted = conn.execute("INSERT INTO reaction_action_versions(conversation_id,conversation_type,author_id,target_sent_at,actor_id,emoji,action_ts,remove) VALUES(?1,?2,?3,?4,?5,?6,?7,?8) ON CONFLICT(conversation_type,conversation_id,author_id,target_sent_at,actor_id,emoji) DO UPDATE SET action_ts=excluded.action_ts,remove=excluded.remove WHERE excluded.action_ts>reaction_action_versions.action_ts OR (excluded.action_ts=reaction_action_versions.action_ts AND excluded.remove>reaction_action_versions.remove)",params![scope.id.to_string(),scope.kind.as_str(),author.to_string(),target as i64,actor.to_string(),emoji,action as i64,remove])?>0;
    if !remove {
        // Record the latest observed add even if its same-emoji removal arrived
        // first. This retains the actor's selection through removal/replay and
        // prevents the previous emoji reappearing in the opposite delivery order.
        let selected = conn.execute("INSERT INTO reaction_actor_selections(conversation_id,conversation_type,author_id,target_sent_at,actor_id,emoji,action_ts) VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(conversation_type,conversation_id,author_id,target_sent_at,actor_id) DO UPDATE SET emoji=excluded.emoji,action_ts=excluded.action_ts WHERE excluded.action_ts>reaction_actor_selections.action_ts OR (excluded.action_ts=reaction_actor_selections.action_ts AND excluded.emoji>reaction_actor_selections.emoji)", params![scope.id.to_string(),scope.kind.as_str(),author.to_string(),target as i64,actor.to_string(),emoji,action as i64])?>0;
        if selected {
            conn.execute("DELETE FROM unified_reactions WHERE conversation_id=?1 AND conversation_type=?2 AND sender_id=?3 AND sent_at_ns=?4 AND user_id=?5 AND emoji<>?6", params![scope.id.to_string(),scope.kind.as_str(),author.to_string(),target as i64,actor.to_string(),emoji])?;
        }
        return Ok(accepted && selected);
    }
    Ok(accepted)
}

fn reaction_receive_binding(conn: &rusqlite::Connection) -> Result<(String, u32), StorageError> {
    use super::super::settings_repo::keys;
    let account: String = conn.query_row(
        "SELECT value FROM settings WHERE key=?1",
        [keys::CURRENT_USER_ID],
        |r| r.get(0),
    )?;
    let device: String = conn.query_row(
        "SELECT value FROM settings WHERE key=?1",
        [keys::CURRENT_DEVICE_ID],
        |r| r.get(0),
    )?;
    Ok((
        serde_json::from_str(&account)
            .map_err(|_| StorageError::InvalidData("Invalid receive account".into()))?,
        serde_json::from_str(&device)
            .map_err(|_| StorageError::InvalidData("Invalid receive device".into()))?,
    ))
}

/// Exact retained evidence, read in the writer transaction: a replay is complete,
/// but a newer clock or a different winning selection cannot complete this action.
fn reaction_action_completed(
    conn: &rusqlite::Connection,
    scope: &ReactionScope,
    author: &UserId,
    target: u64,
    actor: &UserId,
    emoji: &str,
    action: u64,
    remove: bool,
) -> Result<bool, StorageError> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM reaction_action_versions v
         WHERE v.conversation_id=?1 AND v.conversation_type=?2 AND v.author_id=?3
         AND v.target_sent_at=?4 AND v.actor_id=?5 AND v.emoji=?6
         AND v.action_ts=?7 AND v.remove=?8)
         AND CASE WHEN ?8 THEN NOT EXISTS(SELECT 1 FROM reaction_actor_selections s
             WHERE s.conversation_id=?1 AND s.conversation_type=?2 AND s.author_id=?3
             AND s.target_sent_at=?4 AND s.actor_id=?5 AND s.action_ts>?7)
         ELSE EXISTS(SELECT 1 FROM reaction_actor_selections s
             WHERE s.conversation_id=?1 AND s.conversation_type=?2 AND s.author_id=?3
             AND s.target_sent_at=?4 AND s.actor_id=?5 AND s.emoji=?6 AND s.action_ts=?7) END",
        params![
            scope.id.to_string(),
            scope.kind.as_str(),
            author.to_string(),
            target as i64,
            actor.to_string(),
            emoji,
            action as i64,
            remove
        ],
        |row| row.get(0),
    )?)
}

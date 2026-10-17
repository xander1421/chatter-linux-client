//! Awaited application and bounded replay for secret posts and controls.
use std::sync::Arc;

use crate::domain::{ChannelId, UserId};
use crate::events::EventBus;
use crate::inbox::handlers::DomainEvent;
use crate::services::anon_dm::AnonDmService;
use crate::services::anon_group::AnonGroupService;
use crate::storage::receive_artifact::{ReceiveArtifact, ReceiveKind};
use crate::storage::{Database, MessageRepository, SettingsRepository, UserRepository};

#[derive(Clone)]
pub struct ContentHandoff {
    pub dm: Arc<AnonDmService>,
    pub group: Arc<AnonGroupService>,
    pub events: EventBus,
    pub database: Arc<Database>,
}

impl ContentHandoff {
    fn retire_dropped(
        repo: &Arc<MessageRepository>,
        artifact: &ReceiveArtifact,
        reason: &'static str,
        finish: bool,
    ) -> Result<Option<(crate::domain::anon_dm::AnonDmId, u64)>, String> {
        tracing::warn!(envelope_id = %artifact.key.envelope_id, reason, "Secret data post rejected");
        if finish {
            repo.finish_content_artifact(artifact)
                .map_err(|error| error.to_string())?;
            tracing::info!(envelope_id = %artifact.key.envelope_id, kind = artifact.kind.as_str(), "secret receive artifact applied");
        }
        Ok(None)
    }

    pub(crate) async fn apply(
        &self,
        repo: &Arc<MessageRepository>,
        artifact: &ReceiveArtifact,
    ) -> Result<(), String> {
        // Posts can resolve an earlier pending targeted delete; roster and
        // JoinAck can resolve an earlier post from a newly admitted sender.
        // Their own handlers use stable IDs or ordered membership versions.
        if !matches!(
            artifact.kind,
            ReceiveKind::Content | ReceiveKind::Roster | ReceiveKind::JoinAck
        ) && repo
            .has_earlier_pending_receive_artifact(artifact)
            .map_err(|e| e.to_string())?
        {
            return Err("earlier secret receive artifact is pending".into());
        }
        if artifact.kind == ReceiveKind::Roster {
            use crate::proto::content_v1::content::Content as Body;
            let content = crate::network::validation::protovalidate::decode_inbound_content(
                &artifact.content,
                "secret_roster_journal",
            )
            .map_err(|e| e.to_string())?;
            let roster = match content.content.as_ref() {
                Some(Body::AnonGroupMemberList(list)) => Some(list),
                Some(Body::AnonGroupMessage(wrapper)) => match wrapper
                    .inner_content
                    .as_deref()
                    .and_then(|c| c.content.as_ref())
                {
                    Some(Body::AnonGroupMemberList(list)) if list.group_id == wrapper.group_id => {
                        Some(list)
                    }
                    _ => None,
                },
                _ => None,
            };
            let this = self.clone();
            let apply_repo = repo.clone();
            let apply_artifact = artifact.clone();
            tokio::task::spawn_blocking(move || {
                this.apply_blocking(&apply_repo, &apply_artifact, false)
            })
            .await
            .map_err(|e| e.to_string())??;
            if let Some(list) = roster {
                let id = crate::domain::anon_group::AnonGroupId::try_from_slice(&list.group_id)
                    .ok_or("invalid roster group")?;
                let (_, failed) = self
                    .group
                    .refresh_roster_avatars(
                        &id,
                        &list.members,
                        &crate::services::secret_avatar::RETRY_DELAYS,
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                if !failed.is_empty() {
                    return Err("roster avatar fetch awaits replay".into());
                }
            }
            repo.finish_content_artifact(artifact)
                .map_err(|e| e.to_string())?;
            tracing::info!(envelope_id = %artifact.key.envelope_id, kind = artifact.kind.as_str(), "secret receive artifact applied");
            // A joiner's posts are held until the owner roster admits it;
            // this journal is the live roster path, so release them here.
            if let Some(list) = roster {
                if let Some(id) =
                    crate::domain::anon_group::AnonGroupId::try_from_slice(&list.group_id)
                {
                    let group = self.group.clone();
                    let events = self.events.clone();
                    let members = list.members.clone();
                    tokio::spawn(async move {
                        if let Err(error) = group.retry_held_posts_for_group(&id).await {
                            tracing::warn!(%error, "Held group post retry after roster failed");
                        }
                        // Re-read bubble status after fan-out settles.
                        events.emit_domain(DomainEvent::AnonGroupMemberListReceived {
                            group_id: id,
                            members,
                        });
                    });
                }
            }
            return Ok(());
        }
        if matches!(artifact.kind, ReceiveKind::JoinAck | ReceiveKind::Profile) {
            let content = crate::network::validation::protovalidate::decode_inbound_content(
                &artifact.content,
                "secret_control_journal",
            )
            .map_err(|e| e.to_string())?;
            use crate::proto::content_v1::content::Content as Body;
            match content.content.as_ref() {
                Some(Body::AnonGroupJoinAck(ack)) => {
                    let id = crate::domain::anon_group::AnonGroupId::try_from_slice(&ack.group_id)
                        .ok_or("invalid JoinAck group")?;
                    if artifact.key.transport != "anon_group"
                        || ChannelId::from(id).to_string() != artifact.key.scope
                    {
                        return Err("JoinAck mailbox destination mismatch".into());
                    }
                    let sender: UserId = artifact.sender.parse().map_err(|_| "invalid sender")?;
                    let key: [u8; 32] = ack
                        .joiner_mailbox_pk
                        .as_slice()
                        .try_into()
                        .map_err(|_| "invalid JoinAck mailbox key")?;
                    let profile_key = ack.profile_key.as_deref().and_then(|v| v.try_into().ok());
                    let name = profile_key
                        .zip(ack.encrypted_display_name.as_deref())
                        .and_then(|(key, ciphertext)| {
                            crate::crypto::profile_crypto::decrypt_display_name(&key, ciphertext)
                                .ok()
                        })
                        .filter(|s| !s.is_empty());
                    let avatar = if let Some(pointer) = ack.avatar.as_ref() {
                        self.group
                            .fetch_member_avatar(
                                pointer,
                                None,
                                &crate::services::secret_avatar::RETRY_DELAYS,
                            )
                            .await
                            .map_err(|e| e.to_string())?
                    } else {
                        None
                    };
                    let joined = self
                        .group
                        .handle_join_ack_ordered(
                            id,
                            sender,
                            artifact.sender_device,
                            key,
                            profile_key,
                            name,
                            avatar,
                            ack.timestamp,
                        )
                        .await
                        .map_err(|e| e.to_string())?;
                    if joined {
                        self.events.emit_domain(DomainEvent::AnonGroupMemberJoined {
                            group_id: id,
                            user_id: sender,
                            profile_key,
                            display_name: None,
                            avatar: None,
                        });
                    }
                }
                Some(Body::AnonGroupProfileShare(profile)) => {
                    let id =
                        crate::domain::anon_group::AnonGroupId::try_from_slice(&profile.group_id)
                            .ok_or("invalid profile group")?;
                    if artifact.key.transport != "anon_group"
                        || ChannelId::from(id).to_string() != artifact.key.scope
                    {
                        return Err("profile mailbox destination mismatch".into());
                    }
                    let sender: UserId = artifact.sender.parse().map_err(|_| "invalid sender")?;
                    let blocked_group = self
                        .group
                        .anon_group_repo
                        .get_group(&id)
                        .map_err(|e| e.to_string())?
                        .is_some_and(|g| !g.status.can_receive());
                    if blocked_group
                        || self
                            .group
                            .anon_group_repo
                            .is_user_blocked(&id, &sender)
                            .map_err(|e| e.to_string())?
                    {
                        repo.finish_content_artifact(artifact)
                            .map_err(|e| e.to_string())?;
                        tracing::info!(envelope_id = %artifact.key.envelope_id, kind = artifact.kind.as_str(), "secret receive artifact applied");
                        return Ok(());
                    }
                    let key: Option<[u8; 32]> = profile
                        .profile_key
                        .as_deref()
                        .and_then(|v| v.try_into().ok());
                    let name = key
                        .zip(profile.encrypted_display_name.as_deref())
                        .and_then(|(key, ciphertext)| {
                            crate::crypto::profile_crypto::decrypt_display_name(&key, ciphertext)
                                .ok()
                        })
                        .filter(|s| !s.is_empty());
                    let current = self
                        .group
                        .anon_group_repo
                        .get_member(&id, &sender)
                        .map_err(|e| e.to_string())?
                        .and_then(|m| m.avatar);
                    let avatar = if let Some(pointer) = profile.avatar.as_ref() {
                        self.group
                            .fetch_member_avatar(
                                pointer,
                                current,
                                &crate::services::secret_avatar::RETRY_DELAYS,
                            )
                            .await
                            .map_err(|e| e.to_string())?
                    } else {
                        None
                    };
                    self.group
                        .handle_profile_share(id, sender, key, name, avatar)
                        .await
                        .map_err(|e| e.to_string())?;
                }
                Some(Body::AnonGroupMessage(wrapper))
                    if matches!(
                        wrapper
                            .inner_content
                            .as_deref()
                            .and_then(|c| c.content.as_ref()),
                        Some(Body::AnonGroupProfileShare(_))
                    ) =>
                {
                    let id =
                        crate::domain::anon_group::AnonGroupId::try_from_slice(&wrapper.group_id)
                            .ok_or("invalid profile group")?;
                    if artifact.key.transport != "anon_group"
                        || ChannelId::from(id).to_string() != artifact.key.scope
                    {
                        return Err("profile mailbox destination mismatch".into());
                    }
                    let Some(Body::AnonGroupProfileShare(profile)) = wrapper
                        .inner_content
                        .as_deref()
                        .and_then(|c| c.content.as_ref())
                    else {
                        unreachable!()
                    };
                    if profile.group_id != wrapper.group_id {
                        return Err("profile names another group".into());
                    }
                    let sender: UserId = artifact.sender.parse().map_err(|_| "invalid sender")?;
                    let blocked_group = self
                        .group
                        .anon_group_repo
                        .get_group(&id)
                        .map_err(|e| e.to_string())?
                        .is_some_and(|g| !g.status.can_receive());
                    if blocked_group
                        || self
                            .group
                            .anon_group_repo
                            .is_user_blocked(&id, &sender)
                            .map_err(|e| e.to_string())?
                    {
                        repo.finish_content_artifact(artifact)
                            .map_err(|e| e.to_string())?;
                        tracing::info!(envelope_id = %artifact.key.envelope_id, kind = artifact.kind.as_str(), "secret receive artifact applied");
                        return Ok(());
                    }
                    let key: Option<[u8; 32]> = profile
                        .profile_key
                        .as_deref()
                        .and_then(|v| v.try_into().ok());
                    let name = key
                        .zip(profile.encrypted_display_name.as_deref())
                        .and_then(|(key, ciphertext)| {
                            crate::crypto::profile_crypto::decrypt_display_name(&key, ciphertext)
                                .ok()
                        })
                        .filter(|s| !s.is_empty());
                    let current = self
                        .group
                        .anon_group_repo
                        .get_member(&id, &sender)
                        .map_err(|e| e.to_string())?
                        .and_then(|m| m.avatar);
                    let avatar = if let Some(pointer) = profile.avatar.as_ref() {
                        self.group
                            .fetch_member_avatar(
                                pointer,
                                current,
                                &crate::services::secret_avatar::RETRY_DELAYS,
                            )
                            .await
                            .map_err(|e| e.to_string())?
                    } else {
                        None
                    };
                    self.group
                        .handle_profile_share(id, sender, key, name, avatar)
                        .await
                        .map_err(|e| e.to_string())?;
                }
                _ => {
                    // Wrapped DM profile updates use the ordinary, synchronous handler.
                    let this = self.clone();
                    let apply_repo = repo.clone();
                    let apply_artifact = artifact.clone();
                    tokio::task::spawn_blocking(move || {
                        this.apply_blocking(&apply_repo, &apply_artifact, false)
                    })
                    .await
                    .map_err(|e| e.to_string())??;
                    if let Some(Body::AnonDmMessage(wrapper)) = content.content.as_ref()
                        && let Some(Body::AnonDmProfileUpdate(update)) = wrapper
                            .inner_content
                            .as_deref()
                            .and_then(|c| c.content.as_ref())
                        && let Some(pointer) = update.avatar.as_ref()
                    {
                        let id = crate::domain::anon_dm::AnonDmId::try_from_slice(
                            &wrapper.conversation_id,
                        )
                        .ok_or("invalid DM profile destination")?;
                        let sender: UserId =
                            artifact.sender.parse().map_err(|_| "invalid sender")?;
                        if let Some(avatar) = self
                            .dm
                            .refresh_peer_avatar(
                                &id,
                                &sender,
                                pointer,
                                &crate::services::secret_avatar::RETRY_DELAYS,
                            )
                            .await
                            .map_err(|e| e.to_string())?
                        {
                            self.events.emit_domain(DomainEvent::AnonDmProfileUpdated {
                                conversation_id: id,
                                user_id: sender,
                                display_name: None,
                                avatar: Some(avatar),
                                avatar_url: None,
                                bio: None,
                                timestamp: update.timestamp,
                            });
                        }
                    }
                    repo.finish_content_artifact(&artifact)
                        .map_err(|e| e.to_string())?;
                    tracing::info!(envelope_id = %artifact.key.envelope_id, kind = artifact.kind.as_str(), "secret receive artifact applied");
                    return Ok(());
                }
            }
            repo.finish_content_artifact(artifact)
                .map_err(|e| e.to_string())?;
            tracing::info!(envelope_id = %artifact.key.envelope_id, kind = artifact.kind.as_str(), "secret receive artifact applied");
            return Ok(());
        }
        let this = self.clone();
        let repo = repo.clone();
        let artifact = artifact.clone();
        let receipt =
            tokio::task::spawn_blocking(move || this.apply_blocking(&repo, &artifact, true))
                .await
                .map_err(|error| error.to_string())??;
        if let Some((id, timestamp)) = receipt {
            let dm = self.dm.clone();
            tokio::spawn(async move {
                if let Err(error) = dm.send_delivery_receipt(&id, vec![timestamp]).await {
                    tracing::warn!(%error, "Secret DM delivery receipt failed");
                }
            });
        }
        Ok(())
    }

    fn apply_blocking(
        &self,
        repo: &Arc<MessageRepository>,
        artifact: &ReceiveArtifact,
        finish: bool,
    ) -> Result<Option<(crate::domain::anon_dm::AnonDmId, u64)>, String> {
        use crate::proto::content_v1::content::Content as Body;
        if !artifact.kind.is_content() || artifact.applied {
            return Err("expected pending secret content artifact".into());
        }
        let sender: UserId = artifact.sender.parse().map_err(|_| "invalid sender")?;
        let users = UserRepository::new(self.database.clone());
        let settings = SettingsRepository::new(self.database.clone());
        let structural = artifact.key.transport == "anon_group"
            && matches!(
                artifact.kind,
                ReceiveKind::Roster | ReceiveKind::Kick | ReceiveKind::Leave
            );
        if !structural
            && crate::services::gateway::block_filter::sender_is_blocked(&users, &settings, &sender)
                .map_err(|error| error.to_string())?
        {
            return Self::retire_dropped(repo, artifact, "blocked sender", finish);
        }
        let scope: ChannelId = artifact.key.scope.parse().map_err(|_| "invalid scope")?;
        let content = crate::network::validation::protovalidate::decode_inbound_content(
            &artifact.content,
            "secret_data_journal",
        )
        .map_err(|error| error.to_string())?;
        let mut receipt = None;
        let events = match (artifact.key.transport.as_str(), content.content) {
            ("anon_dm", Some(Body::AnonDmMessage(wrapper))) => {
                let id = crate::domain::anon_dm::AnonDmId::try_from_slice(&wrapper.conversation_id)
                    .ok_or("invalid DM destination")?;
                if ChannelId::from(id) != scope {
                    return Self::retire_dropped(
                        repo,
                        artifact,
                        "DM destination differs from mailbox",
                        finish,
                    );
                }
                let dm = self
                    .dm
                    .anon_dm_repo()
                    .get_conversation(&id)
                    .map_err(|error| error.to_string())?
                    .ok_or("DM conversation unavailable")?;
                if dm.peer_id != sender || dm.peer_device_id != Some(artifact.sender_device) {
                    return Self::retire_dropped(
                        repo,
                        artifact,
                        "DM sender is not bound peer",
                        finish,
                    );
                }
                if !dm.is_ready() {
                    return Err("DM acceptance pending".into());
                }
                if self
                    .dm
                    .anon_dm_repo()
                    .is_user_blocked(&sender)
                    .map_err(|error| error.to_string())?
                {
                    Vec::new()
                } else {
                    let inner = wrapper.inner_content.ok_or("missing DM post")?;
                    let events = self
                        .dm
                        .process_incoming_content(&id, &sender, &inner)
                        .map_err(|error| error.to_string())?;
                    if events
                        .iter()
                        .any(|event| matches!(event, DomainEvent::MessageReceived { .. }))
                    {
                        receipt = Some((id, inner.timestamp));
                    }
                    events
                }
            }
            ("anon_group", Some(Body::AnonGroupMessage(wrapper))) => {
                let id = crate::domain::anon_group::AnonGroupId::try_from_slice(&wrapper.group_id)
                    .ok_or("invalid group destination")?;
                if ChannelId::from(id) != scope {
                    return Self::retire_dropped(
                        repo,
                        artifact,
                        "group destination differs from mailbox",
                        finish,
                    );
                }
                let inner = wrapper.inner_content.ok_or("missing group post")?;
                if artifact.kind == ReceiveKind::Delete {
                    if let Some(Body::DataMessage(data)) = inner.content.as_ref()
                        && let Some(delete) = data.delete.as_ref()
                        && delete
                            .target_author_aci
                            .as_deref()
                            .and_then(|v| v.parse::<UserId>().ok())
                            == Some(sender)
                    {
                        let target = repo
                            .get_by_sender_timestamp(&sender, delete.target_sent_timestamp)
                            .map_err(|e| e.to_string())?;
                        if !target.is_some_and(|m| m.conversation_id == scope) {
                            return Err("group delete target awaits its data post".into());
                        }
                    }
                }
                if !matches!(
                    artifact.kind,
                    ReceiveKind::Roster | ReceiveKind::Kick | ReceiveKind::Leave
                ) && !self
                    .group
                    .anon_group_repo
                    .is_member(&id, &sender)
                    .map_err(|error| error.to_string())?
                {
                    return Err("group sender not yet in roster".into());
                }
                self.group
                    .process_incoming_content(&id, &sender, &inner)
                    .map_err(|error| error.to_string())?
            }
            ("anon_group", Some(Body::AnonGroupMemberList(list))) => {
                let id = crate::domain::anon_group::AnonGroupId::try_from_slice(&list.group_id)
                    .ok_or("invalid group")?;
                if ChannelId::from(id) != scope {
                    return Err("group destination mismatch".into());
                }
                if self
                    .group
                    .handle_member_list_ordered(id, sender, list.members.clone(), list.timestamp)
                    .map_err(|e| e.to_string())?
                {
                    vec![DomainEvent::AnonGroupMemberListReceived {
                        group_id: id,
                        members: list.members,
                    }]
                } else {
                    Vec::new()
                }
            }
            ("anon_group", Some(Body::AnonGroupLeave(leave))) => {
                let id = crate::domain::anon_group::AnonGroupId::try_from_slice(&leave.group_id)
                    .ok_or("invalid group")?;
                if ChannelId::from(id) != scope {
                    return Err("group destination mismatch".into());
                }
                if self
                    .group
                    .handle_leave_ordered(id, sender, leave.timestamp)
                    .map_err(|e| e.to_string())?
                {
                    vec![DomainEvent::AnonGroupMemberLeft {
                        group_id: id,
                        user_id: sender,
                    }]
                } else {
                    Vec::new()
                }
            }
            ("anon_group", Some(Body::AnonGroupKick(kick))) => {
                let id = crate::domain::anon_group::AnonGroupId::try_from_slice(&kick.group_id)
                    .ok_or("invalid group")?;
                let target =
                    UserId::try_from_slice(&kick.kicked_user_id).ok_or("invalid kicked user")?;
                if ChannelId::from(id) != scope {
                    return Err("group destination mismatch".into());
                }
                if self
                    .group
                    .handle_kick(id, target, sender, &kick.admin_signature, kick.timestamp)
                    .map_err(|e| e.to_string())?
                {
                    vec![DomainEvent::AnonGroupMemberKicked {
                        group_id: id,
                        user_id: target,
                        kicked_by: sender,
                    }]
                } else {
                    Vec::new()
                }
            }
            ("anon_group", Some(Body::AnonGroupUpdate(update))) => {
                let id = crate::domain::anon_group::AnonGroupId::try_from_slice(&update.group_id)
                    .ok_or("invalid group")?;
                if ChannelId::from(id) != scope {
                    return Err("group destination mismatch".into());
                }
                if self
                    .group
                    .handle_update(id, sender, &update)
                    .map_err(|e| e.to_string())?
                {
                    vec![DomainEvent::AnonGroupUpdated {
                        group_id: id,
                        update,
                        updated_by: sender,
                    }]
                } else {
                    Vec::new()
                }
            }
            _ => return Err("secret artifact transport/content mismatch".into()),
        };
        if finish {
            repo.finish_content_artifact(artifact)
                .map_err(|error| error.to_string())?;
        }
        for event in events {
            self.events.emit_domain(event);
        }
        if finish {
            tracing::info!(envelope_id = %artifact.key.envelope_id, kind = artifact.kind.as_str(), "secret receive artifact applied");
        }
        Ok(receipt)
    }

    pub(crate) async fn replay(&self, repo: &Arc<MessageRepository>) {
        let mut cursor = None;
        for _ in 0..11 {
            let artifacts = match repo.pending_content_artifacts_after(cursor.as_ref()) {
                Ok(artifacts) => artifacts,
                Err(error) => {
                    tracing::warn!(%error, "Secret receive replay deferred");
                    break;
                }
            };
            if artifacts.is_empty() {
                break;
            }
            if let Some(last) = artifacts.last() {
                cursor = Some((last.key.server_timestamp_ns, last.id.clone()));
            }
            for artifact in artifacts {
                if let Err(error) = self.apply(repo, &artifact).await {
                    tracing::warn!(envelope_id = %artifact.key.envelope_id, %error, "Secret data post awaits replay");
                }
            }
        }
    }
}

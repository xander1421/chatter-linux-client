use super::*;

impl CallSignalingService {
    /// Whether we sent `peer` a rotation for `call_id` within `window`: its
    /// key request crossed that key in flight, and a second rotation would
    /// only churn the call (it asks again if the key never lands).
    pub fn recently_keyed(
        &self,
        call_id: &str,
        peer: &UserId,
        window: std::time::Duration,
    ) -> bool {
        sent_recently(
            self.last_key_sent.lock().as_ref(),
            call_id,
            peer,
            std::time::Instant::now(),
            window,
        )
    }

    /// Become the one task waiting for a group-call key in `call_id`; `None`
    /// when one already is. A wait for another (earlier) call is superseded:
    /// it ends by itself once that call is no longer the active one.
    pub fn begin_group_key_wait(&self, call_id: &str) -> Option<GroupKeyWaitGuard> {
        let mut slot = self.group_key_wait.lock();
        if slot.as_deref() == Some(call_id) {
            return None;
        }
        *slot = Some(call_id.to_string());
        Some(GroupKeyWaitGuard {
            slot: Arc::clone(&self.group_key_wait),
            call_id: call_id.to_string(),
        })
    }

    pub fn store_group_call_invite(&self, call_id: &str) -> bool {
        self.group_call_invites.store_group_call_invite(call_id)
    }

    pub fn buffer_group_call_key(
        &self,
        call_id: &str,
        sender: UserId,
        key: [u8; 32],
        epoch: u32,
    ) -> bool {
        self.group_call_invites
            .buffer_group_call_key(call_id, sender, key, epoch)
    }

    pub fn accept_group_call_invite(&self, call_id: &str) {
        self.group_call_invites.accept_group_call_invite(call_id);
    }

    pub fn take_group_call_invite(&self, call_id: &str) -> Vec<PendingGroupCallKey> {
        self.group_call_invites.take_group_call_invite(call_id)
    }

    pub fn is_ringing_group_call_invite(&self, call_id: &str) -> bool {
        self.group_call_invites
            .is_ringing_group_call_invite(call_id)
    }

    pub fn drop_ringing_group_call_invite(&self, call_id: &str) {
        self.group_call_invites
            .drop_ringing_group_call_invite(call_id);
    }

    /// Whether `user_id` is a member of `channel_id`, per the server's full
    /// (paginated) member list.
    pub async fn is_channel_member(
        &self,
        channel_id: &ChannelId,
        user_id: &UserId,
    ) -> Result<bool, CallSignalingError> {
        let members = with_policy_deadline(
            "channel member list",
            self.channel_service.get_all_channel_members(channel_id),
        )
        .await?;
        Ok(members.iter().any(|m| &m.user_id == user_id))
    }

    /// Redistribute a rotated group call key to the peers actually in the room.
    ///
    /// Unlike [`Self::ring_group_call`] — which rings the whole
    /// channel to a call that is starting — rotation exists to lock a departed
    /// participant *out*. A peer who leaves the LiveKit room is still a channel
    /// member, so sending the rotated key to the membership list hands it
    /// straight back to the attacker the rotation targets. Recipients are the
    /// live room roster intersected with channel membership: room identities
    /// that are not entitled to the channel are dropped, never sent to.
    pub async fn redistribute_group_call_key(
        &self,
        channel_id: &ChannelId,
        call_id: &str,
        key: [u8; 32],
        epoch: u32,
        room_identities: &[String],
    ) -> Result<usize, CallSignalingError> {
        let current_user = self.channel_service.current_user_id()?;
        // Full membership, paginated: a member past the server's page size who
        // IS in the room used to be silently skipped here and left unable to
        // decrypt after rotation (OPEN-ITEMS §16).
        let membership_started = std::time::Instant::now();
        let membership = with_policy_deadline(
            "channel member list",
            self.channel_service.get_all_channel_members(channel_id),
        )
        .await;
        let stage = if membership.is_ok() {
            "rotation_membership_refresh_succeeded"
        } else {
            "rotation_membership_refresh_failed"
        };
        tracing::info!(
            stage,
            call_id,
            epoch,
            elapsed_us = membership_started.elapsed().as_micros() as u64,
            "Call stage"
        );
        let members = membership?;
        let recipients = rotation_recipients(&members, room_identities, &current_user);
        let channel_id_str = channel_id.to_string();

        // In parallel: one slow recipient (a first session, a KT fetch) must
        // not hold every later participant's key — a joiner hears nothing
        // until its copy lands.
        let sends = recipients
            .iter()
            .enumerate()
            .map(|(recipient_index, peer_id)| {
                let content = build_group_call_signal_content(
                    call_id,
                    &channel_id_str,
                    crate::domain::call::GroupCallSignal::Key { key, epoch },
                );
                async move {
                    let send_started = std::time::Instant::now();
                    let sent = with_policy_deadline(
                        "group call key send",
                        self.message_service
                            .send_friend_dm_content(peer_id, content),
                    )
                    .await;
                    // Whole per-recipient send includes own-device sync; this is
                    // not a deposit ACK or evidence of remote audio arrival.
                    let stage = if sent.is_ok() {
                        "rotation_recipient_whole_send_including_own_device_sync_succeeded"
                    } else {
                        "rotation_recipient_whole_send_including_own_device_sync_failed"
                    };
                    tracing::info!(
                        stage,
                        call_id,
                        epoch,
                        recipient_index,
                        elapsed_us = send_started.elapsed().as_micros() as u64,
                        "Call stage: whole send includes own-device sync, not deposit ACK"
                    );
                    if let Err(e) = &sent {
                        tracing::warn!(
                            call_id, epoch, peer_id = %peer_id, error = %e,
                            "Failed to redistribute rotated group call key to participant"
                        );
                    }
                    sent.is_ok()
                }
            });
        let sent = futures::future::join_all(sends).await;
        let sent_count = sent.iter().filter(|ok| **ok).count();
        *self.last_key_sent.lock() = Some(KeySent {
            call_id: call_id.to_string(),
            at: std::time::Instant::now(),
            to: recipients
                .iter()
                .zip(&sent)
                .filter(|(_, ok)| **ok)
                .map(|(p, _)| *p)
                .collect(),
        });

        tracing::info!(
            call_id,
            epoch,
            sent_count,
            roster = room_identities.len(),
            members = members.len(),
            "Completed rotated group call key redistribution"
        );
        Ok(sent_count)
    }

    /// Ring a channel's members for the group call we started. The ring
    /// carries no key (§124 1k): the call's key goes only to participants in
    /// the room, from its key authority. Sent pairwise to each member but us.
    pub async fn ring_group_call(
        &self,
        channel_id: &ChannelId,
        call_id: &str,
    ) -> Result<usize, CallSignalingError> {
        let current_user = self.channel_service.current_user_id()?;
        // Full membership, paginated — a member past the server's page size
        // otherwise never rings (OPEN-ITEMS §16).
        let members = self
            .channel_service
            .get_all_channel_members(channel_id)
            .await?;
        let channel_id_str = channel_id.to_string();

        let mut sent_count = 0;
        for member in members.iter().filter(|m| m.user_id != current_user) {
            if crate::client::calls::start_cancelled() {
                break;
            }
            let content = build_group_call_signal_content(
                call_id,
                &channel_id_str,
                crate::domain::call::GroupCallSignal::Ring,
            );
            match self
                .message_service
                .send_friend_dm_content(&member.user_id, content)
                .await
            {
                Ok(_) => sent_count += 1,
                Err(e) => {
                    tracing::warn!(call_id, peer_id = %member.user_id, error = %e, "Group call ring not sent")
                }
            }
        }

        tracing::info!(call_id, sent_count, "Group call rung");
        Ok(sent_count)
    }

    /// Ask the call's key `authority` to rotate: we are in the room holding
    /// no current key — just joined, or a rotation was lost on its way.
    pub async fn request_group_call_key(
        &self,
        channel_id: &ChannelId,
        call_id: &str,
        authority: &UserId,
    ) -> Result<(), CallSignalingError> {
        let content = build_group_call_signal_content(
            call_id,
            &channel_id.to_string(),
            crate::domain::call::GroupCallSignal::KeyRequest,
        );
        with_policy_deadline(
            "group call key request",
            self.message_service
                .send_friend_dm_content(authority, content),
        )
        .await?;
        tracing::info!(call_id, %authority, "Group call key requested");
        Ok(())
    }
}
/// Recipients for a rotated group call key.
///
/// The live LiveKit room roster (`room_identities`, each entry a peer's
/// `user_id` — the server mints call tokens with `SetIdentity(user_id)`)
/// intersected with channel membership, minus ourselves. The roster decides
/// *who still needs the key*; membership decides *who is entitled to it*, so a
/// room identity that is not a channel member is dropped rather than sent to.
fn rotation_recipients(
    members: &[crate::domain::channel::Membership],
    room_identities: &[String],
    current_user: &UserId,
) -> Vec<UserId> {
    let in_room: std::collections::HashSet<&str> =
        room_identities.iter().map(|s| s.as_str()).collect();
    members
        .iter()
        .filter(|m| &m.user_id != current_user)
        .filter(|m| in_room.contains(m.user_id.to_string().as_str()))
        .map(|m| m.user_id)
        .collect()
}

#[cfg(test)]
mod rotation_recipient_tests {
    use super::super::invites::buffer_newest_per_sender;
    use super::*;

    /// A key request that crossed the rotation it asks for in flight is not
    /// answered with another rotation; a later one, or one for another call
    /// or from someone the key did not go to, is.
    #[test]
    fn a_request_crossing_its_key_in_flight_is_not_rerotated() {
        let (b, c) = (UserId::new(), UserId::new());
        let t0 = std::time::Instant::now();
        let w = std::time::Duration::from_secs(2);
        let k = KeySent {
            call_id: "call".into(),
            at: t0,
            to: vec![b],
        };
        assert!(sent_recently(
            Some(&k),
            "call",
            &b,
            t0 + std::time::Duration::from_millis(900),
            w
        ));
        assert!(
            !sent_recently(
                Some(&k),
                "call",
                &b,
                t0 + std::time::Duration::from_secs(3),
                w
            ),
            "too long ago"
        );
        assert!(
            !sent_recently(Some(&k), "call", &c, t0, w),
            "not a recipient"
        );
        assert!(!sent_recently(Some(&k), "other", &b, t0, w), "another call");
        assert!(!sent_recently(None, "call", &b, t0, w));
    }
    #[test]
    fn delayed_request_is_suppressed_only_until_in_flight_window_expires() {
        let peer = UserId::new();
        let other = UserId::new();
        let sent = std::time::Instant::now();
        let key = KeySent {
            call_id: "call".into(),
            at: sent,
            to: vec![peer],
        };
        let window = std::time::Duration::from_secs(10);
        let delayed = sent + std::time::Duration::from_millis(3131);
        assert!(sent_recently(Some(&key), "call", &peer, delayed, window));
        assert!(!sent_recently(
            Some(&key),
            "call",
            &peer,
            sent + window,
            window
        ));
        assert!(!sent_recently(Some(&key), "call", &other, delayed, window));
        assert!(!sent_recently(Some(&key), "rejoin", &peer, delayed, window));
    }

    use crate::domain::channel::{MemberRole, Membership, NotificationPreference};

    fn member(channel_id: &ChannelId, user_id: &UserId) -> Membership {
        Membership {
            channel_id: *channel_id,
            user_id: user_id.clone(),
            role: MemberRole::Member,
            joined_at: chrono::Utc::now(),
            last_read_message_id: None,
            is_muted: false,
            muted_until: None,
            notification_preference: NotificationPreference::default(),
        }
    }

    /// A participant who left the LiveKit room is still a channel member.
    /// Rotation exists to lock them out, so they must NOT be a recipient.
    #[test]
    fn departed_member_does_not_receive_the_rotated_key() {
        let channel_id = ChannelId::new();
        let me = UserId::new();
        let stayed = UserId::new();
        let departed = UserId::new();

        let members = vec![
            member(&channel_id, &me),
            member(&channel_id, &stayed),
            member(&channel_id, &departed),
        ];
        // Roster sampled after the departure: only `stayed` is still in the room.
        let roster = vec![stayed.to_string()];

        let recipients = rotation_recipients(&members, &roster, &me);

        assert!(
            !recipients.contains(&departed),
            "rotated key was sent to the departed participant {departed} — rotation is a no-op"
        );
        assert_eq!(recipients, vec![stayed.clone()]);
    }

    /// Authorisation: a room identity with no channel membership is not
    /// entitled to the channel's call key, so it is dropped.
    #[test]
    fn room_identity_without_membership_is_not_sent_to() {
        let channel_id = ChannelId::new();
        let me = UserId::new();
        let stayed = UserId::new();
        let intruder = UserId::new();

        let members = vec![member(&channel_id, &me), member(&channel_id, &stayed)];
        let roster = vec![stayed.to_string(), intruder.to_string()];

        let recipients = rotation_recipients(&members, &roster, &me);

        assert_eq!(recipients, vec![stayed.clone()]);
        assert!(!recipients.contains(&intruder));
    }

    /// We rotate our own key locally; never DM it to ourselves.
    #[test]
    fn self_is_excluded_even_when_on_the_roster() {
        let channel_id = ChannelId::new();
        let me = UserId::new();
        let members = vec![member(&channel_id, &me)];
        let roster = vec![me.to_string()];

        assert!(rotation_recipients(&members, &roster, &me).is_empty());
    }

    /// Finding 4: a member flooding high epochs while we join cannot
    /// displace another sender's key; the buffer stays bounded.
    #[test]
    fn invite_buffer_keeps_each_senders_newest_key() {
        let mut held = HashMap::new();
        let rotator = UserId::new();
        let mallory = UserId::new();
        buffer_newest_per_sender(&mut held, rotator, [1u8; 32], 4);
        buffer_newest_per_sender(&mut held, mallory, [6u8; 32], u32::MAX);
        buffer_newest_per_sender(&mut held, rotator, [2u8; 32], 3);
        assert_eq!(
            held[&rotator].1, 4,
            "an older key from the same sender is dropped"
        );
        assert_eq!(*held[&rotator].0, [1u8; 32]);
        buffer_newest_per_sender(&mut held, rotator, [3u8; 32], 5);
        assert_eq!(*held[&rotator].0, [3u8; 32]);

        for _ in 0..crate::services::call_signaling::invites::MAX_BUFFERED_KEY_SENDERS {
            buffer_newest_per_sender(&mut held, UserId::new(), [0u8; 32], 1);
        }
        assert_eq!(held.len(), crate::services::call_signaling::invites::MAX_BUFFERED_KEY_SENDERS);
        assert!(held.contains_key(&rotator), "held senders keep their slot");
    }
}

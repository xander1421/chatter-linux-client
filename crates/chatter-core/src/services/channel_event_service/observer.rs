use super::*;

/// Display of verified membership changes: the roster rows, the member
/// notices and the domain events the UI reacts to. Key decisions never read
/// any of this; they read the verified state `MembershipLog` just committed.
impl LogObserver for ChannelEventService {
    fn applied(&self, a: &Applied) {
        use crate::domain::channel::NotificationPreference;
        use crate::proto::chat_v1::{channel_action::Action, remove_member::Reason};
        use crate::services::channel::converters::proto_role_to_domain;
        use crate::services::channel::verified_state::MemberStatus;

        let parse = |u: &str| u.parse::<uuid::Uuid>().ok().map(UserId::from);
        let Some(author) = parse(&a.author) else {
            return;
        };
        let ts = a.timestamp;
        let active = |s: Option<&crate::services::channel::verified_state::MembershipState>,
                      u: &str| {
            s.and_then(|s| s.members.get(u))
                .filter(|m| m.status == MemberStatus::Active)
                .map(|m| m.role)
        };
        let genesis = a
            .actions
            .iter()
            .any(|x| matches!(x.action, Some(Action::Genesis(_))));
        let notify = !a.backfill;

        // Departures, with the signed reason.
        if let Some(prev) = &a.prev {
            for (uid, _) in prev.active_members() {
                if active(Some(&a.new), uid).is_some() {
                    continue;
                }
                let Some(user) = parse(uid) else { continue };
                let reason = a
                    .actions
                    .iter()
                    .find_map(|x| match &x.action {
                        Some(Action::RemoveMember(r)) if r.user_id == *uid => Some(r.reason),
                        _ => None,
                    })
                    .map_or(MemberRemoval::Unexplained, |r| match Reason::try_from(r) {
                        Ok(Reason::Kicked) => MemberRemoval::Kicked(author),
                        Ok(Reason::Banned) => MemberRemoval::Banned(author),
                        _ => MemberRemoval::Left,
                    });
                if notify {
                    self.publish_member_removed(&a.channel_id, &user, reason, ts);
                } else if let Err(e) = self.channel_repo.delete_membership(&a.channel_id, &user) {
                    tracing::debug!(channel_id = %a.channel_id, %user, error = %e, "No roster row to drop");
                }
            }
        }

        // Arrivals, and role changes of those who stayed.
        for (uid, m) in a.new.active_members() {
            let Some(user) = parse(uid) else { continue };
            let role = proto_role_to_domain(m.role as i32);
            match active(a.prev.as_ref(), uid) {
                None => {
                    let membership = Membership {
                        channel_id: a.channel_id,
                        user_id: user,
                        role,
                        notification_preference: NotificationPreference::All,
                        joined_at: chrono::DateTime::from_timestamp_nanos(ts as i64),
                        last_read_message_id: None,
                        is_muted: false,
                        muted_until: None,
                    };
                    if notify && !(genesis && user == author) {
                        self.publish_member_joined(&membership, ts);
                    } else if let Err(e) =
                        self.channel_repo.insert_or_ignore_membership(&membership)
                    {
                        tracing::warn!(channel_id = %a.channel_id, %user, error = %e, "Failed to store member");
                    }
                }
                Some(old) if old != m.role => {
                    if let Ok(Some(mut row)) =
                        self.channel_repo.get_membership(&a.channel_id, &user)
                    {
                        row.role = role;
                        if let Err(e) = self.channel_repo.update_membership(&row) {
                            tracing::warn!(channel_id = %a.channel_id, %user, error = %e, "Failed to store role change");
                        }
                    }
                    let _ =
                        self.event_bus
                            .domain_sender()
                            .send(DomainEvent::ChannelMemberUpdated {
                                channel_id: a.channel_id,
                                user_id: user,
                                role,
                            });
                }
                Some(_) => {}
            }
        }

        for x in &a.actions {
            if let Some(Action::TransferOwnership(t)) = &x.action {
                if let Some(to) = parse(&t.new_owner_user_id) {
                    if notify {
                        self.record_ownership_transferred(&a.channel_id, &author, &to, ts);
                    } else if let Err(e) = crate::services::channel::apply_ownership_transfer(
                        &self.channel_repo,
                        &a.channel_id,
                        &author,
                        &to,
                    ) {
                        tracing::warn!(channel_id = %a.channel_id, error = %e, "Failed to record ownership transfer");
                    }
                }
            }
        }
    }

    fn notice(&self, channel_id: &ChannelId, notice: MembershipNotice) {
        let _ = self
            .event_bus
            .domain_sender()
            .send(DomainEvent::ChannelMembershipNotice {
                channel_id: *channel_id,
                notice,
            });
    }
}

//! Channel membership operations.
//!
//! Every membership mutation is a signed `ChannelChange` authored through
//! [`MembershipLog::author`](crate::services::channel::membership_log::MembershipLog::author):
//! the actions are built from the verified state, previewed with the
//! verifier's rules for `new_state_hash`, signed by the identity key and sent
//! on the matching RPC. The local roster (`channel_repo`) is written for
//! display by `ChannelEventService` when the change is committed, the same
//! path every other member's copy takes.

use tokio_stream::{Stream, StreamExt};

use crate::domain::channel::{MemberRole, Membership};
use crate::domain::ids::{ChannelId, UserId};
use crate::mailbox::group::GroupMailboxSetupResult;
use crate::network::grpc::channel_client::ChannelClient;
use crate::proto::chat_v1::{self as pb, channel_event, ChannelEvent};
use crate::services::channel::membership_log::{
    invite_pub_of, password_invite_key, AuthorBase, IntentContext, InviteLink, LogError,
};
use crate::services::channel::verified_state::{ChannelRole, MemberStatus, UnsignedEvent};
use crate::services::current_user::CurrentUserProvider;
use crate::error::NetworkError;

use crate::services::channel::converters::{domain_role_to_proto, proto_to_domain_membership};
use crate::services::channel::error::ChannelError;

use super::ChannelService;
use pb::channel_action::Action;
use pb::remove_member::Reason;

/// Every proto member, or an error before the caller replaces the local
/// roster. A partial conversion must not delete the member it failed to read.
fn full_member_list(
    channel_id: &ChannelId,
    proto_members: &[pb::channel::ChannelMember],
) -> Result<Vec<Membership>, ChannelError> {
    proto_members
        .iter()
        .map(|proto| proto_to_domain_membership(channel_id, proto))
        .collect()
}

pub(super) fn action(a: Action) -> pb::ChannelAction {
    pb::ChannelAction { action: Some(a) }
}

/// Run a membership RPC's event stream to its end. `OperationFailed` is the
/// server refusing after the change was taken; an `Err` item is the call
/// failing (a conflict surfaces here as `ABORTED`).
pub(super) async fn drain<S>(mut stream: S, op: &str) -> Result<(), NetworkError>
where
    S: Stream<Item = Result<ChannelEvent, NetworkError>> + Unpin,
{
    while let Some(event) = stream.next().await {
        if let Some(channel_event::Event::OperationFailed(failure)) = event?.event {
            let msg = failure
                .error
                .map(|e| format!("{op} failed: {:?} - {}", e.code, e.message))
                .unwrap_or_else(|| format!("{op} failed"));
            return Err(NetworkError::InvalidRequest(msg));
        }
    }
    Ok(())
}

fn status_of(ctx: &IntentContext<'_>, user: &str) -> Option<MemberStatus> {
    ctx.state.and_then(|s| s.members.get(user)).map(|m| m.status)
}

fn intent_error(msg: &str) -> LogError {
    LogError::Intent(crate::services::channel::verified_state::VerifyError::InvalidAction(msg.to_string()))
}

/// How a joiner proves the right to join.
enum JoinProof {
    /// OPEN channel, or a pending invite to accept.
    None,
    /// A join link's or password's capability key.
    Key(libsignal_protocol::PrivateKey),
}

impl ChannelService {
    /// Join a channel: accept a pending invite, self-join an open channel,
    /// or (with `password`) join through the password capability.
    ///
    /// `setup_mailbox` is retained for call-site compatibility only.
    pub async fn join_channel(
        &self,
        channel_id: &ChannelId,
        password: Option<&str>,
        setup_mailbox: bool,
    ) -> Result<(crate::domain::channel::Channel, Option<GroupMailboxSetupResult>), ChannelError> {
        let _ = setup_mailbox;
        let proof = match password.filter(|p| !p.is_empty()) {
            Some(p) => JoinProof::Key(password_invite_key(channel_id, p)?),
            None => JoinProof::None,
        };
        let channel = self.join_with(channel_id, proof).await?;
        Ok((channel, None))
    }

    /// Join through a `chatter://join/...` link.
    pub async fn join_via_link(&self, url: &str) -> Result<crate::domain::channel::Channel, ChannelError> {
        let link = InviteLink::parse(url).ok_or_else(|| ChannelError::InvalidInput("not a channel invite link".into()))?;
        self.join_with(&link.channel_id, JoinProof::Key(link.invite_key)).await
    }

    async fn join_with(&self, channel_id: &ChannelId, proof: JoinProof) -> Result<crate::domain::channel::Channel, ChannelError> {
        let me = self.settings_repo.current_user_id()?;
        let cid = channel_id.to_string();
        let (invite_key, invite_pub) = match &proof {
            JoinProof::Key(k) => (Some(k), invite_pub_of(k).to_vec()),
            JoinProof::None => (None, Vec::new()),
        };

        self.membership_log
            .author(
                channel_id,
                AuthorBase::Server { invite_pub: &invite_pub },
                invite_key,
                |ctx| {
                    let a = match status_of(ctx, ctx.me) {
                        Some(MemberStatus::Invited) => Action::AcceptInvite(pb::AcceptInvite {}),
                        Some(_) => return Err(intent_error("already in the channel")),
                        None => Action::JoinViaInvite(pb::JoinViaInvite { invite_pub: invite_pub.clone() }),
                    };
                    Ok(vec![action(a)])
                },
                |change| {
                    let mut client = self.channel_client();
                    let cid = cid.clone();
                    async move { drain(client.join_channel(&cid, change, false).await?, "join").await }
                },
            )
            .await?;

        let mut channel_client = self.channel_client();
        let channel = self.get_channel_internal(&mut channel_client, channel_id).await?;

        let active = self
            .membership_log
            .load(channel_id)?
            .and_then(|v| v.state.members.get(&me.to_string()).map(|m| m.status == MemberStatus::Active))
            .unwrap_or(false);
        if !active {
            // A needs_approval link: we are REQUESTING until an admin's
            // signed AddMember. No sender key until then.
            tracing::info!(%channel_id, "Join requested; waiting for approval");
            return Ok(channel);
        }
        self.after_becoming_member(&mut channel_client, channel_id).await;
        Ok(channel)
    }

    /// Create and distribute our sender key once we are an ACTIVE member.
    async fn after_becoming_member(&self, channel_client: &mut ChannelClient, channel_id: &ChannelId) {
        // Display roster for the UI; key decisions read the verified state.
        if let Err(e) = self.sync_channel_members_internal(channel_client, channel_id).await {
            tracing::warn!(%channel_id, error = %e, "Failed to sync channel roster after join");
        }
        let skdm = match self.signal_service.create_sender_key(channel_id).await {
            Ok(skdm) => skdm,
            Err(e) => {
                tracing::error!(%channel_id, error = %e, "Failed to create sender key on join — group encryption unavailable");
                return;
            }
        };
        // Best-effort: peers that miss it heal via SenderKeyRequest.
        if let Err(e) = self.message_service.distribute_skdm(channel_id, &skdm).await {
            tracing::warn!(%channel_id, error = %e, "Failed to distribute SKDM on join (peers will heal via SenderKeyRequest)");
        }
    }

    /// Sync channel members from the server to local storage (display only).
    pub async fn sync_channel_members(&self, channel_id: &ChannelId) -> Result<usize, ChannelError> {
        let mut channel_client = self.channel_client();
        self.sync_channel_members_internal(&mut channel_client, channel_id).await
    }

    /// Internal: Sync channel members using an existing client.
    ///
    /// The server roster is a DISPLAY roster. Who receives sender keys is the
    /// verified state; a roster difference from it is an unsigned membership
    /// fact and goes through the asymmetric rule (an add is ignored and
    /// surfaced, a removal excludes the user from distribution).
    pub(super) async fn sync_channel_members_internal(
        &self,
        channel_client: &mut ChannelClient,
        channel_id: &ChannelId,
    ) -> Result<usize, ChannelError> {
        let proto_members = channel_client.get_all_channel_members(&channel_id.to_string()).await?;
        let memberships = full_member_list(channel_id, &proto_members)?;
        let refresh = self.channel_repo.replace_memberships(channel_id, &memberships)?;
        self.classify_roster_changes(channel_id, &refresh.added, &refresh.removed).await;
        Ok(proto_members.len())
    }

    pub(super) async fn classify_roster_changes(&self, channel_id: &ChannelId, added: &[Membership], removed: &[Membership]) {
        // A legitimate joiner shows up in the roster a moment before their
        // signed change reaches us. Fetch the signed log first, as Signal
        // fetches group changes before trusting a roster, so only an add that
        // is STILL unsigned afterwards is surfaced.
        if !added.is_empty() {
            if let Err(e) = self.membership_log.catch_up(channel_id).await {
                tracing::debug!(%channel_id, error = %e, "Membership catch-up before classifying the roster failed");
            }
        }
        let events = added
            .iter()
            .map(|m| UnsignedEvent::Added { user_id: m.user_id.to_string() })
            .chain(removed.iter().map(|m| UnsignedEvent::Removed { user_id: m.user_id.to_string() }));
        for event in events {
            if let Err(e) = self.membership_log.apply_unsigned(channel_id, event) {
                tracing::warn!(%channel_id, error = %e, "Could not apply an unsigned roster change");
            }
        }
    }

    /// Leave a channel.
    ///
    /// Rotates our sender key first (we are still a member) and returns the
    /// SKDM for the caller to distribute to the remaining members, as Signal
    /// rotates on any departure. Sending stays blocked if anything fails.
    pub async fn leave_channel(
        &self,
        channel_id: &ChannelId,
    ) -> Result<crate::proto::content_v1::SenderKeyDistribution, ChannelError> {
        if let Err(e) = self.signal_service.block_channel_send(channel_id) {
            tracing::error!(%channel_id, error = %e, "Failed to block channel send before leave");
        }
        let skdm = self.signal_service.rotate_sender_key(channel_id).await.map_err(|e| {
            let _ = self.signal_service.mark_channel_for_rotation(channel_id);
            ChannelError::SenderKeyRotationFailed(format!(
                "rotation failed before leaving: {e} — message sending blocked until resolved"
            ))
        })?;

        let cid = channel_id.to_string();
        let left = self
            .membership_log
            .author(
                channel_id,
                AuthorBase::Verified,
                None,
                |ctx| {
                    Ok(vec![action(Action::RemoveMember(pb::RemoveMember {
                        user_id: ctx.me.to_string(),
                        reason: Reason::Left as i32,
                    }))])
                },
                |change| {
                    let mut client = self.channel_client();
                    let cid = cid.clone();
                    async move { drain(client.leave_channel(&cid, change).await?, "leave").await }
                },
            )
            .await;
        if let Err(e) = left {
            // The rotated key was never distributed: keep sending blocked.
            let _ = self.signal_service.mark_channel_for_rotation(channel_id);
            return Err(e.into());
        }

        if let Err(e) = self.signal_service.clear_channel_sender_keys(channel_id).await {
            tracing::warn!(%channel_id, error = %e, "Failed to clear sender keys when leaving channel");
        }
        if let Err(e) = self.group_coordinator.cleanup_channel(channel_id) {
            tracing::warn!(%channel_id, error = %e, "Failed to clean up group mailbox when leaving channel");
        }
        Ok(skdm)
    }

    /// Invite a user: a signed `AddMember{pending}` vouching for their
    /// KT-verified identity key. They become a member by their own signed
    /// `AcceptInvite`.
    pub async fn invite_user(
        &self,
        channel_id: &ChannelId,
        invitee_id: &UserId,
        message: Option<&str>,
    ) -> Result<(), ChannelError> {
        let invitee = invitee_id.to_string();
        let identity_key = self.membership_log.vouched_identity_key(&invitee).await?;
        let cid = channel_id.to_string();
        let message = message.map(str::to_string);
        self.membership_log
            .author(
                channel_id,
                AuthorBase::Verified,
                None,
                |ctx| {
                    if status_of(ctx, &invitee).is_some() {
                        return Err(intent_error("already in the channel or invited"));
                    }
                    Ok(vec![action(Action::AddMember(pb::AddMember {
                        user_id: invitee.clone(),
                        role: ChannelRole::Member as i32,
                        identity_key: identity_key.to_vec(),
                        pending: true,
                    }))])
                },
                |change| {
                    let mut client = self.channel_client();
                    let (cid, message) = (cid.clone(), message.clone());
                    async move { drain(client.invite_user(&cid, change, message.as_deref()).await?, "invite").await }
                },
            )
            .await?;
        Ok(())
    }

    /// Respond to a channel invitation: a signed `AcceptInvite`, or a signed
    /// self-`RemoveMember{LEFT}` to decline. Accepting returns the channel
    /// and sets up our sender key as a join does; declining returns `None`.
    pub async fn respond_to_invite(
        &self,
        channel_id: &ChannelId,
        accept: bool,
    ) -> Result<Option<crate::domain::channel::Channel>, ChannelError> {
        let cid = channel_id.to_string();
        self.membership_log
            .author(
                channel_id,
                AuthorBase::Verified,
                None,
                |ctx| {
                    if status_of(ctx, ctx.me) != Some(MemberStatus::Invited) {
                        return Err(intent_error("no pending invite"));
                    }
                    let a = if accept {
                        Action::AcceptInvite(pb::AcceptInvite {})
                    } else {
                        Action::RemoveMember(pb::RemoveMember { user_id: ctx.me.to_string(), reason: Reason::Left as i32 })
                    };
                    Ok(vec![action(a)])
                },
                |change| {
                    let mut client = self.channel_client();
                    let cid = cid.clone();
                    async move { drain(client.respond_to_invite(&cid, change).await?, "respond to invite").await }
                },
            )
            .await?;
        if !accept {
            return Ok(None);
        }
        let mut channel_client = self.channel_client();
        let channel = self.get_channel_internal(&mut channel_client, channel_id).await?;
        self.after_becoming_member(&mut channel_client, channel_id).await;
        Ok(Some(channel))
    }

    /// Approve a REQUESTING joiner (a `needs_approval` link): a signed
    /// `AddMember` that keeps the joiner's own identity key.
    pub async fn approve_join_request(&self, channel_id: &ChannelId, user_id: &UserId) -> Result<(), ChannelError> {
        let user = user_id.to_string();
        self.submit(channel_id, |ctx| {
            let m = ctx
                .state
                .and_then(|s| s.members.get(&user))
                .filter(|m| m.status == MemberStatus::Requesting)
                .ok_or_else(|| intent_error("no such join request"))?;
            Ok(vec![action(Action::AddMember(pb::AddMember {
                user_id: user.clone(),
                role: m.role as i32,
                identity_key: m.identity_key.to_vec(),
                pending: false,
            }))])
        })
        .await
    }

    /// Reject a join request, or withdraw an invite we (or another admin)
    /// sent: a signed `RemoveMember` of a not-yet-active user.
    pub async fn remove_pending_member(&self, channel_id: &ChannelId, user_id: &UserId) -> Result<(), ChannelError> {
        let user = user_id.to_string();
        self.submit(channel_id, |ctx| {
            match status_of(ctx, &user) {
                Some(MemberStatus::Requesting | MemberStatus::Invited) => {}
                _ => return Err(intent_error("no pending request or invite for that user")),
            }
            Ok(vec![action(Action::RemoveMember(pb::RemoveMember { user_id: user.clone(), reason: Reason::Kicked as i32 }))])
        })
        .await
    }

    /// Create a join link. The private key stays in the returned link
    /// (share it out of band or in an E2EE message); only its public half
    /// enters the signed state.
    pub async fn create_invite_link(
        &self,
        channel_id: &ChannelId,
        role: MemberRole,
        expires_at: Option<chrono::DateTime<chrono::Utc>>,
        max_uses: u32,
        needs_approval: bool,
    ) -> Result<InviteLink, ChannelError> {
        let link = InviteLink::generate(*channel_id);
        let capability = pb::InviteCapability {
            invite_pub: link.invite_pub().to_vec(),
            role: domain_role_to_proto(role) as i32,
            expires_at: expires_at.and_then(|t| t.timestamp_nanos_opt()).map_or(0, |n| n.max(1) as u64),
            max_uses,
            needs_approval,
            uses: 0,
        };
        self.submit(channel_id, |_| {
            Ok(vec![action(Action::CreateInvite(pb::CreateInvite { capability: Some(capability.clone()) }))])
        })
        .await?;
        Ok(link)
    }

    /// Revoke a join link by its public key.
    pub async fn revoke_invite_link(&self, channel_id: &ChannelId, invite_pub: &[u8]) -> Result<(), ChannelError> {
        let invite_pub = invite_pub.to_vec();
        self.submit(channel_id, |_| Ok(vec![action(Action::RevokeInvite(pb::RevokeInvite { invite_pub: invite_pub.clone() }))]))
            .await
    }

    /// Author a change on the verified state and send it with
    /// `SubmitChannelChange` (log-only actions and approvals).
    async fn submit<I>(&self, channel_id: &ChannelId, intent: I) -> Result<(), ChannelError>
    where
        I: Fn(&IntentContext<'_>) -> Result<Vec<pb::ChannelAction>, LogError>,
    {
        self.membership_log
            .author(channel_id, AuthorBase::Verified, None, intent, |change| {
                let mut client = self.channel_client();
                async move { client.submit_channel_change(change).await }
            })
            .await?;
        Ok(())
    }

    /// Get channel members with pagination (display roster).
    pub async fn get_channel_members(
        &self,
        channel_id: &ChannelId,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<Membership>, ChannelError> {
        let _ = self.settings_repo.current_user_id()?;

        let channel_id_str = channel_id.to_string();
        let mut channel_client = self.channel_client();
        let response = channel_client
            .get_channel_members(&channel_id_str, limit as i32, offset as i32)
            .await?;

        let mut memberships = Vec::with_capacity(response.members.len());
        let mut added = Vec::new();
        for proto_member in &response.members {
            let membership = proto_to_domain_membership(channel_id, proto_member)?;

            match self
                .channel_repo
                .get_membership(channel_id, &membership.user_id)?
            {
                Some(_) => self.channel_repo.update_membership(&membership)?,
                None => {
                    self.channel_repo.insert_membership(&membership)?;
                    added.push(membership.clone());
                }
            }

            memberships.push(membership);
        }
        // A row this page adds locally is an unsigned roster fact, as in a
        // full sync: once it is stored here no later sync sees it as added,
        // so classify it now or an unsigned add is never surfaced.
        self.classify_roster_changes(channel_id, &added, &[]).await;

        Ok(memberships)
    }

    /// Get EVERY channel member, paginating past the server page size
    /// (display roster; see [`Self::sync_channel_members_internal`]).
    pub async fn get_all_channel_members(
        &self,
        channel_id: &ChannelId,
    ) -> Result<Vec<Membership>, ChannelError> {
        let _ = self.settings_repo.current_user_id()?;

        let mut channel_client = self.channel_client();
        let proto_members = channel_client
            .get_all_channel_members(&channel_id.to_string())
            .await?;

        let memberships = full_member_list(channel_id, &proto_members)?;
        let refresh = self.channel_repo.replace_memberships(channel_id, &memberships)?;
        self.classify_roster_changes(channel_id, &refresh.added, &refresh.removed).await;

        Ok(memberships)
    }

    /// Hand the channel to `new_owner`, a current member; we become an admin.
    pub async fn transfer_ownership(&self, channel_id: &ChannelId, new_owner: &UserId) -> Result<(), ChannelError> {
        let to = new_owner.to_string();
        let cid = channel_id.to_string();
        self.membership_log
            .author(
                channel_id,
                AuthorBase::Verified,
                None,
                |_| Ok(vec![action(Action::TransferOwnership(pb::TransferOwnershipAction { new_owner_user_id: to.clone() }))]),
                |change| {
                    let mut client = self.channel_client();
                    let cid = cid.clone();
                    async move { client.transfer_ownership(&cid, change).await }
                },
            )
            .await?;
        Ok(())
    }

    /// Update a member's role with a signed `SetRole`.
    pub async fn update_member_role(
        &self,
        channel_id: &ChannelId,
        user_id: &UserId,
        new_role: MemberRole,
    ) -> Result<Membership, ChannelError> {
        // Ownership is not grantable; only TransferOwnership moves it.
        if new_role == MemberRole::Owner {
            return Err(ChannelError::InvalidInput("ownership cannot be granted".to_string()));
        }
        let target = user_id.to_string();
        let role = domain_role_to_proto(new_role) as i32;
        let cid = channel_id.to_string();
        self.membership_log
            .author(
                channel_id,
                AuthorBase::Verified,
                None,
                |_| Ok(vec![action(Action::SetRole(pb::SetRole { user_id: target.clone(), role }))]),
                |change| {
                    let mut client = self.channel_client();
                    let cid = cid.clone();
                    async move { drain(client.update_member_role(&cid, change).await?, "update role").await }
                },
            )
            .await?;
        self.channel_repo
            .get_membership(channel_id, user_id)?
            .ok_or_else(|| ChannelError::InvalidInput("member not in the local roster".into()))
    }

    /// Full leave orchestration: block sending → rotate sender key → signed
    /// leave → distribute final SKDM to remaining members → unblock sending
    /// → purge local sender keys for this channel.
    ///
    /// SKDM distribution failure is best-effort: remaining members will
    /// rotate themselves via `mark_channel_for_rotation` on their next send.
    pub async fn leave_channel_complete(
        &self,
        channel_id: &ChannelId,
    ) -> Result<(), ChannelError> {
        let skdm = self.leave_channel(channel_id).await?;

        if let Err(e) = self.message_service.distribute_skdm(channel_id, &skdm).await {
            tracing::warn!(
                channel_id = %channel_id,
                error = %e,
                "Failed to distribute SKDM on leave — remaining members will rotate on next send"
            );
        }

        if let Err(e) = self.signal_service.unblock_channel_send(channel_id) {
            tracing::warn!(channel_id = %channel_id, error = %e, "Failed to unblock channel send after leave");
        }

        if let Err(e) = self.signal_service.clear_channel_sender_keys(channel_id).await {
            tracing::warn!(
                channel_id = %channel_id,
                error = %e,
                "Failed to purge sender keys after leaving channel"
            );
        }

        Ok(())
    }
}

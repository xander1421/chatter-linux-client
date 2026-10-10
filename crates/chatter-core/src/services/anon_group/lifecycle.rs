//! Group creation and join flow operations.

use std::time::Duration;

use chrono::Utc;
use rand::RngCore;
use tokio::time::sleep;
use uuid::Uuid;

use crate::domain::UserId;
use crate::domain::anon_group::{
    AnonGroup, AnonGroupId, AnonGroupMailbox, AnonGroupMember, AnonGroupRole,
};
use crate::services::current_user::CurrentUserProvider;
use crate::storage::repositories::settings_keys;
use crate::util::time::TimestampNanos;

use super::{AnonGroupError, AnonGroupService};

/// Default mailbox TTL (server-enforced) when creating or refreshing an
/// anonymous group mailbox.
///
/// Matches the original `create_group_mailbox` TTL; refresh keeps this
/// unchanged so we preserve the existing forward-secrecy window.
pub const DEFAULT_GROUP_MAILBOX_TTL_DAYS: i64 = 30;

/// How far in advance of expiry to proactively refresh a group mailbox.
///
/// A 7 day lead time gives peers ample opportunity to receive the new
/// `mailbox_pk` broadcast before the old address stops accepting deliveries,
/// minimising in-flight message loss given that the on-disk schema can only
/// hold a single mailbox per group (`UNIQUE(group_id)` in
/// `anon_group_mailboxes`).
pub const GROUP_MAILBOX_REFRESH_LEAD_DAYS: i64 = 7;

impl AnonGroupService {
    /// Get the current user ID.
    pub fn current_user_id(&self) -> Result<UserId, AnonGroupError> {
        Ok(self.settings_repo.current_user_id()?)
    }

    /// Our profile key, when the visibility setting shares it with anon
    /// peers (`profile_visibility::anon_shareable_profile_key`).
    pub(crate) fn get_my_profile_key(&self) -> Option<[u8; 32]> {
        crate::services::profile_visibility::anon_shareable_profile_key(&self.settings_repo)
    }

    /// Get our display name from settings.
    pub(crate) fn get_my_display_name(&self) -> Option<String> {
        self.settings_repo
            .get_string(settings_keys::DISPLAY_NAME)
            .ok()
            .flatten()
    }

    /// Get our avatar bytes from settings (stored locally).
    pub(crate) fn get_my_avatar(&self) -> Option<Vec<u8>> {
        self.settings_repo
            .get_bytes(settings_keys::AVATAR)
            .ok()
            .flatten()
    }

    /// The secret-chat attachment service: every transfer over Tor.
    fn secret_attachments(&self) -> crate::services::AttachmentService {
        crate::services::AttachmentService::new(
            self.attachment_repo.clone(),
            self.grpc_pool.clone(),
        )
        .for_secret_chat()
    }

    /// The pointer for `avatar` (ours, or a member's we relay in a roster):
    /// uploaded once over Tor, then reused (see [`crate::services::secret_avatar`]).
    pub(crate) async fn avatar_pointer(
        &self,
        avatar: &[u8],
    ) -> Option<crate::proto::content_v1::AttachmentPointer> {
        crate::services::secret_avatar::pointer_for(
            &self.settings_repo,
            &self.secret_attachments(),
            avatar,
        )
        .await
    }

    /// The avatar a member's pointer names; `current` is what we hold for them.
    pub(crate) async fn resolve_member_avatar(
        &self,
        pointer: Option<&crate::proto::content_v1::AttachmentPointer>,
        current: Option<Vec<u8>>,
    ) -> Option<Vec<u8>> {
        crate::services::secret_avatar::resolve(&self.secret_attachments(), pointer, current).await
    }

    /// Our avatar's pointer.
    pub(crate) async fn my_avatar_pointer(
        &self,
    ) -> Option<crate::proto::content_v1::AttachmentPointer> {
        self.avatar_pointer(&self.get_my_avatar()?).await
    }

    /// Create a new anonymous group.
    ///
    /// The caller becomes the owner. Invites are sent to the specified users
    /// with staggered delivery to avoid traffic correlation.
    pub async fn create_group(
        &self,
        name: String,
        invitees: Vec<UserId>,
    ) -> Result<AnonGroup, AnonGroupError> {
        let current_user = self.current_user_id()?;

        // 1. Create group locally
        let group = AnonGroup::new_as_owner(name, current_user);
        self.anon_group_repo.insert_group(&group)?;

        // 2. Create our mailbox for this group
        let mailbox = self.create_group_mailbox(&group.id).await?;
        self.anon_group_repo.insert_mailbox(&mailbox)?;

        // 3. Add ourselves as owner member with our mailbox key and profile
        let profile_key = self.get_my_profile_key();
        let my_display_name = self.get_my_display_name();
        let my_avatar = self.get_my_avatar();
        let my_member = AnonGroupMember::with_mailbox(
            group.id,
            current_user,
            AnonGroupRole::Owner,
            mailbox.public_key,
        )
        .with_profile(profile_key, my_display_name, my_avatar);
        self.anon_group_repo.add_member(&my_member)?;

        // 4. Send invites with staggered delivery (0-30 seconds random delay)
        let invitee_count = invitees.len();
        for invitee in invitees {
            // Add as pending member
            let member = AnonGroupMember::new(group.id, invitee, AnonGroupRole::Member);
            if !self.anon_group_repo.invite_ordered_member(
                &member,
                &current_user,
                TimestampNanos::now().as_u64(),
            )? {
                continue;
            }

            // Send invite with small random delay to reduce traffic correlation
            // Keep it short (100-500ms) to maintain good UX
            let delay_ms = 100 + (rand::rng().next_u32() % 400);
            sleep(Duration::from_millis(delay_ms as u64)).await;

            let send_result = self
                .send_invite(invitee, group.id, &group.name, &mailbox.public_key)
                .await;

            if let Err(e) = send_result {
                tracing::error!("Failed to send invite to: {}", e);
            }
        }

        tracing::info!("Created anonymous group with {} invites", invitee_count);

        Ok(group)
    }

    /// Create a new anonymous group with specified roles for invitees.
    ///
    /// The caller becomes the owner. Each invitee is assigned the specified role
    /// (Admin or Member). Invites are sent with staggered delivery.
    pub async fn create_group_with_roles(
        &self,
        name: String,
        invitees: Vec<(UserId, AnonGroupRole)>,
    ) -> Result<AnonGroup, AnonGroupError> {
        let current_user = self.current_user_id()?;

        // 1. Create group locally
        let group = AnonGroup::new_as_owner(name, current_user);
        self.anon_group_repo.insert_group(&group)?;

        // 2. Create our mailbox for this group
        let mailbox = self.create_group_mailbox(&group.id).await?;
        self.anon_group_repo.insert_mailbox(&mailbox)?;

        // 3. Add ourselves as owner member with our mailbox key and profile
        let profile_key = self.get_my_profile_key();
        let my_display_name = self.get_my_display_name();
        let my_avatar = self.get_my_avatar();
        let my_member = AnonGroupMember::with_mailbox(
            group.id,
            current_user,
            AnonGroupRole::Owner,
            mailbox.public_key,
        )
        .with_profile(profile_key, my_display_name, my_avatar);
        self.anon_group_repo.add_member(&my_member)?;

        // 4. Send invites with staggered delivery
        let invitee_count = invitees.len();
        for (invitee, role) in invitees {
            // Add as pending member with specified role
            let member = AnonGroupMember::new(group.id, invitee, role);
            if !self.anon_group_repo.invite_ordered_member(
                &member,
                &current_user,
                TimestampNanos::now().as_u64(),
            )? {
                continue;
            }

            // Send invite with small random delay to reduce traffic correlation
            let delay_ms = 100 + (rand::rng().next_u32() % 400);
            sleep(Duration::from_millis(delay_ms as u64)).await;

            let send_result = self
                .send_invite(invitee, group.id, &group.name, &mailbox.public_key)
                .await;

            if let Err(e) = send_result {
                tracing::error!("Failed to send invite to: {}", e);
            }
        }

        tracing::info!(
            "Created anonymous group with {} invites (with roles)",
            invitee_count
        );

        Ok(group)
    }

    /// Create a mailbox for a group using blind signature tokens.
    pub(crate) async fn create_group_mailbox(
        &self,
        group_id: &AnonGroupId,
    ) -> Result<AnonGroupMailbox, AnonGroupError> {
        // Over Tor or not at all; refuse before the token is spent.
        self.grpc_pool
            .secret_channel()
            .map_err(|e| AnonGroupError::MessageError(e.to_string()))?;
        // Atomically claim a blind-signed token (marks used in same SQL stmt)
        let token = self.claim_token()?;

        // Use the token's Ed25519 keypair for the mailbox
        let public_key: [u8; 32] =
            token.public_key.clone().try_into().map_err(|_| {
                AnonGroupError::InvalidData("Invalid public key length".to_string())
            })?;
        let private_key: [u8; 64] =
            token.private_key.clone().try_into().map_err(|_| {
                AnonGroupError::InvalidData("Invalid private key length".to_string())
            })?;

        // Register the mailbox with the server using the blind signature (ANONYMOUS - no auth headers)
        // Retry on transient transport errors with channel disconnect to avoid stale HTTP/2 connections
        let ttl_seconds = DEFAULT_GROUP_MAILBOX_TTL_DAYS as u64 * 24 * 60 * 60;
        let mut last_err = None;
        let mut response_result = None;
        for attempt in 0..3u32 {
            if attempt > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(200 * (attempt as u64 + 1)))
                    .await;
                tracing::info!(
                    attempt = attempt + 1,
                    "Retrying create_ephemeral_mailbox for group"
                );
            }
            let mut mailbox_client = self
                .grpc_pool
                .secret_mailbox_client()
                .map_err(|e| AnonGroupError::MessageError(e.to_string()))?;
            match mailbox_client
                .create_ephemeral_mailbox(
                    token.public_key.clone(),
                    token.signature.clone(),
                    ttl_seconds,
                )
                .await
            {
                Ok(resp) => {
                    response_result = Some(resp);
                    break;
                }
                Err(e) if e.is_retryable() => {
                    tracing::warn!(attempt = attempt + 1, error = %e, "create_ephemeral_mailbox transient error, disconnecting stale channel and retrying");
                    self.grpc_pool.disconnect_secret();
                    last_err = Some(e);
                }
                Err(e) => {
                    return Err(AnonGroupError::MessageError(format!(
                        "Failed to create mailbox on server: {}",
                        e
                    )));
                }
            }
        }
        let response = response_result.ok_or_else(|| {
            AnonGroupError::MessageError(format!(
                "Failed to create mailbox on server after 3 attempts: {}",
                last_err.map(|e| e.to_string()).unwrap_or_default()
            ))
        })?;

        tracing::info!(
            mailbox_id = %response.mailbox_id,
            expires_at = response.expires_at,
            "Created ephemeral mailbox on server for anonymous group"
        );

        let now = Utc::now();
        let mailbox = AnonGroupMailbox {
            id: Uuid::new_v4().to_string(),
            group_id: *group_id,
            public_key,
            private_key,
            token_id: token.id.clone(),
            created_at: now,
            expires_at: now + chrono::Duration::days(DEFAULT_GROUP_MAILBOX_TTL_DAYS),
        };

        Ok(mailbox)
    }

    /// Accept an invite and join a group.
    ///
    /// Creates our mailbox, stores the group, and sends JoinAck to the inviter.
    pub async fn accept_invite(
        &self,
        group_id: AnonGroupId,
        group_name: String,
        owner_id: UserId,
        inviter_id: UserId,
        // The device the invite was sealed from (sender certificate): it owns
        // `inviter_mailbox_pk`, so the group binds the inviter to it.
        inviter_device_id: u32,
        inviter_mailbox_pk: [u8; 32],
        inviter_profile_key: Option<[u8; 32]>,
        inviter_display_name: Option<String>,
        inviter_avatar: Option<Vec<u8>>,
    ) -> Result<AnonGroup, AnonGroupError> {
        let current_user = self.current_user_id()?;

        // A repeated authenticated owner invitation repairs a lost JoinAck using
        // the existing mailbox. It must never recreate membership or rotate keys.
        if let Some(existing) = self.anon_group_repo.get_group(&group_id)? {
            if existing.is_blocked() || existing.owner_id != owner_id || inviter_id != owner_id {
                return Err(AnonGroupError::InvalidData(
                    "Invitation does not match existing group owner".into(),
                ));
            }
            let own = self
                .anon_group_repo
                .get_member(&group_id, &current_user)?
                .ok_or(AnonGroupError::MemberNotFound(current_user))?;
            let owner = self
                .anon_group_repo
                .get_member(&group_id, &owner_id)?
                .ok_or(AnonGroupError::MemberNotFound(owner_id))?;
            let mailbox = self
                .anon_group_repo
                .get_mailbox(&group_id)?
                .ok_or_else(|| {
                    AnonGroupError::InvalidData("Existing group mailbox unavailable".into())
                })?;
            if mailbox.is_expired()
                || own.mailbox_pk != Some(mailbox.public_key)
                || owner.mailbox_pk != Some(inviter_mailbox_pk)
            {
                return Err(AnonGroupError::InvalidData(
                    "Invitation mailbox does not match current membership".into(),
                ));
            }
            self.send_join_ack(
                &owner_id,
                group_id,
                &inviter_mailbox_pk,
                &mailbox.public_key,
            )
            .await?;
            return Ok(existing);
        }

        // 1. Create group locally as member
        let group = AnonGroup::from_invite(group_id, group_name, owner_id);
        self.anon_group_repo.insert_group(&group)?;

        // 2. Create our mailbox for this group
        let mailbox = self.create_group_mailbox(&group_id).await?;
        self.anon_group_repo.insert_mailbox(&mailbox)?;

        // 3. Add ourselves as member with our profile
        let profile_key = self.get_my_profile_key();
        let my_display_name = self.get_my_display_name();
        let my_avatar = self.get_my_avatar();
        let my_member = AnonGroupMember::with_mailbox(
            group_id,
            current_user,
            AnonGroupRole::Member,
            mailbox.public_key,
        )
        .with_profile(profile_key, my_display_name, my_avatar);
        self.anon_group_repo
            .accept_ordered_self(&my_member, TimestampNanos::now().as_u64())?;

        // 4. Add inviter with their mailbox key and profile.
        //
        // SECURITY: default the inviter to `Member`, NOT `Owner`. The invite
        // envelope is sealed-sender-authenticated as coming from `inviter_id`
        // but carries no proof that this user owns the group. Trusting the
        // invite to assert ownership lets any group member relay an invite
        // and be recorded as Owner until the next authoritative member-list
        // broadcast overwrites the row. During that window, role-gated
        // local actions (UI controls, kick/ban prompts) trust the wrong
        // actor. Member is the safe lower bound; the next member-list
        // signal upgrades to Owner / Admin if applicable.
        let inviter_member = AnonGroupMember {
            device_id: Some(inviter_device_id),
            ..AnonGroupMember::with_mailbox(
                group_id,
                inviter_id,
                AnonGroupRole::Member,
                inviter_mailbox_pk,
            )
            .with_profile(inviter_profile_key, inviter_display_name, inviter_avatar)
        };
        self.anon_group_repo.add_member(&inviter_member)?;

        // 5. Send JoinAck to inviter's anonymous mailbox
        self.send_join_ack(
            &inviter_id,
            group_id,
            &inviter_mailbox_pk,
            &mailbox.public_key,
        )
        .await?;

        tracing::info!("Joined secret group");
        Ok(group)
    }

    /// Refresh (rotate) our mailbox for a specific anonymous group.
    ///
    /// Creates a fresh mailbox on the server using a new blind-signed token,
    /// persists it in place of the existing one (`anon_group_mailboxes` enforces
    /// `UNIQUE(group_id)`), updates our own entry in
    /// `anon_group_members.mailbox_pk`, and announces the new public key to
    /// group peers so they route future messages to the new address.
    ///
    /// # Announcement mechanism
    ///
    /// We reuse existing group-control messages rather than introducing a new
    /// proto type:
    ///
    /// - **Owner**: re-broadcasts the authoritative member list via
    ///   [`broadcast_member_list`](AnonGroupService::broadcast_member_list).
    ///   Our updated entry (with the new `mailbox_pk`) is included, so every
    ///   confirmed peer will overwrite our stored mailbox key when they apply
    ///   the list.
    /// - **Non-owner**: sends a fresh `AnonGroupJoinAck` (which already carries
    ///   `joiner_mailbox_pk`) to the owner. The owner's existing
    ///   [`handle_join_ack`](AnonGroupService::handle_join_ack) handler updates
    ///   our member row and re-broadcasts the full member list to the rest of
    ///   the group, so all peers eventually learn our new address.
    ///
    /// # Grace period
    ///
    /// Because the local schema only stores a single mailbox per group
    /// (`anon_group_mailboxes.group_id UNIQUE`), we cannot persist the old
    /// mailbox alongside the new one. Instead, callers should invoke this
    /// proactively well before expiry (see
    /// [`refresh_expiring_group_mailboxes`](AnonGroupService::refresh_expiring_group_mailboxes)
    /// which defaults to `GROUP_MAILBOX_REFRESH_LEAD_DAYS` days of lead time).
    /// The old mailbox continues to exist server-side until its TTL elapses,
    /// so late-arriving in-flight messages are preserved; peers will, however,
    /// only route to the new address once they process the announcement above.
    pub async fn refresh_group_mailbox(
        &self,
        group_id: &AnonGroupId,
    ) -> Result<AnonGroupMailbox, AnonGroupError> {
        let current_user = self.current_user_id()?;

        // 1. Verify the group still exists and we're a member.
        let group = self
            .anon_group_repo
            .get_group(group_id)?
            .ok_or(AnonGroupError::GroupNotFound(*group_id))?;

        // Refuse to refresh if the group is blocked — no legitimate use case
        // and it would leak a control message to a blocked party.
        if group.is_blocked() {
            return Err(AnonGroupError::GroupBlocked(*group_id));
        }

        let old_mailbox = self.anon_group_repo.get_mailbox(group_id)?;

        // 2. Create a fresh mailbox on the server (new blind-signed token,
        //    new Ed25519 keypair, fresh TTL). Reuses the same flow as initial
        //    creation, including retry-on-transient-error.
        let new_mailbox = self.create_group_mailbox(group_id).await?;

        // 3. Persist the new mailbox (INSERT OR REPLACE on group_id).
        self.anon_group_repo.insert_mailbox(&new_mailbox)?;

        // 4. Update our own row in the member roster so local sends/fanout
        //    use the new mailbox address, and so the owner's subsequent
        //    `broadcast_member_list` carries our fresh key.
        if let Err(e) = self.anon_group_repo.update_member_mailbox(
            group_id,
            &current_user,
            &new_mailbox.public_key,
        ) {
            // Self-row may not exist if the group is in a weird state; log but
            // keep going — the broadcast below still rotates on the peer side.
            tracing::warn!(
                error = %e,
                "Failed to update our own mailbox_pk in member roster during refresh"
            );
        }

        tracing::info!(
            mailbox_id = %new_mailbox.id,
            expires_at = %new_mailbox.expires_at.to_rfc3339(),
            old_mailbox_id = ?old_mailbox.as_ref().map(|m| &m.id),
            "[ANON_GROUP_REFRESH] Rotated mailbox; announcing new public key to peers"
        );

        // 5. Announce the new mailbox to peers.
        if group.is_owner() {
            // Owner is authoritative for the member list — a broadcast carries
            // our updated mailbox_pk to every confirmed member.
            if let Err(e) = self.broadcast_member_list(group_id).await {
                tracing::warn!(
                    error = %e,
                    "Failed to broadcast member list after mailbox refresh"
                );
            }
        } else {
            // Non-owner: re-ack to the owner with our new mailbox_pk. The
            // owner updates our member row and re-broadcasts the roster to
            // everyone else via the existing handle_join_ack path.
            //
            // We locate the owner's current mailbox_pk from the local member
            // roster. If it's unknown (bootstrap incomplete) we log and skip —
            // peers will still converge when the owner eventually broadcasts.
            let members = self.anon_group_repo.get_members(group_id)?;
            match members
                .iter()
                .find(|m| m.user_id == group.owner_id)
                .and_then(|m| m.mailbox_pk)
            {
                Some(owner_mailbox_pk) => {
                    if let Err(e) = self
                        .send_join_ack(
                            &group.owner_id,
                            *group_id,
                            &owner_mailbox_pk,
                            &new_mailbox.public_key,
                        )
                        .await
                    {
                        tracing::warn!(
                            error = %e,
                            "Failed to send refresh JoinAck to owner"
                        );
                    }
                }
                None => {
                    tracing::warn!("Cannot announce refreshed mailbox: owner mailbox_pk unknown");
                }
            }
        }

        Ok(new_mailbox)
    }

    /// Proactively refresh any anonymous group mailboxes that are close to
    /// expiry.
    ///
    /// Intended to be invoked periodically (e.g. on app start and from a
    /// background tick) by the orchestration layer. Uses
    /// [`GROUP_MAILBOX_REFRESH_LEAD_DAYS`] as the default lead time.
    ///
    /// Errors for individual groups are logged and swallowed so one failing
    /// group does not block refresh of the others. Returns the list of group
    /// IDs that were successfully refreshed.
    pub async fn refresh_expiring_group_mailboxes(
        &self,
    ) -> Result<Vec<AnonGroupId>, AnonGroupError> {
        self.refresh_expiring_group_mailboxes_within(chrono::Duration::days(
            GROUP_MAILBOX_REFRESH_LEAD_DAYS,
        ))
        .await
    }

    /// Refresh any anonymous group mailbox whose `expires_at` is within the
    /// supplied `lead` window of `now`.
    ///
    /// Separated from [`refresh_expiring_group_mailboxes`] so tests and
    /// callers with non-default cadence can pick their own window.
    ///
    /// No re-rotation guard is needed here (unlike the DM/group paths). Those
    /// keep two coexisting in-window rows during overlap, so the expiring-set
    /// query could re-select the lingering old row every cycle and drain a
    /// token per pass. This path is single-row-per-group
    /// (`anon_group_mailboxes.group_id UNIQUE`): `refresh_group_mailbox` does
    /// INSERT OR REPLACE, so after a refresh `get_mailbox` returns only the
    /// fresh row and the `expires_at > threshold` check below skips it. The
    /// guarding invariant is asserted in the v020 migration test.
    pub async fn refresh_expiring_group_mailboxes_within(
        &self,
        lead: chrono::Duration,
    ) -> Result<Vec<AnonGroupId>, AnonGroupError> {
        let groups = self.anon_group_repo.list_groups()?;
        let threshold = Utc::now() + lead;
        let mut refreshed = Vec::new();

        for group in groups {
            // Skip blocked groups — no refresh necessary and avoids leaking
            // control traffic to blocked peers.
            if group.is_blocked() {
                continue;
            }

            let mailbox = match self.anon_group_repo.get_mailbox(&group.id)? {
                Some(m) => m,
                // No mailbox yet (e.g. group pending bootstrap); nothing to
                // refresh.
                None => continue,
            };

            if mailbox.expires_at > threshold {
                // Not close enough to expiry to warrant rotation yet.
                continue;
            }

            match self.refresh_group_mailbox(&group.id).await {
                Ok(new_mailbox) => {
                    tracing::info!(
                        new_mailbox_id = %new_mailbox.id,
                        old_expires_at = %mailbox.expires_at.to_rfc3339(),
                        new_expires_at = %new_mailbox.expires_at.to_rfc3339(),
                        "[ANON_GROUP_REFRESH] Auto-refreshed expiring mailbox"
                    );
                    refreshed.push(group.id);
                }
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        "[ANON_GROUP_REFRESH] Failed to refresh expiring mailbox; will retry on next cycle"
                    );
                }
            }
        }

        Ok(refreshed)
    }
}

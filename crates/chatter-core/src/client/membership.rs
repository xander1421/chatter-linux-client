//! Verified channel membership, shaped for the UI (Design B, W5).
//!
//! Everything a screen says about a channel's signed membership log is decided
//! here: the header banner (verification failed / fork / not yet verified),
//! the "public" badge, the per-member "unverified" marker, join links, join
//! requests and server-operated readers. The UI renders these values and
//! never reads the verified state itself.
//!
//! The source of truth is `VerifiedMembershipRepository` through
//! [`MembershipLog::load`](crate::services::channel::membership_log::MembershipLog::load);
//! the send gate that these banners explain is `GroupStateGate::for_distribution`
//! (`services/message/group.rs`).

use std::sync::Arc;

use crate::domain::channel::{Channel, ChannelType, MemberRole};
use crate::domain::ids::{ChannelId, UserId};
use crate::services::channel::verified_state::{
    ChannelRole, JoinMode, MemberStatus, MembershipState, VerificationStatus, VerifiedChannel, VerifiedMembership,
};
use crate::services::channel::ChannelError;

use super::ChatterClient;

/// The badge a channel anyone may join carries, everywhere it is shown.
pub const PUBLIC_BADGE: &str = "Public \u{2014} anyone can join and read";

/// The label a server-operated reader (an AI integration) carries.
pub const SERVER_OPERATED_LABEL: &str = "Server-operated member: can read";

/// Where a channel's membership stands for sender-key distribution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelVerification {
    /// DMs, Saved Messages: no membership log.
    NotApplicable,
    /// A channel with a log, but no verified state on this device yet.
    /// Sending is blocked (the gate fails closed).
    NotYetVerified,
    Ok,
    /// A change failed verification; cleared by the next one that verifies.
    VerificationFailed,
    /// The server showed different changes at one revision. Sending stays
    /// blocked; there is no acknowledgement that lifts it.
    Fork,
}

impl ChannelVerification {
    pub(crate) fn of(applies: bool, verified: Option<&VerifiedChannel>) -> Self {
        if !applies {
            return Self::NotApplicable;
        }
        match verified.map(|v| v.status) {
            None => Self::NotYetVerified,
            Some(VerificationStatus::Ok) => Self::Ok,
            Some(VerificationStatus::VerificationFailed) => Self::VerificationFailed,
            Some(VerificationStatus::Fork) => Self::Fork,
        }
    }

    /// Whether the send gate refuses group sends for this status.
    pub fn blocks_sending(self) -> bool {
        matches!(self, Self::NotYetVerified | Self::VerificationFailed | Self::Fork)
    }
}

/// How loud a membership banner is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BannerSeverity {
    Warning,
    /// A fork: the server is showing members different things.
    Alert,
}

/// The strip under a channel's header while its membership is not verified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MembershipBanner {
    pub severity: BannerSeverity,
    pub title: String,
    /// Shown on "Details": what happened, the revision and the state code a
    /// user can compare with another member.
    pub detail: String,
}

/// What the open channel's header shows about membership.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChannelMembershipHeader {
    pub banner: Option<MembershipBanner>,
    /// [`PUBLIC_BADGE`] for a channel anyone may join, else empty.
    pub public_badge: String,
}

/// A live join link, as listed in Channel Settings. The private key is never
/// stored, so a link can be copied only when it is created.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InviteLinkRow {
    /// Hex of the capability's public key: the handle for revoking it.
    pub id: String,
    pub summary: String,
}

/// A user waiting for approval after joining through a `needs_approval` link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinRequestRow {
    pub user_id: UserId,
    pub display_name: String,
}

/// A reader the server runs (an AI integration attached to the channel).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerOperatedMember {
    pub id: String,
    pub name: String,
    pub label: &'static str,
}

/// Channel Settings' "Access" pane.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChannelAccessView {
    pub public_badge: String,
    pub can_manage_invites: bool,
    pub invite_links: Vec<InviteLinkRow>,
    pub can_approve_joins: bool,
    pub join_requests: Vec<JoinRequestRow>,
    pub server_operated: Vec<ServerOperatedMember>,
}

/// Channels with a membership log (`ChannelService::create_channel` authors
/// Genesis for exactly these).
pub(crate) fn has_membership_log(channel_type: ChannelType) -> bool {
    matches!(channel_type, ChannelType::Public | ChannelType::Private)
}

fn state_code(v: &VerifiedChannel) -> String {
    let hex = hex::encode(v.state_hash);
    // Grouped like a safety number so it can be read aloud.
    hex[..16].as_bytes().chunks(4).map(|c| String::from_utf8_lossy(c).into_owned()).collect::<Vec<_>>().join(" ")
}

/// The banner for a channel's verification status; `None` when sending is
/// not blocked by membership.
pub(crate) fn banner_for(verification: ChannelVerification, verified: Option<&VerifiedChannel>) -> Option<MembershipBanner> {
    let at = |v: &VerifiedChannel| format!("Verified revision {}, state code {}.", v.state.revision, state_code(v));
    match verification {
        ChannelVerification::NotApplicable | ChannelVerification::Ok => None,
        ChannelVerification::NotYetVerified => Some(MembershipBanner {
            severity: BannerSeverity::Warning,
            title: "Membership not verified yet \u{2014} sending is paused".into(),
            detail: "This device has no verified member list for this channel yet, so it does not know who may \
                     receive your messages. Sending resumes once the channel's signed membership log verifies."
                .into(),
        }),
        ChannelVerification::VerificationFailed => Some(MembershipBanner {
            severity: BannerSeverity::Warning,
            title: "Membership could not be verified \u{2014} sending is paused".into(),
            detail: format!(
                "A change to this channel's member list was not signed by someone allowed to make it, or did not \
                 match the list this device holds. It was not applied, and nobody it named receives your \
                 messages. Sending resumes when a later change verifies. {}",
                verified.map(at).unwrap_or_default()
            ),
        }),
        ChannelVerification::Fork => Some(MembershipBanner {
            severity: BannerSeverity::Alert,
            title: "The server is showing members different member lists".into(),
            detail: format!(
                "Another member, or the server's log, has a different member list at a revision this device \
                 already verified. Sending stays paused for this channel. Compare the state code below with \
                 another member over a channel you trust. {}",
                verified.map(at).unwrap_or_default()
            ),
        }),
    }
}

/// Whether the channel is open to anyone: the verified policy when there is
/// one (a password channel is INVITE_ONLY even though it is public), else the
/// channel type.
pub(crate) fn public_badge(channel_type: ChannelType, verified: Option<&VerifiedChannel>) -> String {
    let open = match verified {
        Some(v) => v.state.policy.join == JoinMode::Open,
        None => channel_type == ChannelType::Public,
    };
    if open { PUBLIC_BADGE.to_string() } else { String::new() }
}

/// Whether a roster row (from the server's member list) is outside the
/// verified distribution set: listed, but nobody signed them in, or the
/// server alone removed them. Always false for a channel with no log.
pub(crate) fn is_unverified_member(applies: bool, verified: Option<&VerifiedChannel>, user_id: &UserId) -> bool {
    applies && !verified.is_some_and(|v| v.contains(&user_id.to_string()))
}

fn my_role(state: &MembershipState, me: &str) -> Option<ChannelRole> {
    state.members.get(me).filter(|m| m.status == MemberStatus::Active).map(|m| m.role)
}

fn allows(mine: Option<ChannelRole>, needed: ChannelRole) -> bool {
    mine.is_some_and(|r| r as i32 >= needed as i32)
}

fn role_label(role: ChannelRole) -> &'static str {
    match role {
        ChannelRole::Owner => "Owner",
        ChannelRole::Admin => "Admin",
        ChannelRole::Moderator => "Moderator",
        _ => "Member",
    }
}

/// The live join links of `state`, in canonical order. `now_nanos` drops
/// expired and used-up ones (they no longer admit anyone).
pub(crate) fn invite_rows(state: &MembershipState, now_nanos: u64) -> Vec<InviteLinkRow> {
    state
        .invites
        .iter()
        .filter(|(_, i)| i.expires_at == 0 || i.expires_at > now_nanos)
        .filter(|(_, i)| i.max_uses == 0 || i.uses < i.max_uses)
        .map(|(key, i)| {
            let mut parts = vec![format!("{} link", role_label(i.role))];
            parts.push(if i.expires_at == 0 {
                "no expiry".to_string()
            } else {
                let at = chrono::DateTime::from_timestamp_nanos(i.expires_at as i64);
                format!("expires {}", at.with_timezone(&chrono::Local).format("%b %d, %Y %H:%M"))
            });
            parts.push(if i.max_uses == 0 {
                format!("{} used", i.uses)
            } else {
                format!("{} of {} uses", i.uses, i.max_uses)
            });
            if i.needs_approval {
                parts.push("needs approval".into());
            }
            InviteLinkRow { id: hex::encode(key), summary: parts.join(" \u{b7} ") }
        })
        .collect()
}

/// Users in REQUESTING status, in canonical order.
pub(crate) fn requesting_users(state: &MembershipState) -> Vec<UserId> {
    state
        .members
        .iter()
        .filter(|(_, m)| m.status == MemberStatus::Requesting)
        .filter_map(|(id, _)| id.parse().ok())
        .collect()
}

/// The access-pane flags that come from the verified state alone.
pub(crate) fn access_flags(state: Option<&MembershipState>, me: &str) -> (bool, bool) {
    let Some(state) = state else { return (false, false) };
    let mine = my_role(state, me);
    (allows(mine, state.policy.manage_invites), allows(mine, state.policy.add_member))
}

fn now_nanos() -> u64 {
    chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0).max(0) as u64
}

fn parse_hex_key(id: &str) -> Result<Vec<u8>, ChannelError> {
    hex::decode(id).map_err(|_| ChannelError::InvalidInput("not an invite link id".into()))
}

impl ChatterClient {
    fn verified_channel(&self, channel_id: &ChannelId) -> Option<VerifiedChannel> {
        self.channels()
            .membership_log()
            .load(channel_id)
            .inspect_err(|e| tracing::warn!(%channel_id, error = %e, "Could not load verified membership"))
            .ok()
            .flatten()
    }

    fn channel_with_log(&self, channel_id: &ChannelId) -> Option<Channel> {
        self.get_channel_local(channel_id).filter(|c| has_membership_log(c.channel_type))
    }

    /// The channel's membership verification status.
    pub fn channel_verification(&self, channel_id: &ChannelId) -> ChannelVerification {
        let applies = self.channel_with_log(channel_id).is_some();
        ChannelVerification::of(applies, self.verified_channel(channel_id).as_ref())
    }

    /// The membership banner and public badge for the open channel. Local
    /// only (no network), so it can run on every conversation switch.
    pub fn channel_membership_header(&self, channel_id: &ChannelId) -> ChannelMembershipHeader {
        let Some(channel) = self.channel_with_log(channel_id) else { return ChannelMembershipHeader::default() };
        let verified = self.verified_channel(channel_id);
        ChannelMembershipHeader {
            banner: banner_for(ChannelVerification::of(true, verified.as_ref()), verified.as_ref()),
            public_badge: public_badge(channel.channel_type, verified.as_ref()),
        }
    }

    /// Whether `user_id`, listed by the server, is outside the verified
    /// distribution set (the member list's "unverified" marker).
    pub fn is_unverified_channel_member(&self, channel_id: &ChannelId, user_id: &UserId) -> bool {
        let applies = self.channel_with_log(channel_id).is_some();
        is_unverified_member(applies, self.verified_channel(channel_id).as_ref(), user_id)
    }

    /// Channel Settings' "Access" pane: links, join requests, the public
    /// badge and server-operated readers. One RPC (channel integrations);
    /// when it fails the list of server-operated readers is empty and logged.
    pub async fn channel_access_view(&self, channel_id: &ChannelId) -> ChannelAccessView {
        let Some(channel) = self.channel_with_log(channel_id) else { return ChannelAccessView::default() };
        let verified = self.verified_channel(channel_id);
        let state = verified.as_ref().map(|v| &v.state);
        let me = self.session_current_user_id().map(|u| u.to_string()).unwrap_or_default();
        let (can_manage_invites, can_approve_joins) = access_flags(state, &me);

        // AI integrations are not channel members: the server attaches them
        // through `channel_integrations` + a SpiceDB `bot_user` tuple
        // (`internal/services/integration/service/service.go`), so they never
        // appear in the signed log or the roster. List them here so nobody
        // mistakes a server-run reader for "no one else".
        let server_operated = match self.ai().get_channel_integrations(&channel_id.to_string()).await {
            Ok(list) => list
                .into_iter()
                .map(|i| ServerOperatedMember { id: i.id, name: i.name, label: SERVER_OPERATED_LABEL })
                .collect(),
            Err(e) => {
                tracing::debug!(%channel_id, error = %e, "Could not list channel integrations");
                Vec::new()
            }
        };

        ChannelAccessView {
            public_badge: public_badge(channel.channel_type, verified.as_ref()),
            can_manage_invites,
            invite_links: if can_manage_invites { state.map(|s| invite_rows(s, now_nanos())).unwrap_or_default() } else { Vec::new() },
            can_approve_joins,
            join_requests: if can_approve_joins {
                state
                    .map(requesting_users)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|user_id| JoinRequestRow { display_name: self.display_name(&user_id), user_id })
                    .collect()
            } else {
                Vec::new()
            },
            server_operated,
        }
    }

    /// Create a join link and return it as `chatter://join/<channel>#<key>`.
    /// The key exists only in the returned string; copy it now.
    pub async fn create_channel_invite_link(
        &self,
        channel_id: &ChannelId,
        needs_approval: bool,
    ) -> Result<String, ChannelError> {
        let link = self
            .channels()
            .create_invite_link(channel_id, MemberRole::Member, None, 0, needs_approval)
            .await?;
        Ok(link.to_url())
    }

    /// Revoke a join link by its [`InviteLinkRow::id`].
    pub async fn revoke_channel_invite_link(&self, channel_id: &ChannelId, link_id: &str) -> Result<(), ChannelError> {
        let key = parse_hex_key(link_id)?;
        self.channels().revoke_invite_link(channel_id, &key).await
    }

    /// Whether `text` is a channel join link (the join box accepts both a
    /// link and a bare channel id).
    pub fn is_channel_join_link(text: &str) -> bool {
        crate::services::channel::membership_log::InviteLink::parse(text).is_some()
    }

    /// Join through a pasted join link, then set up group encryption as
    /// [`Self::join_channel`] does.
    pub async fn join_channel_via_link(self: &Arc<Self>, url: &str) -> Result<Channel, ChannelError> {
        let channel = self.channels().join_via_link(url).await?;
        self.resubscribe_channel_messages();
        if let Err(e) = self.messages().request_skdms_from_channel_members(&channel.id).await {
            tracing::warn!(channel_id = %channel.id, error = %e, "Failed to request SKDMs on join");
        }
        Ok(channel)
    }

    /// Join from the join box: a link, or a bare channel id.
    pub async fn join_channel_from_input(self: &Arc<Self>, input: &str) -> Result<Channel, ChannelError> {
        let input = input.trim();
        if Self::is_channel_join_link(input) {
            return self.join_channel_via_link(input).await;
        }
        let channel_id: ChannelId =
            input.parse().map_err(|_| ChannelError::InvalidInput("not a channel id or invite link".into()))?;
        self.join_channel(&channel_id, None).await
    }

    /// Admit a REQUESTING joiner (signed `AddMember`).
    pub async fn approve_channel_join_request(&self, channel_id: &ChannelId, user_id: &UserId) -> Result<(), ChannelError> {
        self.channels().approve_join_request(channel_id, user_id).await
    }

    /// Turn a REQUESTING joiner away (signed `RemoveMember`).
    pub async fn reject_channel_join_request(&self, channel_id: &ChannelId, user_id: &UserId) -> Result<(), ChannelError> {
        self.channels().remove_pending_member(channel_id, user_id).await
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::*;
    use crate::services::channel::verified_state::{Invite, Member, Policy};

    const OWNER: &str = "00000000-0000-4000-8000-000000000001";
    const ADMIN: &str = "00000000-0000-4000-8000-000000000002";
    const MEMBER: &str = "00000000-0000-4000-8000-000000000003";
    const ASKING: &str = "00000000-0000-4000-8000-000000000004";
    const GONE: &str = "00000000-0000-4000-8000-000000000005";

    fn member(role: ChannelRole, status: MemberStatus) -> Member {
        Member { role, status, identity_key: [5; 33] }
    }

    fn channel(status: VerificationStatus, public: bool) -> VerifiedChannel {
        let members = BTreeMap::from([
            (OWNER.to_string(), member(ChannelRole::Owner, MemberStatus::Active)),
            (ADMIN.to_string(), member(ChannelRole::Admin, MemberStatus::Active)),
            (MEMBER.to_string(), member(ChannelRole::Member, MemberStatus::Active)),
            (ASKING.to_string(), member(ChannelRole::Member, MemberStatus::Requesting)),
            (GONE.to_string(), member(ChannelRole::Member, MemberStatus::Active)),
        ]);
        let invites = BTreeMap::from([
            ([1u8; 33], Invite { role: ChannelRole::Member, expires_at: 0, max_uses: 0, uses: 2, needs_approval: true }),
            ([2u8; 33], Invite { role: ChannelRole::Member, expires_at: 10, max_uses: 0, uses: 0, needs_approval: false }),
            ([3u8; 33], Invite { role: ChannelRole::Member, expires_at: 0, max_uses: 3, uses: 3, needs_approval: false }),
            ([4u8; 33], Invite { role: ChannelRole::Moderator, expires_at: 0, max_uses: 5, uses: 1, needs_approval: false }),
        ]);
        let state = MembershipState {
            channel_id: "00000000-0000-4000-8000-0000000000aa".into(),
            revision: 7,
            members,
            policy: Policy::defaults(public),
            invites,
            disappearing_seconds: 0,
        };
        VerifiedChannel { state_hash: [0xab; 32], state, excluded: BTreeSet::from([GONE.to_string()]), status }
    }

    #[test]
    fn verification_status_maps_and_blocks() {
        assert_eq!(ChannelVerification::of(false, None), ChannelVerification::NotApplicable);
        assert_eq!(ChannelVerification::of(true, None), ChannelVerification::NotYetVerified);
        let ok = channel(VerificationStatus::Ok, false);
        assert_eq!(ChannelVerification::of(true, Some(&ok)), ChannelVerification::Ok);
        let failed = channel(VerificationStatus::VerificationFailed, false);
        assert_eq!(ChannelVerification::of(true, Some(&failed)), ChannelVerification::VerificationFailed);
        let fork = channel(VerificationStatus::Fork, false);
        assert_eq!(ChannelVerification::of(true, Some(&fork)), ChannelVerification::Fork);
        // A DM never shows a banner, even with a stray state row.
        assert_eq!(ChannelVerification::of(false, Some(&fork)), ChannelVerification::NotApplicable);

        assert!(!ChannelVerification::Ok.blocks_sending());
        assert!(!ChannelVerification::NotApplicable.blocks_sending());
        for s in [ChannelVerification::NotYetVerified, ChannelVerification::VerificationFailed, ChannelVerification::Fork] {
            assert!(s.blocks_sending());
        }
    }

    #[test]
    fn banners_match_status() {
        assert_eq!(banner_for(ChannelVerification::Ok, None), None);
        assert_eq!(banner_for(ChannelVerification::NotApplicable, None), None);

        let failed = channel(VerificationStatus::VerificationFailed, false);
        let b = banner_for(ChannelVerification::VerificationFailed, Some(&failed)).unwrap();
        assert_eq!(b.severity, BannerSeverity::Warning);
        assert_eq!(b.title, "Membership could not be verified \u{2014} sending is paused");
        assert!(b.detail.contains("revision 7"), "{}", b.detail);
        assert!(b.detail.contains("abab abab abab abab"), "{}", b.detail);

        let fork = channel(VerificationStatus::Fork, false);
        let b = banner_for(ChannelVerification::Fork, Some(&fork)).unwrap();
        assert_eq!(b.severity, BannerSeverity::Alert);
        assert!(b.detail.contains("Sending stays paused"));

        let b = banner_for(ChannelVerification::NotYetVerified, None).unwrap();
        assert!(b.title.contains("sending is paused"));
    }

    #[test]
    fn public_badge_follows_verified_policy() {
        // Verified OPEN policy: badge, whatever the type says.
        assert_eq!(public_badge(ChannelType::Private, Some(&channel(VerificationStatus::Ok, true))), PUBLIC_BADGE);
        // A public channel with a password is INVITE_ONLY: no badge.
        assert_eq!(public_badge(ChannelType::Public, Some(&channel(VerificationStatus::Ok, false))), "");
        // No verified state yet: fall back to the type.
        assert_eq!(public_badge(ChannelType::Public, None), PUBLIC_BADGE);
        assert_eq!(public_badge(ChannelType::Private, None), "");
    }

    #[test]
    fn unverified_member_marker() {
        let v = channel(VerificationStatus::Ok, false);
        let id = |s: &str| -> UserId { s.parse().unwrap() };
        assert!(!is_unverified_member(true, Some(&v), &id(MEMBER)));
        // Server-injected row nobody signed in.
        assert!(is_unverified_member(true, Some(&v), &id("00000000-0000-4000-8000-0000000000ff")));
        // Not yet admitted.
        assert!(is_unverified_member(true, Some(&v), &id(ASKING)));
        // Removed by the server alone: out of distribution.
        assert!(is_unverified_member(true, Some(&v), &id(GONE)));
        // No verified state: nobody is verified.
        assert!(is_unverified_member(true, None, &id(MEMBER)));
        // No log (DM): never marked.
        assert!(!is_unverified_member(false, None, &id(MEMBER)));
    }

    #[test]
    fn invite_rows_hide_dead_links_and_describe_live_ones() {
        let v = channel(VerificationStatus::Ok, false);
        let rows = invite_rows(&v.state, 100);
        let ids: Vec<_> = rows.iter().map(|r| r.id.clone()).collect();
        // [2;33] expired at 10 < now, [3;33] used up.
        assert_eq!(ids, vec![hex::encode([1u8; 33]), hex::encode([4u8; 33])]);
        assert_eq!(rows[0].summary, "Member link \u{b7} no expiry \u{b7} 2 used \u{b7} needs approval");
        assert_eq!(rows[1].summary, "Moderator link \u{b7} no expiry \u{b7} 1 of 5 uses");
    }

    #[test]
    fn access_flags_follow_policy_and_role() {
        let v = channel(VerificationStatus::Ok, false);
        assert_eq!(access_flags(Some(&v.state), OWNER), (true, true));
        assert_eq!(access_flags(Some(&v.state), ADMIN), (true, true));
        assert_eq!(access_flags(Some(&v.state), MEMBER), (false, false));
        // A requester has no role yet.
        assert_eq!(access_flags(Some(&v.state), ASKING), (false, false));
        assert_eq!(access_flags(None, OWNER), (false, false));
        assert_eq!(requesting_users(&v.state), vec![ASKING.parse::<UserId>().unwrap()]);
    }

    #[test]
    fn join_link_detection() {
        let id = ChannelId::from(uuid::Uuid::new_v4());
        let url = crate::services::channel::membership_log::InviteLink::generate(id).to_url();
        assert!(ChatterClient::is_channel_join_link(&url));
        assert!(!ChatterClient::is_channel_join_link(&id.to_string()));
        assert!(!ChatterClient::is_channel_join_link("chatter://join/nonsense"));
    }

    #[test]
    fn has_log_only_for_channels() {
        assert!(has_membership_log(ChannelType::Public));
        assert!(has_membership_log(ChannelType::Private));
        assert!(!has_membership_log(ChannelType::DirectMessage));
        assert!(!has_membership_log(ChannelType::SavedMessages));
    }
}

//! Where AI is allowed to run, and on what.
//!
//! Three rules, enforced in one place so every call site inherits them:
//!
//!   1. Mesh-delivered traffic gets no AI at all. Mesh is the serverless
//!      transport; a peer's message must never leave the device to a cloud
//!      provider, and running a model over it is not something the sender
//!      can consent to.
//!   2. DMs — every flavour, including anonymous ones and Saved Messages —
//!      get local models only. Nothing 1:1 goes to a third party.
//!   3. Named channels may use a local model or a cloud provider, per the
//!      channel's own configuration.
//!
//! The match is deliberately fail-closed: a `ConversationType` added later
//! lands in the `_` arm and is treated as a DM (local only), never as a
//! channel. Widening access has to be a deliberate edit to this file.

use crate::domain::message::ConversationType;

/// Which silo an engine belongs to.
///
/// The two AI silos are [`crate::local_ai`] (on-device) and
/// [`crate::cloud_ai`] (off-device). Policy is stated in terms of silos, not
/// concrete providers, so adding a provider to either silo cannot change what
/// a DM is allowed to reach.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Silo {
    /// On-device or self-hosted: content never leaves the machine.
    Local,
    /// Off-device: content is sent to a third party.
    Cloud,
}

/// What inference is permitted for a given conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AiAccess {
    /// No inference at all.
    Denied,
    /// On-device / self-hosted providers only — nothing leaves the machine.
    LocalOnly,
    /// Local or a configured cloud provider.
    LocalOrCloud,
}

impl AiAccess {
    /// Whether this access level permits a given silo.
    pub fn permits_silo(self, silo: Silo) -> bool {
        self.permits_cloud(silo == Silo::Cloud)
    }

    /// Whether an engine that does (or doesn't) leave the device is permitted.
    ///
    /// For call sites holding a constructed backend rather than a silo value.
    pub fn permits_cloud(self, is_cloud: bool) -> bool {
        match self {
            Self::Denied => false,
            Self::LocalOnly => !is_cloud,
            Self::LocalOrCloud => true,
        }
    }
}

/// Resolve the access level for a conversation.
///
/// `delivered_via_mesh` is whether the triggering message arrived over the
/// mesh transport rather than the server.
pub fn ai_access(conversation: &ConversationType, delivered_via_mesh: bool) -> AiAccess {
    if delivered_via_mesh {
        return AiAccess::Denied;
    }

    match conversation {
        ConversationType::Channel => AiAccess::LocalOrCloud,
        // Every DM flavour, anon transport, and the personal vault: local only.
        _ => AiAccess::LocalOnly,
    }
}

/// The workspace an AI turn is confined to.
///
/// Context assembly must be scoped through one of these. Holding the channel
/// id in a distinct type makes "which channel is this context for" impossible
/// to leave unanswered at a call site, and gives the file-scoping check below
/// one place to live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiWorkspace {
    channel_id: String,
    access: AiAccess,
}

impl AiWorkspace {
    /// Open a workspace for a conversation, or `None` if AI is denied here.
    pub fn open(
        channel_id: impl Into<String>,
        conversation: &ConversationType,
        delivered_via_mesh: bool,
    ) -> Option<Self> {
        let access = ai_access(conversation, delivered_via_mesh);
        if access == AiAccess::Denied {
            return None;
        }
        Some(Self {
            channel_id: channel_id.into(),
            access,
        })
    }

    pub fn channel_id(&self) -> &str {
        &self.channel_id
    }

    pub fn access(&self) -> AiAccess {
        self.access
    }

    /// Whether a file may be pulled into this workspace's context.
    ///
    /// This is the cross-channel boundary: a file is only in scope when it
    /// belongs to *this* channel. The server-side equivalent of this check is
    /// what `context_builder.rs::fetchDirectFileContent` is missing — it
    /// fetches any file id the caller can read, including files from other
    /// channels the same user belongs to.
    pub fn allows_file(&self, file_channel_id: Option<&str>) -> bool {
        match file_channel_id {
            Some(id) => id == self.channel_id,
            // Unknown provenance is not in scope. Fail closed.
            None => false,
        }
    }

    /// Filter a set of candidate files down to the ones in scope.
    pub fn scope_files<'a, T, F>(&self, files: &'a [T], channel_of: F) -> Vec<&'a T>
    where
        F: Fn(&T) -> Option<&'a str>,
    {
        files
            .iter()
            .filter(|f| self.allows_file(channel_of(f)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mesh_denies_everything_including_channels() {
        for conv in &[
            ConversationType::Channel,
            ConversationType::ServerDm,
            ConversationType::AnonDm,
            ConversationType::AnonGroup,
        ] {
            assert_eq!(
                ai_access(conv, true),
                AiAccess::Denied,
                "{conv:?} over mesh"
            );
        }
    }

    #[test]
    fn dms_are_local_only_and_channels_may_use_cloud() {
        assert_eq!(
            ai_access(&ConversationType::Channel, false),
            AiAccess::LocalOrCloud
        );

        for conv in &[
            ConversationType::ServerDm,
            ConversationType::AnonDm,
            ConversationType::AnonGroup,
            ConversationType::SavedMessages,
        ] {
            assert_eq!(ai_access(conv, false), AiAccess::LocalOnly, "{conv:?}");
        }
    }

    #[test]
    fn local_only_rejects_the_cloud_silo_and_admits_the_local_one() {
        let dm = ai_access(&ConversationType::ServerDm, false);
        assert!(
            !dm.permits_silo(Silo::Cloud),
            "cloud silo reachable from a DM"
        );
        assert!(dm.permits_silo(Silo::Local));

        let channel = ai_access(&ConversationType::Channel, false);
        assert!(channel.permits_silo(Silo::Cloud));
        assert!(channel.permits_silo(Silo::Local));

        assert!(!AiAccess::Denied.permits_silo(Silo::Local));
        assert!(!AiAccess::Denied.permits_silo(Silo::Cloud));
    }

    #[test]
    fn mesh_conversation_opens_no_workspace() {
        assert!(AiWorkspace::open("c1", &ConversationType::Channel, true).is_none());
        assert!(AiWorkspace::open("c1", &ConversationType::Channel, false).is_some());
    }

    #[test]
    fn workspace_admits_only_its_own_channels_files() {
        let ws = AiWorkspace::open("chan-a", &ConversationType::Channel, false).unwrap();

        assert!(ws.allows_file(Some("chan-a")));
        // The leak this exists to stop: same user, different channel.
        assert!(!ws.allows_file(Some("chan-b")));
        // Unknown provenance is refused rather than assumed in scope.
        assert!(!ws.allows_file(None));
    }

    #[test]
    fn scope_files_drops_out_of_channel_candidates() {
        let ws = AiWorkspace::open("chan-a", &ConversationType::Channel, false).unwrap();
        let files = vec![
            ("in", Some("chan-a")),
            ("other-channel", Some("chan-b")),
            ("orphan", None),
        ];

        let kept = ws.scope_files(&files, |f| f.1);

        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].0, "in");
    }
}

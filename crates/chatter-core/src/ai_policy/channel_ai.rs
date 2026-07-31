//! Per-conversation AI configuration, resolved against policy.
//!
//! Two independent knobs:
//!   * **per-conversation**: is AI on here at all? Stored under the existing
//!     `conv.<setting>.<id>` convention (`conv.ai.enabled.<id>`), alongside
//!     `conv.pinned.*` / `conv.muted.*` / `conv.disappearing.*`. No server
//!     state, no proto field, no migration.
//!   * **user-level**: which silo to prefer when both are allowed.
//!
//! Resolution never names a concrete provider — it picks a *silo*, and each
//! silo picks its own provider from its own settings. That is what keeps the
//! two apart: adding a provider to either silo cannot change this logic.

use super::policy::{AiAccess, Silo};

/// Settings key: is AI enabled for this conversation?
pub fn enabled_key(conversation_id: &str) -> String {
    format!("conv.ai.enabled.{conversation_id}")
}

/// Settings key: the user's silo preference.
pub const SILO_PREFERENCE_KEY: &str = "ai.silo_preference";

/// Which silo the user would rather use, where policy allows a choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SiloPreference {
    /// Never leave the device, even where a cloud provider would be allowed.
    #[default]
    LocalOnly,
    /// Use the cloud silo where permitted, falling back to local otherwise.
    PreferCloud,
}

impl SiloPreference {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LocalOnly => "local-only",
            Self::PreferCloud => "prefer-cloud",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "local-only" => Some(Self::LocalOnly),
            "prefer-cloud" => Some(Self::PreferCloud),
            _ => None,
        }
    }
}

/// What a conversation should actually run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolved {
    /// AI must not run here (switched off, or mesh-delivered).
    Off,
    /// Run in this silo; the silo resolves its own provider.
    Use(Silo),
}

/// Resolve which silo a conversation should run in.
///
/// * `enabled` — the per-conversation switch.
/// * `preference` — the user-level silo preference.
/// * `cloud_available` — whether the cloud silo is actually configured (an
///   API key is present). Preference alone is not enough.
/// * `access` — from [`super::policy::ai_access`].
///
/// Cloud is chosen only when *all three* agree: the user prefers it, policy
/// permits it, and it is configured. Anything short of that falls back to
/// local rather than switching AI off — "DM = local AI" asks for a local
/// model, not for the feature to disappear.
pub fn resolve(
    enabled: bool,
    preference: SiloPreference,
    cloud_available: bool,
    access: AiAccess,
) -> Resolved {
    if !enabled || access == AiAccess::Denied {
        return Resolved::Off;
    }

    let wants_cloud = preference == SiloPreference::PreferCloud;
    if wants_cloud && cloud_available && access.permits_silo(Silo::Cloud) {
        return Resolved::Use(Silo::Cloud);
    }

    if access.permits_silo(Silo::Local) {
        return Resolved::Use(Silo::Local);
    }

    Resolved::Off
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai_policy::policy::ai_access;
    use crate::domain::message::ConversationType;

    const CHANNEL: AiAccess = AiAccess::LocalOrCloud;
    const DM: AiAccess = AiAccess::LocalOnly;

    #[test]
    fn key_follows_the_conv_convention() {
        assert_eq!(enabled_key("abc"), "conv.ai.enabled.abc");
    }

    #[test]
    fn preference_defaults_to_local_only() {
        assert_eq!(SiloPreference::default(), SiloPreference::LocalOnly);
    }

    #[test]
    fn preference_round_trips() {
        for p in [SiloPreference::LocalOnly, SiloPreference::PreferCloud] {
            assert_eq!(SiloPreference::parse(p.as_str()), Some(p));
        }
        assert_eq!(SiloPreference::parse("nonsense"), None);
    }

    #[test]
    fn disabled_conversation_runs_nothing() {
        assert_eq!(
            resolve(false, SiloPreference::PreferCloud, true, CHANNEL),
            Resolved::Off
        );
    }

    #[test]
    fn channel_with_cloud_preferred_and_configured_uses_cloud() {
        assert_eq!(
            resolve(true, SiloPreference::PreferCloud, true, CHANNEL),
            Resolved::Use(Silo::Cloud)
        );
    }

    #[test]
    fn cloud_preference_without_a_key_falls_back_to_local() {
        assert_eq!(
            resolve(true, SiloPreference::PreferCloud, false, CHANNEL),
            Resolved::Use(Silo::Local)
        );
    }

    #[test]
    fn local_preference_stays_local_even_on_a_channel() {
        assert_eq!(
            resolve(true, SiloPreference::LocalOnly, true, CHANNEL),
            Resolved::Use(Silo::Local)
        );
    }

    #[test]
    fn dm_downgrades_a_cloud_preference_to_local() {
        assert_eq!(
            resolve(true, SiloPreference::PreferCloud, true, DM),
            Resolved::Use(Silo::Local)
        );
    }

    #[test]
    fn mesh_is_off_regardless_of_configuration() {
        let mesh = ai_access(&ConversationType::Channel, true);
        assert_eq!(
            resolve(true, SiloPreference::PreferCloud, true, mesh),
            Resolved::Off
        );
    }

    #[test]
    fn no_dm_flavour_ever_resolves_to_the_cloud_silo() {
        // The property that matters, across every DM kind and every config.
        for conv in &[
            ConversationType::ServerDm,
            ConversationType::MailboxDm,
            ConversationType::AnonDm,
            ConversationType::AnonGroup,
            ConversationType::SavedMessages,
        ] {
            let access = ai_access(conv, false);
            for pref in [SiloPreference::LocalOnly, SiloPreference::PreferCloud] {
                for cloud_available in [true, false] {
                    assert_ne!(
                        resolve(true, pref, cloud_available, access),
                        Resolved::Use(Silo::Cloud),
                        "{conv:?} / {pref:?} / cloud_available={cloud_available}"
                    );
                }
            }
        }
    }
}

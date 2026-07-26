//! Per-channel AI configuration.
//!
//! Which channels have AI switched on, and which provider each uses. Stored in
//! the settings repo under the established `conv.<setting>.<id>` convention
//! (alongside `conv.pinned.*`, `conv.muted.*`, `conv.disappearing.*`) — no
//! server state, no proto field, no migration.
//!
//! The resolution rule is the point of this module: a channel's configured
//! provider is a *request*, not a decision. [`super::policy`] clamps it. A
//! channel configured for a cloud provider that later turns out to be a DM
//! gets downgraded to the local default rather than quietly reaching the
//! cloud — matching "in DM channels allow only local AI", which asks for a
//! local model, not for AI to switch off.

use super::policy::AiAccess;
use super::providers::ProviderKind;

/// Settings key: is AI enabled for this conversation?
pub fn enabled_key(conversation_id: &str) -> String {
    format!("conv.ai.enabled.{conversation_id}")
}

/// Settings key: which provider this conversation prefers.
pub fn provider_key(conversation_id: &str) -> String {
    format!("conv.ai.provider.{conversation_id}")
}

/// What a conversation should actually run, after policy is applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolved {
    /// AI must not run here at all (mesh traffic, or switched off).
    Off,
    /// Run on this provider.
    Use(ProviderKind),
    /// Policy permits only local inference, but neither the channel's choice
    /// nor the global default is local — the user has to configure one.
    NeedsLocalProvider,
}

/// Resolve the provider for a conversation.
///
/// * `enabled` — the channel's on/off switch (`conv.ai.enabled.<id>`).
/// * `requested` — the channel's provider override, if any.
/// * `default_provider` — the global `active_provider` from settings.
/// * `access` — from [`super::policy::ai_access`].
pub fn resolve(
    enabled: bool,
    requested: Option<ProviderKind>,
    default_provider: ProviderKind,
    access: AiAccess,
) -> Resolved {
    if !enabled || access == AiAccess::Denied {
        return Resolved::Off;
    }

    let choice = requested.unwrap_or(default_provider);

    if access.permits(choice) {
        return Resolved::Use(choice);
    }

    // Choice is cloud but this conversation is local-only. Fall back to the
    // global default when that is itself local, so a DM still gets a model.
    if access.permits(default_provider) {
        return Resolved::Use(default_provider);
    }

    Resolved::NeedsLocalProvider
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::message::ConversationType;
    use crate::local_ai::policy::ai_access;

    const CHANNEL: AiAccess = AiAccess::LocalOrCloud;
    const DM: AiAccess = AiAccess::LocalOnly;

    #[test]
    fn keys_follow_the_conv_convention() {
        assert_eq!(enabled_key("abc"), "conv.ai.enabled.abc");
        assert_eq!(provider_key("abc"), "conv.ai.provider.abc");
    }

    #[test]
    fn disabled_channel_runs_nothing_even_when_permitted() {
        assert_eq!(
            resolve(false, Some(ProviderKind::Ollama), ProviderKind::Ollama, CHANNEL),
            Resolved::Off
        );
    }

    #[test]
    fn channel_override_wins_over_the_global_default() {
        assert_eq!(
            resolve(true, Some(ProviderKind::Anthropic), ProviderKind::Ollama, CHANNEL),
            Resolved::Use(ProviderKind::Anthropic)
        );
    }

    #[test]
    fn no_override_falls_back_to_the_global_default() {
        assert_eq!(
            resolve(true, None, ProviderKind::Ollama, CHANNEL),
            Resolved::Use(ProviderKind::Ollama)
        );
    }

    #[test]
    fn dm_downgrades_a_cloud_choice_to_the_local_default() {
        // The channel asked for Anthropic; policy says this is a DM.
        assert_eq!(
            resolve(true, Some(ProviderKind::Anthropic), ProviderKind::Ollama, DM),
            Resolved::Use(ProviderKind::Ollama),
        );
    }

    #[test]
    fn dm_with_no_local_option_anywhere_asks_for_configuration() {
        // Both the override and the default are cloud: nothing legal to run.
        assert_eq!(
            resolve(true, Some(ProviderKind::Anthropic), ProviderKind::Anthropic, DM),
            Resolved::NeedsLocalProvider,
        );
    }

    #[test]
    fn mesh_is_off_regardless_of_configuration() {
        let mesh = ai_access(&ConversationType::Channel, true);
        assert_eq!(
            resolve(true, Some(ProviderKind::Ollama), ProviderKind::Ollama, mesh),
            Resolved::Off,
        );
    }

    #[test]
    fn end_to_end_a_dm_never_resolves_to_a_cloud_provider() {
        // The property that matters, over every DM flavour and both config
        // shapes: resolution never hands back a cloud provider.
        for conv in &[
            ConversationType::ServerDm,
            ConversationType::MailboxDm,
            ConversationType::AnonDm,
            ConversationType::AnonGroup,
            ConversationType::SavedMessages,
        ] {
            let access = ai_access(conv, false);
            for requested in [None, Some(ProviderKind::Anthropic), Some(ProviderKind::Ollama)] {
                for default in [ProviderKind::Ollama, ProviderKind::Anthropic] {
                    if let Resolved::Use(p) = resolve(true, requested, default, access) {
                        assert!(!p.is_cloud(), "{conv:?} resolved to cloud {p:?}");
                    }
                }
            }
        }
    }
}

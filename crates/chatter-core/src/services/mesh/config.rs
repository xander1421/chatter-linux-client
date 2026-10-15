//! What the mesh is configured to do.

use chatter_mesh_nostr::relay::DEFAULT_RELAYS;
use std::sync::{Arc, RwLock};

/// Static configuration for [`MeshService`](super::MeshService).
///
/// Everything here is a decision the *service* acts on. Nothing in it is a
/// rendering preference: the UI crate gets shaped data and makes no choices, so
/// a knob that only changes how something looks does not belong here either.
#[derive(Clone, Debug)]
pub struct MeshConfig {
    /// The name broadcast in announces and attached to public messages.
    ///
    /// Unauthenticated by construction on public traffic — anyone can claim any
    /// nickname on a broadcast — so it is a display string, never an identity.
    pub nickname: String,
    /// Current local choice shared with running radio and relay loops.
    live_nickname: Arc<RwLock<Option<String>>>,

    /// Geohash cells to join on start.
    pub geohashes: Vec<String>,

    /// Nostr relays to use. Empty means the built-in default set.
    pub relays: Vec<String>,

    /// Park sealed copies on relays when no courier is physically nearby.
    ///
    /// Off by default: a drop is published to public relays, and while its
    /// envelope is opaque and its recipient tag rotates daily, it is still
    /// traffic leaving the device for a third party. That is a choice the user
    /// makes, not one the default makes for them.
    pub bridge_enabled: bool,

    /// Duty-cycle the scan rather than scanning continuously.
    ///
    /// Scanning is the expensive half of BLE; advertising stays up either way.
    pub scan_duty_cycle: bool,

    /// Advertise a bridge rendezvous cell in announces, so mesh-only peers can
    /// find a device with internet. `None` means we advertise no bridge.
    pub bridge_geohash: Option<String>,
}

impl Default for MeshConfig {
    fn default() -> Self {
        Self {
            nickname: "anon".to_string(),
            live_nickname: Arc::new(RwLock::new(None)),
            geohashes: Vec::new(),
            relays: Vec::new(),
            bridge_enabled: false,
            scan_duty_cycle: true,
            bridge_geohash: None,
        }
    }
}

impl MeshConfig {
    pub fn nickname(&self) -> String {
        self.live_nickname
            .read()
            .unwrap()
            .clone()
            .unwrap_or_else(|| self.nickname.clone())
    }

    pub fn set_nickname(&self, nickname: String) {
        *self.live_nickname.write().unwrap() = Some(nickname);
    }
    /// The relay set to actually use.
    #[must_use]
    pub fn relay_urls(&self) -> Vec<String> {
        if self.relays.is_empty() {
            DEFAULT_RELAYS
                .iter()
                .map(|url| (*url).to_string())
                .collect()
        } else {
            self.relays.clone()
        }
    }

    /// The nickname, trimmed, or `None` when it is blank.
    ///
    /// A blank nickname is omitted from the wire rather than sent empty, so that
    /// clearing it actually anonymises a message instead of publishing an empty
    /// tag that still marks the sender as "someone who cleared their name".
    #[must_use]
    pub fn display_nickname(&self) -> Option<&str> {
        let trimmed = self.nickname.trim();
        (!trimmed.is_empty()).then_some(trimmed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_relay_list_falls_back_to_the_defaults() {
        let config = MeshConfig::default();
        assert_eq!(config.relay_urls().len(), DEFAULT_RELAYS.len());
        assert!(
            config
                .relay_urls()
                .iter()
                .all(|url| url.starts_with("wss://"))
        );
    }

    #[test]
    fn configured_relays_replace_the_defaults_rather_than_adding_to_them() {
        let config = MeshConfig {
            relays: vec!["wss://relay.example".into()],
            ..MeshConfig::default()
        };
        assert_eq!(config.relay_urls(), vec!["wss://relay.example".to_string()]);
    }

    #[test]
    fn a_blank_nickname_is_omitted_rather_than_sent_empty() {
        let mut config = MeshConfig::default();
        config.nickname = "   ".into();
        assert!(config.display_nickname().is_none());
        config.nickname = "  ada  ".into();
        assert_eq!(config.display_nickname(), Some("ada"));
    }

    #[test]
    fn the_relay_bridge_is_off_unless_asked_for() {
        assert!(!MeshConfig::default().bridge_enabled);
    }
}

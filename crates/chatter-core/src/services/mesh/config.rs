//! What the mesh is configured to do.

use chatter_mesh::ids::PeerId;
use chatter_mesh::packet::MessageType;
use chatter_mesh_nostr::relay::DEFAULT_RELAYS;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use super::{BleCommand, MeshError, MeshEvent, MeshService, NostrCommand};

const PAIRING_WINDOW: Duration = Duration::from_secs(120);

#[derive(Debug)]
struct PairingWindow {
    deadline: Instant,
    peer: Option<PeerId>,
}

pub(super) trait PairedAnnounceMatcher {
    type Announce: ?Sized;

    fn paired_contact_for_announce(&self, announce: &Self::Announce) -> Option<PeerId>;
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum ConnectionAdmission {
    Paired(PeerId),
    PublicChat,
    Pairing,
}

#[derive(Debug)]
struct MeshNickname {
    custom: Option<String>,
    automatic: String,
    peer: Option<PeerId>,
}

fn anonymous_nickname() -> String {
    format!("anon-{:016x}", rand::random::<u64>())
}

/// Configuration for [`MeshService`](super::MeshService).
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
    nickname: Arc<RwLock<MeshNickname>>,
    bridge_mode: Arc<AtomicBool>,
    public_chat: Arc<AtomicBool>,
    pairing: Arc<Mutex<Option<PairingWindow>>>,

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
            nickname: Arc::new(RwLock::new(MeshNickname {
                custom: None,
                automatic: anonymous_nickname(),
                peer: None,
            })),
            bridge_mode: Arc::new(AtomicBool::new(false)),
            public_chat: Arc::new(AtomicBool::new(false)),
            pairing: Arc::new(Mutex::new(None)),
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
        let name = self.nickname.read().unwrap();
        name.custom.as_ref().unwrap_or(&name.automatic).clone()
    }

    pub fn custom_nickname(&self) -> Option<String> {
        self.nickname.read().unwrap().custom.clone()
    }

    pub fn set_nickname(&self, nickname: String) {
        let nickname = nickname.trim();
        self.nickname.write().unwrap().custom = (!nickname.is_empty()).then(|| nickname.to_owned());
    }

    /// Refresh the automatic nickname once when a fresh BLE identity takes effect.
    pub fn on_identity_rotated(&self, peer: &PeerId) {
        let mut nickname = self.nickname.write().unwrap();
        if nickname.peer.as_ref() != Some(peer) {
            nickname.automatic = anonymous_nickname();
            nickname.peer = Some(peer.clone());
        }
    }

    pub fn bridge_mode_enabled(&self) -> bool {
        self.bridge_mode.load(Ordering::Relaxed)
    }

    pub fn set_bridge_mode(&self, enabled: bool) {
        self.bridge_mode.store(enabled, Ordering::Relaxed);
    }

    pub fn public_chat_enabled(&self) -> bool {
        self.public_chat.load(Ordering::Relaxed)
    }

    pub fn set_public_chat(&self, enabled: bool) {
        self.public_chat.store(enabled, Ordering::Relaxed);
    }

    pub(super) fn pairing_active(&self, now: Instant) -> bool {
        self.pairing
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|window| now < window.deadline)
    }

    pub(super) fn set_pairing(&self, enabled: bool, now: Instant) {
        *self.pairing.lock().unwrap() = enabled.then(|| PairingWindow {
            deadline: now + PAIRING_WINDOW,
            peer: None,
        });
    }

    pub(super) fn expire_pairing(&self, now: Instant) -> bool {
        let mut pairing = self.pairing.lock().unwrap();
        if pairing
            .as_ref()
            .is_some_and(|window| now >= window.deadline)
        {
            *pairing = None;
            true
        } else {
            false
        }
    }

    pub(super) fn select_pairing_peer(&self, peer: PeerId, now: Instant) {
        if let Some(window) = self
            .pairing
            .lock()
            .unwrap()
            .as_mut()
            .filter(|window| now < window.deadline)
        {
            window.peer = Some(peer);
        }
    }

    pub(super) fn is_pairing_peer(&self, peer: &PeerId, now: Instant) -> bool {
        self.pairing
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|window| now < window.deadline && window.peer.as_ref() == Some(peer))
    }

    pub(super) fn connection_admission<M: PairedAnnounceMatcher>(
        &self,
        matcher: &M,
        announce: Option<&M::Announce>,
        now: Instant,
    ) -> Option<ConnectionAdmission> {
        if let Some(contact) =
            announce.and_then(|announce| matcher.paired_contact_for_announce(announce))
        {
            Some(ConnectionAdmission::Paired(contact))
        } else if self.public_chat_enabled() {
            Some(ConnectionAdmission::PublicChat)
        } else if self.pairing_active(now) {
            Some(ConnectionAdmission::Pairing)
        } else {
            None
        }
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
}

impl MeshService {
    pub fn custom_nickname(&self) -> Option<String> {
        self.config.custom_nickname()
    }

    pub fn bridge_mode_enabled(&self) -> bool {
        self.config.bridge_mode_enabled()
    }

    pub fn set_bridge_mode(&self, enabled: bool) {
        self.state.with(|state| {
            self.config.set_bridge_mode(enabled);
            if enabled {
                return;
            }
            // Discard older announcements, including fragments of a bridge
            // announce queued before the setting was switched off.
            let links: Vec<_> = state.writes.link_ids().cloned().collect();
            for link in links {
                let mut pending = state.writes.take_all(&link);
                pending.retain(|write| {
                    super::ble::broadcasts::frame_kind(&write.bytes) != Some(MessageType::Announce)
                });
                state.writes.prepend(&link, pending);
            }
        });
        if self.is_running() {
            let _ = self.ble.send(BleCommand::Announce);
            let _ = self.nostr.send(NostrCommand::BridgeModeChanged);
        }
    }

    pub fn public_chat_enabled(&self) -> bool {
        self.config.public_chat_enabled()
    }

    pub fn set_public_chat(&self, enabled: bool) {
        self.config.set_public_chat(enabled);
        if enabled {
            self.timeline.open_public_room();
        }
        self.events.send(MeshEvent::Peers(self.peers()));
    }

    pub fn pairing_active(&self) -> bool {
        self.config.pairing_active(Instant::now())
    }

    pub fn set_pairing(&self, enabled: bool) {
        self.config
            .set_pairing(enabled && self.status().radio_running, Instant::now());
        self.events.send(MeshEvent::Peers(self.peers()));
    }

    pub fn select_pairing_peer(&self, peer: &PeerId) {
        let peer = self.state.route(peer);
        if self.state.with(|state| state.peers.info(&peer).is_some()) {
            self.config.select_pairing_peer(peer, Instant::now());
        }
    }

    pub(super) fn require_public_chat(&self) -> Result<(), MeshError> {
        if self.public_chat_enabled() {
            Ok(())
        } else {
            Err(MeshError::PublicChatDisabled)
        }
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
    fn the_default_nickname_is_random_and_stable_until_rotation() {
        let config = MeshConfig::default();
        let original = config.nickname();
        assert!(original.starts_with("anon-"));
        assert_eq!(config.nickname(), original);
        assert_eq!(config.custom_nickname(), None);

        let peer = PeerId::from_bytes(&[1; 8]);
        config.on_identity_rotated(&peer);
        assert_ne!(config.nickname(), original);
        let rotated = config.nickname();
        config.on_identity_rotated(&peer);
        assert_eq!(config.nickname(), rotated);
        assert_ne!(MeshConfig::default().nickname(), config.nickname());
    }

    #[test]
    fn a_mesh_nickname_survives_rotation_and_clearing_it_restores_anonymity() {
        let config = MeshConfig::default();
        let original = config.nickname();
        config.set_nickname("  ada  ".into());
        config.on_identity_rotated(&PeerId::from_bytes(&[2; 8]));
        assert_eq!(config.nickname(), "ada");
        assert_eq!(config.custom_nickname().as_deref(), Some("ada"));

        config.set_nickname("   ".into());
        assert_eq!(config.custom_nickname(), None);
        assert!(config.nickname().starts_with("anon-"));
        assert_ne!(config.nickname(), original);
    }

    #[test]
    fn the_relay_bridge_is_off_unless_asked_for() {
        assert!(!MeshConfig::default().bridge_enabled);
    }

    #[test]
    fn bridge_metadata_and_public_chat_have_independent_opt_ins() {
        let config = MeshConfig::default();
        assert!(!config.bridge_mode_enabled());
        assert!(!config.public_chat_enabled());
        config.set_bridge_mode(true);
        assert!(!config.public_chat_enabled());
        config.set_bridge_mode(false);
        config.set_public_chat(true);
        assert!(!config.bridge_mode_enabled());
    }

    #[test]
    fn pairing_requires_an_explicit_start_and_expires_once() {
        let config = MeshConfig::default();
        let now = Instant::now();
        let peer = PeerId::from_bytes(&[3; 8]);
        assert!(!config.pairing_active(now));
        config.select_pairing_peer(peer.clone(), now);
        assert!(!config.is_pairing_peer(&peer, now));

        config.set_pairing(true, now);
        assert!(config.pairing_active(now));
        assert!(!config.is_pairing_peer(&peer, now));
        config.select_pairing_peer(peer.clone(), now);
        assert!(config.is_pairing_peer(&peer, now));
        assert!(!config.is_pairing_peer(&PeerId::from_bytes(&[4; 8]), now));
        let deadline = now + PAIRING_WINDOW;
        assert!(!config.pairing_active(deadline));
        assert!(!config.is_pairing_peer(&peer, deadline));
        assert!(config.expire_pairing(deadline));
        assert!(!config.expire_pairing(deadline));
    }

    #[test]
    fn new_links_require_a_paired_announcement_or_an_explicit_exception() {
        struct Matcher;
        impl PairedAnnounceMatcher for Matcher {
            type Announce = [u8; 8];

            fn paired_contact_for_announce(&self, announce: &Self::Announce) -> Option<PeerId> {
                (announce == &[42; 8]).then(|| PeerId::from_bytes(&[1; 8]))
            }
        }

        let config = MeshConfig::default();
        let now = Instant::now();
        assert_eq!(config.connection_admission(&Matcher, None, now), None);
        assert_eq!(
            config.connection_admission(&Matcher, Some(&[7; 8]), now),
            None
        );
        assert_eq!(
            config.connection_admission(&Matcher, Some(&[42; 8]), now),
            Some(ConnectionAdmission::Paired(PeerId::from_bytes(&[1; 8]))),
        );
        config.set_pairing(true, now);
        assert_eq!(
            config.connection_admission(&Matcher, None, now),
            Some(ConnectionAdmission::Pairing)
        );
        assert_eq!(
            config.connection_admission(&Matcher, None, now + PAIRING_WINDOW),
            None
        );
        config.set_public_chat(true);
        assert_eq!(
            config.connection_admission(&Matcher, None, now),
            Some(ConnectionAdmission::PublicChat)
        );
    }
}

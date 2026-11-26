//! Durable, directional pairings for recognizing a fresh BLE identity.

use std::collections::BTreeMap;
use std::sync::Arc;

use chatter_mesh::ids::PeerId;
use chatter_mesh::tlv::announce::AnnouncementPacket;
use chatter_mesh::tlv::courier_envelope::{CourierEnvelope, candidate_tags};
use chatter_mesh::tlv::pairing::{PAIRING_BEACON_SLOTS, PairingBeacon};
use chatter_mesh_link::registry::{PairingKey, PeerRegistry};
use chatter_mesh_noise::MeshIdentity;
use hmac::{Hmac, Mac};
use rand::{Rng, seq::SliceRandom};
use sha2::Sha256;

use super::state::{MeshState, now_secs};

/// Eight contacts per 40-second window give a full 4,096-entry roster
/// at least four passes per day. Only the five seconds at a window edge
/// accommodate the adjacent bucket.
pub(super) const BEACON_WINDOW_SECS: u64 = 40;
const BEACON_SKEW_SECS: u64 = 5;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum BeaconKind {
    Advert,
    Preface,
}

fn beacon_window(now: u64) -> u64 {
    now / BEACON_WINDOW_SECS
}

fn mac(key: &[u8; 32], domain: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut mac =
        <Hmac<Sha256> as hmac::KeyInit>::new_from_slice(key).expect("HMAC accepts 32-byte keys");
    mac.update(domain);
    for part in parts {
        mac.update(part);
    }
    mac.finalize().into_bytes().into()
}

/// Issuer and scanner order their two live Hello nonces identically.
pub(super) fn establish_key(
    qr_token: &[u8; 32],
    transcript: &[u8; 32],
    issuer_nonce: &[u8; 16],
    scanner_nonce: &[u8; 16],
    issuer: bool,
) -> PairingKey {
    let root = zeroize::Zeroizing::new(mac(
        qr_token,
        b"chatter.mesh.pairing.v1\0",
        &[transcript, issuer_nonce, scanner_nonce],
    ));
    PairingKey::new(*root, issuer)
}

fn beacon(key: &PairingKey, window: u64, direction: u8, kind: BeaconKind) -> PairingBeacon {
    let domain = match kind {
        BeaconKind::Advert => b"chatter.mesh.advert.v2\0".as_slice(),
        BeaconKind::Preface => b"chatter.mesh.preface.v2\0".as_slice(),
    };
    let tag = mac(key.root(), domain, &[&window.to_be_bytes(), &[direction]]);
    let mut bytes = [0; 13];
    bytes.copy_from_slice(&tag[..13]);
    bytes
}

#[derive(Default)]
pub(super) struct PairingBeaconSlots {
    epoch: Option<(PeerId, u64)>,
    published: Option<(PeerId, u64)>,
    next_contact: Option<usize>,
    slots: [PairingBeacon; PAIRING_BEACON_SLOTS],
    incoming: BTreeMap<u64, BeaconIndex>,
}

type BeaconIndex = BTreeMap<(BeaconKind, PairingBeacon), Option<PeerId>>;

impl PairingBeaconSlots {
    fn active(&self, local: &PeerId, window: u64) -> bool {
        local.to_routing_bytes().is_some()
            && self
                .epoch
                .as_ref()
                .is_none_or(|(peer, start)| peer != local || window > *start)
    }

    fn for_window(&mut self, local: &PeerId, peers: &PeerRegistry, now: u64) -> Vec<PairingBeacon> {
        let window = beacon_window(now);
        // Keep the cover slots constant within a window, including when the
        // roster changes or several callers request the same schedule.
        if self
            .published
            .as_ref()
            .is_some_and(|(peer, at)| peer == local && *at == window)
        {
            return self.slots.to_vec();
        }
        let mut rng = rand::rng();
        for slot in &mut self.slots {
            rng.fill(slot);
        }
        if self.active(local, window) {
            let mut pairings: Vec<_> = peers
                .pinned_identities()
                .filter_map(|(_, pin)| Some((&pin.contact, pin.pairing.as_ref()?)))
                .collect();
            pairings.sort_unstable_by(|(a, _), (b, _)| a.cmp(b));
            let count = pairings.len();
            if count != 0 {
                let start = self
                    .next_contact
                    .map_or_else(|| rng.random_range(0..count), |cursor| cursor % count);
                let used = count.min(PAIRING_BEACON_SLOTS);
                for (slot, index) in self.slots.iter_mut().zip(start..start + used) {
                    let (_, key) = pairings[index % count];
                    *slot = beacon(key, window, key.local_direction(), BeaconKind::Advert);
                }
                self.next_contact = Some((start + used) % count);
            }
        }
        self.slots.shuffle(&mut rng);
        self.published = Some((local.clone(), window));
        self.slots.to_vec()
    }

    pub(super) fn invalidate_contacts(&mut self) {
        self.incoming.clear();
    }

    fn recognize(
        &mut self,
        peers: &PeerRegistry,
        token: &PairingBeacon,
        kind: BeaconKind,
        now: u64,
    ) -> Option<PeerId> {
        let window = beacon_window(now);
        let offset = now % BEACON_WINDOW_SECS;
        let adjacent = if offset < BEACON_SKEW_SECS {
            window.checked_sub(1)
        } else if BEACON_WINDOW_SECS - offset <= BEACON_SKEW_SECS {
            Some(window + 1)
        } else {
            None
        };
        self.incoming
            .retain(|&at, _| at == window || Some(at) == adjacent);
        let mut matched = None;
        for at in std::iter::once(window).chain(adjacent) {
            let index = self.incoming.entry(at).or_insert_with(|| {
                let mut index = BeaconIndex::new();
                for (_, pin) in peers.pinned_identities() {
                    if let Some(key) = &pin.pairing {
                        for kind in [BeaconKind::Advert, BeaconKind::Preface] {
                            let tag = beacon(key, at, key.local_direction() ^ 1, kind);
                            index
                                .entry((kind, tag))
                                .and_modify(|match_| *match_ = None)
                                .or_insert_with(|| Some(pin.contact.clone()));
                        }
                    }
                }
                index
            });
            if let Some(match_) = index.get(&(kind, *token)) {
                let contact = match_.as_ref()?;
                if matched.as_ref().is_some_and(|prior| prior != contact) {
                    return None;
                }
                matched = Some(contact.clone());
            }
        }
        matched
    }
}

/// A proof for a particular Noise peer and transcript. Its direction prevents
/// reflection to the side that created it.
pub(super) fn route_proof(
    key: &PairingKey,
    transcript: &[u8; 32],
    noise: &[u8; 32],
    signing: &[u8; 32],
    local: bool,
) -> [u8; 32] {
    let direction = key.local_direction() ^ u8::from(!local);
    mac(
        key.root(),
        b"chatter.mesh.route.v1\0",
        &[transcript, noise, signing, &[direction]],
    )
}

#[derive(Clone)]
pub(super) struct PairingCourier {
    pub(super) local: Arc<MeshIdentity>,
    pub(super) remote: [u8; 32],
}

fn courier_identity(key: &PairingKey, direction: u8) -> MeshIdentity {
    let noise = zeroize::Zeroizing::new(mac(
        key.root(),
        b"chatter.mesh.courier.noise.v1\0",
        &[&[direction]],
    ));
    let signing = zeroize::Zeroizing::new(mac(
        key.root(),
        b"chatter.mesh.courier.sign.v1\0",
        &[&[direction]],
    ));
    MeshIdentity::from_keys(*noise, *signing)
}

impl PairingCourier {
    fn new(key: &PairingKey) -> Self {
        let direction = key.local_direction();
        Self {
            local: Arc::new(courier_identity(key, direction)),
            remote: courier_identity(key, direction ^ 1).static_public(),
        }
    }
}

/// Derived courier keys are cached so looking up an offline recipient never
/// needs to repeat a Curve25519 operation over the whole contact list.
#[derive(Default)]
pub(super) struct PairingCouriers {
    by_contact: BTreeMap<PeerId, PairingCourier>,
    by_recipient: BTreeMap<[u8; 32], PeerId>,
}

impl PairingCouriers {
    pub(super) fn insert(&mut self, contact: PeerId, key: &PairingKey) {
        let courier = PairingCourier::new(key);
        self.by_recipient.insert(courier.remote, contact.clone());
        if let Some(old) = self.by_contact.insert(contact, courier.clone())
            && old.remote != courier.remote
        {
            self.by_recipient.remove(&old.remote);
        }
    }

    pub(super) fn restore(&mut self, peers: &PeerRegistry) {
        for (_, pin) in peers.pinned_identities() {
            if let Some(key) = &pin.pairing {
                self.insert(pin.contact.clone(), key);
            }
        }
    }
}

impl MeshState {
    /// Keep the first partial clock window covered with decoys after enabling
    /// or replacing our local identity, including after a process restart.
    pub(super) fn begin_beacon_epoch(&self, local: &PeerId, started_at: u64) {
        self.with(|state| {
            state.pairing_beacon_slots.epoch = Some((local.clone(), beacon_window(started_at)));
            state.pairing_beacon_slots.published = None;
        });
    }

    /// Eight decoy-padded slots. Each window advances to the next contacts.
    pub(crate) fn pairing_beacons(&self, local: &PeerId) -> Vec<PairingBeacon> {
        self.pairing_beacons_at(local, now_secs())
    }

    pub(super) fn pairing_beacons_at(&self, local: &PeerId, now: u64) -> Vec<PairingBeacon> {
        self.with(|state| {
            state
                .pairing_beacon_slots
                .for_window(local, &state.peers, now)
        })
    }

    /// A first-write token uses a distinct domain, so a captured first write
    /// cannot be mistaken for an advertised slot or identify which slot is real.
    pub(crate) fn pairing_beacon_for_contact(
        &self,
        local: &PeerId,
        contact: &PeerId,
    ) -> Option<PairingBeacon> {
        self.pairing_beacon_for_contact_at(local, contact, now_secs())
    }

    pub(super) fn pairing_beacon_for_contact_at(
        &self,
        local: &PeerId,
        contact: &PeerId,
        now: u64,
    ) -> Option<PairingBeacon> {
        let window = beacon_window(now);
        self.with(|state| {
            if !state.pairing_beacon_slots.active(local, window) {
                return None;
            }
            let key = state
                .peers
                .pinned(state.peers.route(contact))?
                .pairing
                .as_ref()?;
            Some(beacon(
                key,
                window,
                key.local_direction(),
                BeaconKind::Preface,
            ))
        })
    }

    pub(crate) fn paired_contact_for_beacon(&self, advertised: &PairingBeacon) -> Option<PeerId> {
        self.recognize_beacon(advertised, BeaconKind::Advert, now_secs())
    }

    pub(crate) fn paired_contact_for_preface(&self, preface: &PairingBeacon) -> Option<PeerId> {
        self.recognize_beacon(preface, BeaconKind::Preface, now_secs())
    }

    fn recognize_beacon(
        &self,
        token: &PairingBeacon,
        kind: BeaconKind,
        now: u64,
    ) -> Option<PeerId> {
        self.with(|state| {
            state
                .pairing_beacon_slots
                .recognize(&state.peers, token, kind, now)
        })
    }

    pub(crate) fn paired_contact_for_announce(
        &self,
        announce: &AnnouncementPacket,
    ) -> Option<PeerId> {
        self.paired_contact_for_beacon(announce.pairing_beacon.as_ref()?)
    }

    pub(super) fn pairing_key(&self, peer: &PeerId) -> Option<PairingKey> {
        self.with(|state| state.peers.pinned(state.peers.route(peer))?.pairing.clone())
    }

    pub(super) fn has_paired_pin(&self, peer: &PeerId) -> bool {
        self.with(|state| {
            state
                .peers
                .pinned(state.peers.route(peer))
                .is_some_and(|pin| pin.pairing.is_some())
        })
    }

    pub(super) fn pairing_courier(&self, peer: &PeerId) -> Option<PairingCourier> {
        self.with(|state| {
            state
                .pairing_couriers
                .by_contact
                .get(state.peers.contact(peer))
                .cloned()
        })
    }

    pub(super) fn courier_sender(&self, recipient: &[u8; 32]) -> Option<Arc<MeshIdentity>> {
        self.with(|state| {
            let couriers = &state.pairing_couriers;
            Some(
                couriers
                    .by_contact
                    .get(couriers.by_recipient.get(recipient)?)?
                    .local
                    .clone(),
            )
        })
    }

    pub(super) fn local_courier_keys(&self) -> Vec<Arc<MeshIdentity>> {
        self.with(|state| {
            state
                .pairing_couriers
                .by_contact
                .values()
                .map(|pair| pair.local.clone())
                .collect()
        })
    }

    /// Recognize the local pair address before doing any public-key operations.
    pub(super) fn open_paired_mail(
        &self,
        envelope: &CourierEnvelope,
        now: u64,
    ) -> Option<(Vec<u8>, PeerId)> {
        if envelope.prekey_id.is_some() {
            return None;
        }
        let (contact, pair) = self.with(|state| {
            state
                .pairing_couriers
                .by_contact
                .iter()
                .find_map(|(contact, pair)| {
                    let public = pair.local.static_public();
                    candidate_tags(&public, now)
                        .contains(&envelope.recipient_tag)
                        .then(|| (contact.clone(), pair.clone()))
                })
        })?;
        let (payload, sender) =
            chatter_mesh_noise::seal::open_courier(&envelope.ciphertext, &pair.local).ok()?;
        (sender == pair.remote).then_some((payload, contact))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chatter_mesh_link::registry::MAX_PINNED_IDENTITIES;
    use std::collections::BTreeSet;

    fn add_pairing(state: &MeshState, index: u16, issuer: bool) -> (PeerId, PairingKey) {
        let mut noise = [0; 32];
        noise[..2].copy_from_slice(&index.to_be_bytes());
        let peer = PeerId::from_noise_key(&noise);
        let key = PairingKey::new(noise, issuer);
        state.with(|state| {
            assert!(
                state
                    .peers
                    .pair_noise_identity(&peer, &noise, &noise, 0, key.clone())
            )
        });
        (peer, key)
    }

    #[test]
    fn eight_slots_stay_constant_and_cover_a_full_roster_in_less_than_six_hours() {
        let state = MeshState::new();
        let local = PeerId::from_noise_key(&[101; 32]);
        let decoys = state.pairing_beacons_at(&local, 0);
        assert_eq!(
            decoys.iter().collect::<BTreeSet<_>>().len(),
            PAIRING_BEACON_SLOTS
        );
        let contacts: Vec<_> = (1..=MAX_PINNED_IDENTITIES as u16)
            .map(|index| add_pairing(&state, index, true))
            .collect();
        assert_eq!(state.pairing_beacons_at(&local, 1), decoys);
        let mut advertised = BTreeSet::new();
        let windows = MAX_PINNED_IDENTITIES.div_ceil(PAIRING_BEACON_SLOTS) as u64;
        assert!(windows * BEACON_WINDOW_SECS < 6 * 60 * 60);
        for window in 1..=windows {
            let slots = state.pairing_beacons_at(&local, window * BEACON_WINDOW_SECS);
            assert_eq!(slots.len(), PAIRING_BEACON_SLOTS);
            // Use the sorted roster's start and step to check coverage without
            // reproducing a 4,096-key receiver scan for every window.
            let slot_ids = state.with(|inner| {
                let mut roster: Vec<_> = contacts.iter().collect();
                roster.sort_unstable_by(|a, b| a.0.cmp(&b.0));
                let next = inner.pairing_beacon_slots.next_contact.unwrap();
                let start = (next + contacts.len() - PAIRING_BEACON_SLOTS) % contacts.len();
                (start..start + PAIRING_BEACON_SLOTS)
                    .map(|index| {
                        let (id, key) = roster[index % contacts.len()];
                        assert!(slots.contains(&beacon(
                            key,
                            window,
                            key.local_direction(),
                            BeaconKind::Advert
                        )));
                        id.clone()
                    })
                    .collect::<Vec<_>>()
            });
            advertised.extend(slot_ids);
        }
        assert_eq!(advertised.len(), contacts.len());
    }

    #[test]
    fn clock_window_accepts_small_skew_and_rejects_old_beacons() {
        let state = MeshState::new();
        let (contact, key) = add_pairing(&state, 1, false);
        let window = 10;
        let start = window * BEACON_WINDOW_SECS;
        let next = start + BEACON_WINDOW_SECS;
        let tag = beacon(&key, window, 1, BeaconKind::Advert);
        let recognize = |now| state.recognize_beacon(&tag, BeaconKind::Advert, now);
        assert_eq!(recognize(start - 1), Some(contact.clone()));
        assert_eq!(recognize(start + 1), Some(contact.clone()));
        assert_eq!(recognize(next + BEACON_SKEW_SECS - 1), Some(contact));
        assert_eq!(recognize(next + BEACON_SKEW_SECS), None);
        assert_eq!(recognize(next + 86_400), None);
        assert_ne!(tag, beacon(&key, window + 1, 1, BeaconKind::Advert));
        assert_eq!(
            state.recognize_beacon(&tag, BeaconKind::Preface, start),
            None
        );
    }

    #[test]
    fn received_beacon_index_is_reused_under_unmatched_traffic() {
        let state = MeshState::new();
        let (contact, key) = add_pairing(&state, 1, false);
        let now = 418;
        let tag = beacon(&key, beacon_window(now), 1, BeaconKind::Advert);
        assert_eq!(
            state.recognize_beacon(&tag, BeaconKind::Advert, now),
            Some(contact)
        );
        let (_, new_pair) = add_pairing(&state, 2, false);
        let new_tag = beacon(&new_pair, beacon_window(now), 1, BeaconKind::Advert);
        assert!(
            state
                .recognize_beacon(&new_tag, BeaconKind::Advert, now)
                .is_none()
        );
        state.with(|state| state.pairing_beacon_slots.invalidate_contacts());
        assert!(
            state
                .recognize_beacon(&new_tag, BeaconKind::Advert, now)
                .is_some()
        );
    }

    #[test]
    fn newly_enabled_epoch_covers_the_rest_of_its_first_window() {
        let state = MeshState::new();
        let (contact, key) = add_pairing(&state, 1, false);
        let local = PeerId::from_noise_key(&[101; 32]);
        state.begin_beacon_epoch(&local, 415);
        let current = state.pairing_beacons_at(&local, 416);
        let next = state.pairing_beacons_at(&local, 440);
        assert!(!current.contains(&beacon(&key, 10, 0, BeaconKind::Advert)));
        assert!(next.contains(&beacon(&key, 11, 0, BeaconKind::Advert)));
        assert!(
            state
                .pairing_beacon_for_contact_at(&local, &contact, 416)
                .is_none()
        );
        assert!(
            state
                .pairing_beacon_for_contact_at(&local, &contact, 440)
                .is_some()
        );
    }
}

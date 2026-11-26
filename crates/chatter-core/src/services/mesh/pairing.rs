//! Durable, directional pairings for recognizing a fresh BLE identity.

use std::collections::BTreeMap;
use std::sync::Arc;

use chatter_mesh::ids::PeerId;
use chatter_mesh::tlv::announce::AnnouncementPacket;
use chatter_mesh::tlv::courier_envelope::{CourierEnvelope, candidate_tags};
use chatter_mesh::tlv::pairing::{PAIRING_BEACON_NONCE_LEN, PAIRING_BEACON_SLOTS, PairingBeacon};
use chatter_mesh_link::registry::{PairingKey, PeerRegistry};
use chatter_mesh_noise::MeshIdentity;
use hmac::{Hmac, Mac};
use rand::{Rng, seq::SliceRandom};
use sha2::Sha256;
use subtle::ConstantTimeEq;

use super::state::MeshState;

type Nonce = [u8; PAIRING_BEACON_NONCE_LEN];

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
    PairingKey::new(
        mac(
            qr_token,
            b"chatter.mesh.pairing.v1\0",
            &[transcript, issuer_nonce, scanner_nonce],
        ),
        issuer,
    )
}

fn epoch_nonce(peer: &PeerId) -> Option<Nonce> {
    peer.to_routing_bytes()?[..PAIRING_BEACON_NONCE_LEN]
        .try_into()
        .ok()
}

fn beacon(key: &PairingKey, nonce: &Nonce, direction: u8) -> PairingBeacon {
    let tag = mac(
        key.root(),
        b"chatter.mesh.beacon.v1\0",
        &[nonce, &[direction]],
    );
    let mut bytes = [0; 13];
    bytes[..PAIRING_BEACON_NONCE_LEN].copy_from_slice(nonce);
    bytes[PAIRING_BEACON_NONCE_LEN..].copy_from_slice(&tag[..8]);
    bytes
}

#[derive(Default)]
pub(super) struct PairingBeaconSlots {
    local: Option<PeerId>,
    next_contact: Option<usize>,
    slots: [PairingBeacon; PAIRING_BEACON_SLOTS],
}

impl PairingBeaconSlots {
    fn for_epoch(&mut self, local: &PeerId, peers: &PeerRegistry) -> Vec<PairingBeacon> {
        // Reusing the slots for the whole epoch keeps a roster change from
        // revealing how many decoys were replaced.
        if self.local.as_ref() == Some(local) {
            return self.slots.to_vec();
        }

        let mut rng = rand::rng();
        let nonce = epoch_nonce(local);
        for slot in &mut self.slots {
            let (prefix, tag) = slot.split_at_mut(PAIRING_BEACON_NONCE_LEN);
            prefix.copy_from_slice(&nonce.unwrap_or_default());
            rng.fill(tag);
        }

        if let Some(nonce) = nonce {
            let mut pairings: Vec<_> = peers
                .pinned_identities()
                .filter_map(|(_, pin)| Some((&pin.contact, pin.pairing.as_ref()?)))
                .collect();
            pairings.sort_unstable_by(|(a, _), (b, _)| a.cmp(b));
            if !pairings.is_empty() {
                let count = pairings.len();
                let start = self
                    .next_contact
                    .map_or_else(|| rng.random_range(0..count), |cursor| cursor % count);
                let used = count.min(PAIRING_BEACON_SLOTS);
                for (slot, index) in self.slots.iter_mut().zip(start..start + used) {
                    let (_, key) = pairings[index % count];
                    *slot = beacon(key, &nonce, key.local_direction());
                }
                self.next_contact = Some((start + used) % count);
            }
        }
        self.slots.shuffle(&mut rng);
        self.local = Some(local.clone());
        self.slots.to_vec()
    }
}

fn match_beacon<'a>(
    advertised: &PairingBeacon,
    pairings: impl Iterator<Item = (&'a PeerId, &'a PairingKey)>,
) -> Option<PeerId> {
    let nonce = advertised[..PAIRING_BEACON_NONCE_LEN].try_into().ok()?;
    let mut matched = None;
    for (contact, pairing) in pairings {
        let expected = beacon(pairing, &nonce, pairing.local_direction() ^ 1);
        if bool::from(expected.ct_eq(advertised)) {
            if matched.is_some() {
                return None;
            }
            matched = Some(contact.clone());
        }
    }
    matched
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
    MeshIdentity::from_keys(
        mac(
            key.root(),
            b"chatter.mesh.courier.noise.v1\0",
            &[&[direction]],
        ),
        mac(
            key.root(),
            b"chatter.mesh.courier.sign.v1\0",
            &[&[direction]],
        ),
    )
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
    /// Eight randomly ordered, decoy-padded slots, held until the next epoch.
    pub(crate) fn pairing_beacons(&self, local: &PeerId) -> Vec<PairingBeacon> {
        self.with(|state| state.pairing_beacon_slots.for_epoch(local, &state.peers))
    }

    /// The beacon for a particular contact's first write, including contacts
    /// outside of the current epoch's broadcast slots.
    pub(crate) fn pairing_beacon_for_contact(
        &self,
        local: &PeerId,
        contact: &PeerId,
    ) -> Option<PairingBeacon> {
        let nonce = epoch_nonce(local)?;
        self.with(|state| {
            let key = state
                .peers
                .pinned(state.peers.route(contact))?
                .pairing
                .as_ref()?;
            Some(beacon(key, &nonce, key.local_direction()))
        })
    }

    /// A bounded recognition pass over current pairings. Epoch history is
    /// never consulted: the caller presents the fresh epoch nonce.
    pub(crate) fn paired_contact_for_beacon(&self, advertised: &PairingBeacon) -> Option<PeerId> {
        self.with(|state| {
            let pairings = state
                .peers
                .pinned_identities()
                .filter_map(|(_, pin)| Some((&pin.contact, pin.pairing.as_ref()?)));
            match_beacon(advertised, pairings)
        })
    }

    pub(crate) fn paired_contact_for_announce(
        &self,
        announce: &AnnouncementPacket,
    ) -> Option<PeerId> {
        let advertised = announce.pairing_beacon.as_ref()?;
        let peer = PeerId::from_noise_key(&announce.noise_public_key);
        if advertised[..PAIRING_BEACON_NONCE_LEN] != epoch_nonce(&peer)? {
            return None;
        }
        self.paired_contact_for_beacon(advertised)
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
    use std::collections::BTreeSet;

    use super::*;
    use chatter_mesh_link::registry::MAX_PINNED_IDENTITIES;

    #[test]
    fn eight_slots_stay_constant_and_contacts_take_turns_across_epochs() {
        let state = MeshState::new();
        let first = PeerId::from_noise_key(&[101; 32]);
        let decoys = state.pairing_beacons(&first);
        assert_eq!(decoys.len(), PAIRING_BEACON_SLOTS);
        assert_eq!(
            decoys.iter().collect::<BTreeSet<_>>().len(),
            PAIRING_BEACON_SLOTS
        );
        let contacts: Vec<_> = (1..=PAIRING_BEACON_SLOTS as u8 + 3)
            .map(|index| {
                let noise = [index; 32];
                let peer = PeerId::from_noise_key(&noise);
                state.with(|state| {
                    assert!(state.peers.pair_noise_identity(
                        &peer,
                        &noise,
                        &noise,
                        0,
                        PairingKey::new([index; 32], true),
                    ));
                });
                peer
            })
            .collect();
        assert_eq!(state.pairing_beacons(&first), decoys);
        let mut advertised = BTreeSet::new();
        for epoch in [102, 103] {
            let local = PeerId::from_noise_key(&[epoch; 32]);
            let slots = state.pairing_beacons(&local);
            assert_eq!(slots.len(), PAIRING_BEACON_SLOTS);
            let visible: Vec<_> = contacts
                .iter()
                .filter(|contact| {
                    let beacon = state.pairing_beacon_for_contact(&local, contact).unwrap();
                    slots.contains(&beacon)
                })
                .collect();
            assert_eq!(visible.len(), PAIRING_BEACON_SLOTS);
            advertised.extend(visible);
        }
        assert_eq!(advertised.len(), contacts.len());
    }

    #[test]
    fn recognition_checks_each_pair_at_most_once_with_a_fresh_epoch_nonce() {
        let roster: Vec<_> = (0..MAX_PINNED_IDENTITIES)
            .map(|index| {
                let mut root = [0; 32];
                root[..8].copy_from_slice(&(index as u64).to_be_bytes());
                (PeerId::from_noise_key(&root), PairingKey::new(root, false))
            })
            .collect();
        let (contact, key) = &roster[MAX_PINNED_IDENTITIES / 2];
        let advertised = beacon(key, &[0x9f; PAIRING_BEACON_NONCE_LEN], 1);
        let mut attempts = 0;
        let pairings = roster.iter().map(|(id, root)| {
            attempts += 1;
            (id, root)
        });
        assert_eq!(match_beacon(&advertised, pairings), Some(contact.clone()));
        assert_eq!(attempts, MAX_PINNED_IDENTITIES);
    }
}

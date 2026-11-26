//! Durable, directional pairings for recognizing a fresh BLE identity.

use std::collections::BTreeMap;
use std::sync::Arc;

use chatter_mesh::ids::PeerId;
use chatter_mesh::tlv::announce::AnnouncementPacket;
use chatter_mesh::tlv::courier_envelope::{CourierEnvelope, candidate_tags};
use chatter_mesh::tlv::pairing::{PAIRING_BEACON_NONCE_LEN, PairingBeacon};
use chatter_mesh_link::registry::{PairingKey, PeerRegistry};
use chatter_mesh_noise::MeshIdentity;
use hmac::{Hmac, Mac};
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
    /// One current-epoch beacon per pairing, in durable contact order.
    pub(crate) fn pairing_beacons(&self, local: &PeerId) -> Vec<PairingBeacon> {
        let Some(nonce) = epoch_nonce(local) else {
            return Vec::new();
        };
        self.with(|state| {
            let mut beacons: Vec<_> = state
                .peers
                .pinned_identities()
                .filter_map(|(_, pin)| {
                    let pairing = pin.pairing.as_ref()?;
                    Some((
                        &pin.contact,
                        beacon(pairing, &nonce, pairing.local_direction()),
                    ))
                })
                .collect();
            beacons.sort_unstable_by(|(a, _), (b, _)| a.cmp(b));
            beacons.into_iter().map(|(_, beacon)| beacon).collect()
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

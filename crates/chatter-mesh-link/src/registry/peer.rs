use std::collections::BTreeMap;

use zeroize::{Zeroize, Zeroizing};

use chatter_mesh::ids::PeerId;
use chatter_mesh::tlv::announce::AnnouncementPacket;
use chatter_mesh::tlv::capabilities::PeerCapabilities;

use super::link_state::LinkPresence;

/// `TransportConfig.bleReachabilityRetentionVerifiedSeconds` (60 s).
///
/// How long a *verified* peer stays reachable after its last sighting. Verified
/// means the signing key was bound to the Noise static key inside an
/// authenticated session.
pub const REACHABILITY_RETENTION_VERIFIED_MS: u64 = 60_000;

/// `TransportConfig.bleReachabilityRetentionUnverifiedSeconds` (45 s).
pub const REACHABILITY_RETENTION_UNVERIFIED_MS: u64 = 45_000;

/// `TransportConfig.blePeerInactivityTimeoutSeconds` (8 s).
///
/// A *connected* peer that has said nothing for this long is demoted to
/// disconnected — but only when it also has no live link, because an idle link
/// is not a dead one.
pub const PEER_INACTIVITY_TIMEOUT_MS: u64 = 8_000;

/// How long a peer stays reachable after its last sighting.
pub fn reachability_retention_ms(verified: bool) -> u64 {
    if verified {
        REACHABILITY_RETENTION_VERIFIED_MS
    } else {
        REACHABILITY_RETENTION_UNVERIFIED_MS
    }
}

/// What we know about one peer. Ported from `BLEPeerInfo`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerRecord {
    pub peer: PeerId,
    pub nickname: String,
    /// Live on the mesh right now. Distinct from reachable: a peer stays
    /// reachable for a while after its last link goes.
    pub connected: bool,
    pub noise_public_key: Option<Vec<u8>>,
    pub signing_public_key: Option<Vec<u8>>,
    /// The nickname was announced under the peer's Noise-bound signing key.
    pub verified_nickname: bool,
    pub last_seen_ms: u64,
    pub capabilities: PeerCapabilities,
    /// Rendezvous cell from the announce, when it advertises
    /// [`PeerCapabilities::BRIDGE`].
    pub bridge_geohash: Option<String>,
}

/// What an announce changed. Ported from `BLEPeerAnnounceUpdate`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AnnounceUpdate {
    pub is_new_peer: bool,
    /// The peer existed and was marked disconnected — i.e. this announce is a
    /// reconnection, which the UI reports differently from a first sighting.
    pub was_disconnected: bool,
    pub previous_nickname: Option<String>,
}

/// A secret established by the explicit QR pairing and scoped to its two peers.
#[derive(Clone, PartialEq, Eq)]
pub struct PairingKey {
    root: [u8; 32],
    issuer: bool,
}

impl PairingKey {
    pub fn new(root: [u8; 32], issuer: bool) -> Self {
        Self { root, issuer }
    }

    pub fn root(&self) -> &[u8; 32] {
        &self.root
    }

    /// A distinct direction for the issuer and the scanner.
    pub fn local_direction(&self) -> u8 {
        u8::from(self.issuer)
    }
}

impl std::fmt::Debug for PairingKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PairingKey").finish_non_exhaustive()
    }
}

impl Drop for PairingKey {
    fn drop(&mut self) {
        self.root.zeroize();
    }
}

/// The keys proven for a peer ID inside an authenticated session.
///
/// Held apart from [`PeerRecord`] because the two have opposite lifetimes. A
/// record is presence: it ages out, and a peer that leaves is forgotten. A pin
/// is identity: if it went the same way, anything that makes us forget a peer
/// — silence, a restart, a `Leave` — would also reopen trust on first use for
/// its ID, and the next announce to claim it would be believed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PinnedIdentity {
    /// Local contact handle, retained when the radio routing ID changes.
    pub contact: PeerId,
    pub pairing: Option<PairingKey>,
    pub noise_public_key: Vec<u8>,
    pub signing_public_key: Vec<u8>,
    /// The nickname at first sight. Display only, for naming the peer in a
    /// key-change warning after its record is gone.
    pub nickname: String,
    pub pinned_at_ms: u64,
}

/// How many pins are kept.
///
/// When full, new peers are not pinned and existing pins are untouched. The
/// other choice, evicting the oldest, would let anyone in radio range push a
/// victim's pin out by announcing throwaway identities. The ceiling is a heavy
/// user eventually meeting more distinct peers than this; expiring pins unseen
/// for months is the upgrade, and needs a last-seen write this does not do.
pub const MAX_PINNED_IDENTITIES: usize = 4096;

/// First byte of the persisted pin set.
pub(super) const PIN_BLOB_VERSION: u8 = 4;
const MAX_UNPROVEN_PEERS: usize = 512;

/// The persisted pin set could not be read. Carries nothing: the only thing a
/// caller can do is refuse to start from it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MalformedPins;

/// A peer that aged out. Ported from `BLERemovedPeer`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemovedPeer {
    pub peer: PeerId,
    pub nickname: String,
}

/// What one reconciliation pass changed. Ported from `BLEPeerConnectivityChanges`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConnectivityChanges {
    /// Demoted from connected to merely reachable.
    pub disconnected: Vec<PeerId>,
    /// Aged past retention and dropped entirely.
    pub removed: Vec<RemovedPeer>,
}

impl ConnectivityChanges {
    pub fn is_empty(&self) -> bool {
        self.disconnected.is_empty() && self.removed.is_empty()
    }
}

/// Per-peer records: who exists, when we last heard from them, whether their
/// nickname is verified. Ported from `BLEPeerRegistry`.
///
/// A `BTreeMap` rather than a hash map so eviction and snapshot order are stable
/// — the reconciliation pass emits events, and event order that depends on hash
/// seeding is a class of test flake worth not having.
#[derive(Clone, Debug, Default)]
pub struct PeerRegistry {
    peers: BTreeMap<PeerId, PeerRecord>,
    /// Outlives `peers` on purpose — see [`PinnedIdentity`]. Nothing that
    /// forgets a peer touches it.
    pins: BTreeMap<PeerId, PinnedIdentity>,
    routes: BTreeMap<PeerId, PeerId>,
}

impl PeerRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.peers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.peers.is_empty()
    }

    pub fn records(&self) -> impl Iterator<Item = &PeerRecord> {
        self.peers.values()
    }

    pub fn info(&self, peer: &PeerId) -> Option<&PeerRecord> {
        self.peers.get(peer)
    }

    /// How many peers are live right now. This is the mesh degree, and the relay
    /// policy's fanout decisions are scaled by it.
    pub fn connected_count(&self) -> usize {
        self.peers.values().filter(|p| p.connected).count()
    }

    pub fn connected_peer_ids(&self) -> Vec<PeerId> {
        self.peers
            .values()
            .filter(|p| p.connected)
            .map(|p| p.peer.clone())
            .collect()
    }

    /// Lookups that a caller may reach with a full Noise key go through
    /// [`PeerId::to_short`], matching Swift's `peers[peerID.toShort()]`.
    pub fn is_connected(&self, peer: &PeerId) -> bool {
        self.peers
            .get(&peer.to_short())
            .is_some_and(|p| p.connected)
    }

    /// Whether we can still plausibly route to this peer.
    ///
    /// Three tiers, and the middle one matters: a connected peer is reachable; a
    /// remembered peer is reachable only while we are *attached to the mesh at
    /// all*, because with no links there is no path for the memory to be worth
    /// anything; otherwise the memory expires on a retention window that is
    /// longer for verified peers.
    ///
    /// The verified/unverified split is why the retention constants exist
    /// separately: an unverified record is a nickname anyone could have claimed,
    /// so it decays faster.
    pub fn is_reachable(&self, peer: &PeerId, now_unix_millis: u64) -> bool {
        let Some(record) = self.peers.get(&peer.to_short()) else {
            return false;
        };
        if record.connected {
            return true;
        }
        if self.connected_count() == 0 {
            return false;
        }
        let age = now_unix_millis.saturating_sub(record.last_seen_ms);
        age <= reachability_retention_ms(
            record.verified_nickname || self.pins.contains_key(&record.peer),
        )
    }

    pub fn nickname(&self, peer: &PeerId, connected_only: bool) -> Option<&str> {
        let record = self.peers.get(peer)?;
        if connected_only && !record.connected {
            return None;
        }
        Some(record.nickname.as_str())
    }

    pub fn upsert(&mut self, record: PeerRecord) {
        self.peers.insert(record.peer.clone(), record);
    }

    pub fn remove(&mut self, peer: &PeerId) -> Option<PeerRecord> {
        self.peers.remove(peer)
    }

    pub fn remove_all(&mut self) {
        self.peers.clear();
    }

    /// The keys this peer ID was first seen with, whether or not the peer is
    /// still in the registry.
    pub fn pinned(&self, peer: &PeerId) -> Option<&PinnedIdentity> {
        self.pins.get(peer)
    }

    pub fn can_pin(&self, peer: &PeerId) -> bool {
        self.pins.contains_key(peer)
            || (!self.routes.contains_key(peer) && self.pins.len() < MAX_PINNED_IDENTITIES)
    }

    /// Forget every pin. For a panic wipe, and nothing else.
    pub fn clear_pins(&mut self) {
        self.pins.clear();
        self.routes.clear();
    }

    /// Resolve a durable contact handle to the current wire address.
    pub fn route<'a>(&'a self, peer: &'a PeerId) -> &'a PeerId {
        self.routes.get(peer).unwrap_or(peer)
    }

    /// Resolve a wire address to the handle used by the local conversation.
    pub fn contact<'a>(&'a self, peer: &'a PeerId) -> &'a PeerId {
        self.pins.get(peer).map_or(peer, |pin| &pin.contact)
    }

    pub fn pinned_identities(&self) -> impl Iterator<Item = (&PeerId, &PinnedIdentity)> {
        self.pins.iter()
    }

    /// The caller verified the pairing key and the new identity in Noise.
    pub fn bind_paired_route(
        &mut self,
        prior: &PeerId,
        noise: &[u8; 32],
        signing: &[u8; 32],
        now_ms: u64,
    ) -> bool {
        let next = PeerId::from_noise_key(noise);
        let Some(old_pin) = self.pins.get(prior) else {
            return false;
        };
        if old_pin.pairing.is_none()
            || prior == &next
            || self.routes.contains_key(&next)
            || self.pins.contains_key(&next)
            || old_pin.signing_public_key.as_slice() == signing
        {
            return false;
        }
        let contact = old_pin.contact.clone();
        let Some(old_pin) = self.pins.remove(prior) else {
            return false;
        };
        if !self.pin_noise_identity(&next, noise, signing, now_ms) {
            self.pins.insert(prior.clone(), old_pin);
            return false;
        }
        self.routes.remove(&next);
        if let Some(next_pin) = self.pins.get_mut(&next) {
            next_pin.contact = contact.clone();
            next_pin.pairing = old_pin.pairing;
        }
        self.routes.insert(contact, next);
        self.peers.remove(prior);
        true
    }

    /// The pin set, for the caller to seal and persist.
    ///
    /// Every field is length-prefixed with one byte, so a nickname is cut at
    /// 255 bytes — it is display text, and the announce TLV cannot carry more.
    pub fn pins_to_bytes(&self) -> Zeroizing<Vec<u8>> {
        fn field(out: &mut Vec<u8>, bytes: &[u8]) {
            let bytes = &bytes[..bytes.len().min(usize::from(u8::MAX))];
            out.push(bytes.len() as u8);
            out.extend_from_slice(bytes);
        }

        // Reserve the entire blob up front so growing the vector never leaves
        // a previous allocation containing pairing secrets.
        let capacity = 1 + self.pins.len() * (5 * (1 + usize::from(u8::MAX)) + 8 + 1 + 32);
        let mut out = Zeroizing::new(Vec::with_capacity(capacity));
        out.push(PIN_BLOB_VERSION);
        for (peer, pin) in &self.pins {
            field(&mut out, peer.as_str().as_bytes());
            field(&mut out, pin.contact.as_str().as_bytes());
            field(&mut out, &pin.noise_public_key);
            field(&mut out, &pin.signing_public_key);
            field(&mut out, pin.nickname.as_bytes());
            out.extend_from_slice(&pin.pinned_at_ms.to_be_bytes());
            if let Some(pairing) = &pin.pairing {
                out.push(pairing.local_direction() + 1);
                out.extend_from_slice(pairing.root());
            } else {
                out.push(0);
            }
        }
        out
    }

    /// Load a persisted pin set, keeping any pin already held.
    ///
    /// All or nothing. A blob that does not parse to its last byte changes
    /// nothing, because half a pin set is a set with holes an attacker may
    /// have chosen.
    pub fn restore_pins(&mut self, bytes: &[u8]) -> Result<(), MalformedPins> {
        fn field<'a>(rest: &mut &'a [u8]) -> Result<&'a [u8], MalformedPins> {
            let (&len, tail) = rest.split_first().ok_or(MalformedPins)?;
            let (value, tail) = tail
                .split_at_checked(usize::from(len))
                .ok_or(MalformedPins)?;
            *rest = tail;
            Ok(value)
        }

        let Some((&PIN_BLOB_VERSION, mut rest)) = bytes.split_first() else {
            return Err(MalformedPins);
        };
        let mut loaded = Vec::new();
        while !rest.is_empty() {
            let peer = std::str::from_utf8(field(&mut rest)?).map_err(|_| MalformedPins)?;
            let contact = PeerId::from_str(
                std::str::from_utf8(field(&mut rest)?).map_err(|_| MalformedPins)?,
            );
            let noise_public_key = field(&mut rest)?.to_vec();
            let signing_public_key = field(&mut rest)?.to_vec();
            let nickname = String::from_utf8_lossy(field(&mut rest)?).into_owned();
            let (stamp, tail) = rest.split_first_chunk::<8>().ok_or(MalformedPins)?;
            let (&role, tail) = tail.split_first().ok_or(MalformedPins)?;
            let pairing = match role {
                0 => {
                    rest = tail;
                    None
                }
                1 | 2 => {
                    let (root, tail) = tail.split_first_chunk::<32>().ok_or(MalformedPins)?;
                    rest = tail;
                    Some(PairingKey::new(*root, role == 2))
                }
                _ => return Err(MalformedPins),
            };
            // A pin filed under an ID its own key does not derive to could
            // never have been recorded by `on_announce`, so the blob is not
            // one we wrote.
            if noise_public_key.len() != 32
                || signing_public_key.len() != 32
                || contact.to_routing_bytes().is_none()
                || PeerId::from_noise_key(&noise_public_key).as_str() != peer
            {
                return Err(MalformedPins);
            }
            loaded.push((
                PeerId::from_str(peer),
                PinnedIdentity {
                    contact,
                    pairing,
                    noise_public_key,
                    signing_public_key,
                    nickname,
                    pinned_at_ms: u64::from_be_bytes(*stamp),
                },
            ));
        }
        for (peer, pin) in loaded {
            if self.pins.len() >= MAX_PINNED_IDENTITIES {
                break;
            }
            if self.pins.contains_key(&peer)
                || self.routes.contains_key(&peer)
                || self.routes.contains_key(&pin.contact)
            {
                continue;
            }
            self.routes.insert(pin.contact.clone(), peer.clone());
            self.pins.insert(peer, pin);
        }
        Ok(())
    }

    pub fn update_last_seen(&mut self, peer: &PeerId, now_unix_millis: u64) {
        if let Some(record) = self.peers.get_mut(peer) {
            record.last_seen_ms = now_unix_millis;
        }
    }

    /// An in-person QR exchange authorized this Noise-proven identity.
    pub fn pair_noise_identity(
        &mut self,
        peer: &PeerId,
        noise: &[u8; 32],
        signing: &[u8; 32],
        now_ms: u64,
        pairing: PairingKey,
    ) -> bool {
        if !self.pin_noise_identity(peer, noise, signing, now_ms) {
            return false;
        }
        if let Some(pin) = self.pins.get_mut(peer) {
            pin.pairing = Some(pairing);
        }
        true
    }

    /// Record the keys proven by a binding received inside an authenticated Noise session.
    pub fn pin_noise_identity(
        &mut self,
        peer: &PeerId,
        noise: &[u8; 32],
        signing: &[u8; 32],
        now_ms: u64,
    ) -> bool {
        if PeerId::from_noise_key(noise) != *peer {
            return false;
        }
        let existing_pin = self.pins.get(peer);
        if let Some(pin) = existing_pin {
            if pin.noise_public_key.as_slice() != noise
                || pin.signing_public_key.as_slice() != signing
            {
                return false;
            }
        } else if !self.can_pin(peer) {
            return false;
        }

        let mut nickname = self
            .pins
            .get(peer)
            .map_or_else(|| peer.to_string(), |pin| pin.nickname.clone());
        if let Some(record) = self.peers.get_mut(peer) {
            if record.noise_public_key.as_deref() == Some(noise.as_slice())
                && record.signing_public_key.as_deref() == Some(signing.as_slice())
            {
                if existing_pin.is_none() {
                    nickname = record.nickname.clone();
                    record.verified_nickname = true;
                }
            } else {
                // Only the keys were proven. Drop the displaced candidate's metadata.
                record.nickname = nickname.clone();
                record.capabilities = Default::default();
                record.bridge_geohash = None;
                record.noise_public_key = Some(noise.to_vec());
                record.signing_public_key = Some(signing.to_vec());
                record.verified_nickname = false;
            }
        }
        if existing_pin.is_none() {
            self.routes.insert(peer.clone(), peer.clone());
        }
        self.pins
            .entry(peer.clone())
            .or_insert_with(|| PinnedIdentity {
                contact: peer.clone(),
                pairing: None,
                noise_public_key: noise.to_vec(),
                signing_public_key: signing.to_vec(),
                nickname,
                pinned_at_ms: now_ms,
            });
        true
    }

    /// Records a peer from an announce whose signature we verified.
    ///
    /// Replaces the record wholesale rather than merging fields, matching Swift:
    /// the announce is the peer's own current statement about itself, and keeping
    /// stale fields around would mean a nickname or capability that no announce
    /// still claims.
    pub fn upsert_verified_announce(
        &mut self,
        peer: &PeerId,
        announcement: &AnnouncementPacket,
        connected: bool,
        now_unix_millis: u64,
    ) -> AnnounceUpdate {
        self.upsert_verified_announce_preserving(
            peer,
            announcement,
            connected,
            now_unix_millis,
            |_| false,
        )
    }

    /// Protect active links from candidate expiry and eviction under load.
    pub fn upsert_verified_announce_preserving(
        &mut self,
        peer: &PeerId,
        announcement: &AnnouncementPacket,
        connected: bool,
        now_unix_millis: u64,
        has_live_link: impl Fn(&PeerId) -> bool,
    ) -> AnnounceUpdate {
        let verified = self.pins.get(peer).is_some_and(|pin| {
            pin.noise_public_key == announcement.noise_public_key
                && pin.signing_public_key == announcement.signing_public_key
        });
        if self.pins.contains_key(peer) && !verified {
            return AnnounceUpdate::default();
        }
        if !verified {
            self.peers.retain(|id, record| {
                self.pins.contains_key(id)
                    || has_live_link(id)
                    || now_unix_millis.saturating_sub(record.last_seen_ms)
                        < REACHABILITY_RETENTION_UNVERIFIED_MS
            });
            if !self.peers.contains_key(peer) {
                let unproven = self
                    .peers
                    .iter()
                    .filter(|(id, _)| !self.pins.contains_key(*id));
                if unproven.clone().count() >= MAX_UNPROVEN_PEERS {
                    let Some((oldest, _)) = unproven
                        .filter(|(id, _)| !has_live_link(id))
                        .min_by_key(|(_, record)| record.last_seen_ms)
                    else {
                        return AnnounceUpdate::default();
                    };
                    let oldest = oldest.clone();
                    self.peers.remove(&oldest);
                }
            }
        }
        let existing = self.peers.get(peer);
        let update = AnnounceUpdate {
            is_new_peer: existing.is_none(),
            was_disconnected: existing.is_some_and(|e| !e.connected),
            previous_nickname: existing.map(|e| e.nickname.clone()),
        };

        self.peers.insert(
            peer.clone(),
            PeerRecord {
                peer: peer.clone(),
                nickname: announcement.nickname.clone(),
                connected,
                noise_public_key: Some(announcement.noise_public_key.clone()),
                signing_public_key: Some(announcement.signing_public_key.clone()),
                verified_nickname: verified,
                last_seen_ms: now_unix_millis,
                capabilities: announcement.capabilities.unwrap_or_default(),
                bridge_geohash: announcement.bridge_geohash.clone(),
            },
        );

        update
    }

    /// Ages the registry against the clock and the current link presence.
    ///
    /// Two independent rules:
    ///
    /// - A **connected** peer silent for longer than [`PEER_INACTIVITY_TIMEOUT_MS`]
    ///   is demoted — but only if it has no live link. An idle link is not a dead
    ///   peer, and demoting one would tear down a working path over silence alone.
    /// - A **disconnected** peer silent past its retention window is forgotten.
    ///
    /// Both rules read the peer's state as it was at the top of the pass, so a
    /// peer demoted by the first rule is not also removed by the second in the
    /// same call. That is bitchat's behaviour (its second `if` tests the captured
    /// copy) and it is the right one: it gives a freshly demoted peer a full
    /// retention window as a remembered peer instead of evicting it instantly.
    pub fn reconcile_connectivity(
        &mut self,
        now_unix_millis: u64,
        presence: &BTreeMap<PeerId, LinkPresence>,
    ) -> ConnectivityChanges {
        let mut changes = ConnectivityChanges::default();
        let snapshot: Vec<_> = self
            .peers
            .values()
            .map(|p| {
                let verified = p.verified_nickname || self.pins.contains_key(&p.peer);
                (
                    p.peer.clone(),
                    p.connected,
                    verified,
                    p.last_seen_ms,
                    p.nickname.clone(),
                    !verified && p.noise_public_key.is_some(),
                )
            })
            .collect();

        for (peer, was_connected, verified, last_seen, nickname, unproven) in snapshot {
            let age = now_unix_millis.saturating_sub(last_seen);
            let retention = reachability_retention_ms(verified);

            let live = presence.get(&peer).copied().unwrap_or_default();
            if unproven && age > retention && !live.any() {
                self.peers.remove(&peer);
                changes.removed.push(RemovedPeer { peer, nickname });
                continue;
            }

            if was_connected && age > PEER_INACTIVITY_TIMEOUT_MS {
                if !live.any() {
                    if let Some(record) = self.peers.get_mut(&peer) {
                        record.connected = false;
                    }
                    changes.disconnected.push(peer.clone());
                }
            }

            if !was_connected && age > retention && !live.any() {
                self.peers.remove(&peer);
                changes.removed.push(RemovedPeer { peer, nickname });
            }
        }

        changes
    }
}

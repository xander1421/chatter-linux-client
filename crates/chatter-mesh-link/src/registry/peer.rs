use std::collections::BTreeMap;

use chatter_mesh::ids::PeerId;
use chatter_mesh::tlv::announce::AnnouncementPacket;
use chatter_mesh::tlv::capabilities::PeerCapabilities;

use super::link_state::LinkPresence;

/// `TransportConfig.bleReachabilityRetentionVerifiedSeconds` (60 s).
///
/// How long a *verified* peer stays reachable after its last sighting. Verified
/// means we checked a signature against the signing key in its announce, so the
/// longer window cannot be claimed by simply asserting a nickname.
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
    /// The nickname arrived inside an announce whose signature we checked.
    /// Buys the longer reachability retention, and nothing else.
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
    /// This announce established the pin, so the caller has something new to
    /// persist.
    pub pinned_now: bool,
}

/// The keys a peer ID was first seen with.
///
/// Held apart from [`PeerRecord`] because the two have opposite lifetimes. A
/// record is presence: it ages out, and a peer that leaves is forgotten. A pin
/// is identity: if it went the same way, anything that makes us forget a peer
/// — silence, a restart, a `Leave` — would also reopen trust on first use for
/// its ID, and the next announce to claim it would be believed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PinnedIdentity {
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
pub(super) const PIN_BLOB_VERSION: u8 = 1;

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
        age <= reachability_retention_ms(record.verified_nickname)
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
        self.pins.contains_key(peer) || self.pins.len() < MAX_PINNED_IDENTITIES
    }

    /// Forget every pin. For a panic wipe, and nothing else.
    pub fn clear_pins(&mut self) {
        self.pins.clear();
    }

    /// The pin set, for the caller to seal and persist.
    ///
    /// Every field is length-prefixed with one byte, so a nickname is cut at
    /// 255 bytes — it is display text, and the announce TLV cannot carry more.
    pub fn pins_to_bytes(&self) -> Vec<u8> {
        fn field(out: &mut Vec<u8>, bytes: &[u8]) {
            let bytes = &bytes[..bytes.len().min(usize::from(u8::MAX))];
            out.push(bytes.len() as u8);
            out.extend_from_slice(bytes);
        }

        let mut out = vec![PIN_BLOB_VERSION];
        for (peer, pin) in &self.pins {
            field(&mut out, peer.as_str().as_bytes());
            field(&mut out, &pin.noise_public_key);
            field(&mut out, &pin.signing_public_key);
            field(&mut out, pin.nickname.as_bytes());
            out.extend_from_slice(&pin.pinned_at_ms.to_be_bytes());
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
            let noise_public_key = field(&mut rest)?.to_vec();
            let signing_public_key = field(&mut rest)?.to_vec();
            let nickname = String::from_utf8_lossy(field(&mut rest)?).into_owned();
            let (stamp, tail) = rest.split_first_chunk::<8>().ok_or(MalformedPins)?;
            rest = tail;
            // A pin filed under an ID its own key does not derive to could
            // never have been recorded by `on_announce`, so the blob is not
            // one we wrote.
            if PeerId::from_noise_key(&noise_public_key).as_str() != peer {
                return Err(MalformedPins);
            }
            loaded.push((
                PeerId::from_str(peer),
                PinnedIdentity {
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
            self.pins.entry(peer).or_insert(pin);
        }
        Ok(())
    }

    pub fn update_last_seen(&mut self, peer: &PeerId, now_unix_millis: u64) {
        if let Some(record) = self.peers.get_mut(peer) {
            record.last_seen_ms = now_unix_millis;
        }
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
        let pinned_now = !self.pins.contains_key(peer) && self.pins.len() < MAX_PINNED_IDENTITIES;
        if pinned_now {
            self.pins.insert(
                peer.clone(),
                PinnedIdentity {
                    noise_public_key: announcement.noise_public_key.clone(),
                    signing_public_key: announcement.signing_public_key.clone(),
                    nickname: announcement.nickname.clone(),
                    pinned_at_ms: now_unix_millis,
                },
            );
        }

        let existing = self.peers.get(peer);
        let update = AnnounceUpdate {
            is_new_peer: existing.is_none(),
            was_disconnected: existing.is_some_and(|e| !e.connected),
            previous_nickname: existing.map(|e| e.nickname.clone()),
            pinned_now,
        };

        self.peers.insert(
            peer.clone(),
            PeerRecord {
                peer: peer.clone(),
                nickname: announcement.nickname.clone(),
                connected,
                noise_public_key: Some(announcement.noise_public_key.clone()),
                signing_public_key: Some(announcement.signing_public_key.clone()),
                verified_nickname: true,
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
        let snapshot: Vec<(PeerId, bool, bool, u64, String)> = self
            .peers
            .values()
            .map(|p| {
                (
                    p.peer.clone(),
                    p.connected,
                    p.verified_nickname,
                    p.last_seen_ms,
                    p.nickname.clone(),
                )
            })
            .collect();

        for (peer, was_connected, verified, last_seen, nickname) in snapshot {
            let age = now_unix_millis.saturating_sub(last_seen);
            let retention = reachability_retention_ms(verified);

            if was_connected && age > PEER_INACTIVITY_TIMEOUT_MS {
                let live = presence.get(&peer).copied().unwrap_or_default();
                if !live.any() {
                    if let Some(record) = self.peers.get_mut(&peer) {
                        record.connected = false;
                    }
                    changes.disconnected.push(peer.clone());
                }
            }

            if !was_connected && age > retention {
                self.peers.remove(&peer);
                changes.removed.push(RemovedPeer { peer, nickname });
            }
        }

        changes
    }
}

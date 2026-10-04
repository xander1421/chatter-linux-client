//! One-time prekeys for forward-secret courier mail.
//!
//! The storage half of bitchat's `LocalPrekeyStore` and `PrekeyBundleStore`;
//! the wire format, signature and policy rules are `chatter_mesh_noise::prekey`.
//!
//! Courier mail sealed to our long-lived Noise static key can be read by anyone
//! who later takes that key, for as long as a copy survives in some courier's
//! bag. Sealed to a one-time prekey instead, it is forward secret once the
//! prekey is consumed and its grace window lapses. So we publish a signed bundle
//! of unconsumed prekeys, gossiped like an announce; a peer that verified our
//! announce caches it and seals each offline envelope to one of them, falling
//! back to the static seal only when it holds no fresh bundle for us.

use std::collections::{BTreeMap, BTreeSet};

use chatter_mesh::tlv::courier_envelope::CourierEnvelope;
use chatter_mesh_noise::identity::MeshIdentity;
use chatter_mesh_noise::prekey::{
    MAX_CACHED_BUNDLES, MAX_PREKEYS, Prekey, PrekeyBundle, bundle_is_sealable, bundle_supersedes,
    generate_prekey, needs_replenish, next_generated_at, prekey_can_open, prekey_is_expired,
    prekey_public,
};
use chatter_mesh_noise::seal::{open_courier, open_prekey, seal_courier, seal_prekey};
use zeroize::{Zeroize, Zeroizing};

use super::state::MeshState;

/// Leading bytes of the persisted blob, so a blob of another shape is refused
/// rather than misread as key material.
const MAGIC: &[u8; 4] = b"LPK1";

/// One of our prekeys. The secret is wiped on drop.
struct LocalPrekey {
    id: u32,
    secret: [u8; 32],
    created_at_ms: u64,
    consumed_at_ms: Option<u64>,
}

impl Drop for LocalPrekey {
    fn drop(&mut self) {
        self.secret.zeroize();
    }
}

/// Our own prekeys: the secrets behind the bundle we publish.
#[derive(Default)]
pub struct LocalPrekeys {
    keys: Vec<LocalPrekey>,
    next_id: u32,
    generated_at_ms: u64,
    /// A prekey was consumed since the last save.
    dirty: bool,
}

impl LocalPrekeys {
    /// Apply the store rules: drop expired keys, top the batch back up when it
    /// runs low, and advance the bundle stamp if anything changed.
    ///
    /// Returns whether the set changed and must be persisted, then republished
    /// — in that order, because a published prekey whose secret was never saved
    /// is mail that can never be opened.
    pub fn refresh(&mut self, now_ms: u64, rng: &mut impl rand_core_06::CryptoRngCore) -> bool {
        let before = self.keys.len();
        self.keys
            .retain(|key| !prekey_is_expired(key.created_at_ms, key.consumed_at_ms, now_ms));
        let mut changed = self.dirty || self.keys.len() != before;

        let mut unconsumed = self.unconsumed().count();
        if needs_replenish(unconsumed) {
            while unconsumed < MAX_PREKEYS {
                let id = self.next_id;
                self.next_id = self.next_id.wrapping_add(1);
                if self.keys.iter().any(|key| key.id == id) {
                    continue;
                }
                let (_, secret) = generate_prekey(id, rng);
                self.keys.push(LocalPrekey {
                    id,
                    secret,
                    created_at_ms: now_ms,
                    consumed_at_ms: None,
                });
                unconsumed += 1;
            }
            changed = true;
        }

        if changed {
            self.generated_at_ms = next_generated_at(self.generated_at_ms, now_ms);
        }
        changed
    }

    /// The state has been saved.
    pub fn mark_saved(&mut self) {
        self.dirty = false;
    }

    /// Our current bundle, signed: every unconsumed prekey, ascending by ID.
    pub fn bundle(&self, identity: &MeshIdentity) -> Option<PrekeyBundle> {
        let mut prekeys: Vec<Prekey> = self
            .unconsumed()
            .map(|key| Prekey {
                id: key.id,
                public_key: prekey_public(&key.secret),
            })
            .collect();
        if prekeys.is_empty() {
            return None;
        }
        prekeys.sort_by_key(|prekey| prekey.id);
        prekeys.truncate(MAX_PREKEYS);
        Some(PrekeyBundle::sign(
            identity.static_public(),
            prekeys,
            self.generated_at_ms,
            identity,
        ))
    }

    /// Open an envelope sealed to one of our prekeys, consuming it.
    ///
    /// A consumed prekey still opens inside its grace window: spray-and-wait
    /// delivers the same ciphertext through several couriers, days apart.
    pub fn open(
        &mut self,
        prekey_id: u32,
        ciphertext: &[u8],
        identity: &MeshIdentity,
        now_ms: u64,
    ) -> Option<(Vec<u8>, [u8; 32])> {
        let key = self.keys.iter_mut().find(|key| key.id == prekey_id)?;
        if !prekey_can_open(key.consumed_at_ms, now_ms) {
            return None;
        }
        let opened = open_prekey(ciphertext, prekey_id, &key.secret, identity).ok()?;
        if key.consumed_at_ms.is_none() {
            key.consumed_at_ms = Some(now_ms);
            self.dirty = true;
        }
        Some(opened)
    }

    fn unconsumed(&self) -> impl Iterator<Item = &LocalPrekey> {
        self.keys.iter().filter(|key| key.consumed_at_ms.is_none())
    }

    /// `MAGIC ‖ be_u32(next_id) ‖ be_u64(generated_at) ‖ be_u16(n) ‖
    /// (be_u32(id) ‖ secret32 ‖ be_u64(created) ‖ u8(consumed?) ‖ be_u64(at)) * n`
    pub fn to_bytes(&self) -> Zeroizing<Vec<u8>> {
        let mut out = Zeroizing::new(Vec::with_capacity(18 + self.keys.len() * 53));
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&self.next_id.to_be_bytes());
        out.extend_from_slice(&self.generated_at_ms.to_be_bytes());
        let count = u16::try_from(self.keys.len()).unwrap_or(u16::MAX);
        out.extend_from_slice(&count.to_be_bytes());
        for key in self.keys.iter().take(usize::from(count)) {
            out.extend_from_slice(&key.id.to_be_bytes());
            out.extend_from_slice(&key.secret);
            out.extend_from_slice(&key.created_at_ms.to_be_bytes());
            out.push(u8::from(key.consumed_at_ms.is_some()));
            out.extend_from_slice(&key.consumed_at_ms.unwrap_or(0).to_be_bytes());
        }
        out
    }

    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        let mut cursor = bytes.strip_prefix(MAGIC.as_slice())?;
        let mut take = |len: usize| -> Option<&[u8]> {
            if cursor.len() < len {
                return None;
            }
            let (head, rest) = cursor.split_at(len);
            cursor = rest;
            Some(head)
        };
        let next_id = u32::from_be_bytes(take(4)?.try_into().ok()?);
        let generated_at_ms = u64::from_be_bytes(take(8)?.try_into().ok()?);
        let count = u16::from_be_bytes(take(2)?.try_into().ok()?);
        let mut keys = Vec::with_capacity(usize::from(count));
        for _ in 0..count {
            let id = u32::from_be_bytes(take(4)?.try_into().ok()?);
            let secret: [u8; 32] = take(32)?.try_into().ok()?;
            let created_at_ms = u64::from_be_bytes(take(8)?.try_into().ok()?);
            let consumed = match take(1)? {
                [0] => false,
                [1] => true,
                _ => return None,
            };
            let at = u64::from_be_bytes(take(8)?.try_into().ok()?);
            keys.push(LocalPrekey {
                id,
                secret,
                created_at_ms,
                consumed_at_ms: consumed.then_some(at),
            });
        }
        if !cursor.is_empty() {
            return None;
        }
        Some(Self {
            keys,
            next_id,
            generated_at_ms,
            dirty: false,
        })
    }
}

/// Peers' verified bundles, keyed by owner static key.
#[derive(Default)]
pub struct PeerBundles {
    bundles: BTreeMap<[u8; 32], CachedBundle>,
}

struct CachedBundle {
    bundle: PrekeyBundle,
    /// Prekeys we have already sealed to, so each envelope gets its own.
    used: BTreeSet<u32>,
    updated_at_ms: u64,
}

impl PeerBundles {
    /// Cache a bundle whose signature the caller has verified against the
    /// owner's announce-pinned signing key. Strictly newer replaces; past
    /// [`MAX_CACHED_BUNDLES`] the least recently updated owner is evicted.
    pub fn ingest(&mut self, bundle: PrekeyBundle, now_ms: u64) -> bool {
        if let Some(cached) = self.bundles.get(&bundle.owner_static)
            && !bundle_supersedes(bundle.generated_at, cached.bundle.generated_at)
        {
            return false;
        }
        if !self.bundles.contains_key(&bundle.owner_static)
            && self.bundles.len() >= MAX_CACHED_BUNDLES
            && let Some(oldest) = self
                .bundles
                .iter()
                .min_by_key(|(_, cached)| cached.updated_at_ms)
                .map(|(owner, _)| *owner)
        {
            self.bundles.remove(&oldest);
        }
        self.bundles.insert(
            bundle.owner_static,
            CachedBundle {
                bundle,
                used: BTreeSet::new(),
                updated_at_ms: now_ms,
            },
        );
        true
    }

    /// A prekey of `owner` to seal one envelope to, if we hold a bundle fresh
    /// enough to seal to. Prefers one we have not used; once all are used, one
    /// is reused, which the owner's grace window tolerates and which is still
    /// better than the non-forward-secret static seal.
    pub fn take(&mut self, owner_static: &[u8; 32], now_ms: u64) -> Option<Prekey> {
        let cached = self.bundles.get_mut(owner_static)?;
        if !bundle_is_sealable(cached.bundle.generated_at, now_ms) {
            return None;
        }
        let prekey = cached
            .bundle
            .prekeys
            .iter()
            .find(|prekey| !cached.used.contains(&prekey.id))
            .or_else(|| cached.bundle.prekeys.first())
            .copied()?;
        cached.used.insert(prekey.id);
        Some(prekey)
    }
}

/// Both halves, behind the mesh state lock.
#[derive(Default)]
pub struct PrekeyState {
    /// `None` until restored from storage. Never generated in its place: a
    /// locked store that read as empty would otherwise overwrite secrets that
    /// published mail is sealed to.
    pub local: Option<LocalPrekeys>,
    pub peers: PeerBundles,
}

/// Seal a payload for a recipient who may be offline: to one of their one-time
/// prekeys when we hold a fresh verified bundle, else to their static key.
/// Returns the ciphertext and the prekey ID for the envelope.
pub fn seal_offline(
    state: &MeshState,
    payload: &[u8],
    recipient_key: &[u8; 32],
    identity: &MeshIdentity,
    now_ms: u64,
) -> Option<(Vec<u8>, Option<u32>)> {
    let prekey = state.with(|inner| inner.prekeys.peers.take(recipient_key, now_ms));
    match prekey {
        Some(prekey) => seal_prekey(payload, prekey.id, &prekey.public_key, identity)
            .ok()
            .map(|ciphertext| (ciphertext, Some(prekey.id))),
        None => seal_courier(payload, recipient_key, identity)
            .ok()
            .map(|ciphertext| (ciphertext, None)),
    }
}

/// Open an envelope if it is addressed to us, by whichever seal it names.
pub fn open_envelope(
    state: &MeshState,
    envelope: &CourierEnvelope,
    identity: &MeshIdentity,
    now_ms: u64,
) -> Option<(Vec<u8>, [u8; 32])> {
    match envelope.prekey_id {
        Some(id) => state.with(|inner| {
            inner
                .prekeys
                .local
                .as_mut()?
                .open(id, &envelope.ciphertext, identity, now_ms)
        }),
        None => open_courier(&envelope.ciphertext, identity).ok(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chatter_mesh_noise::prekey::{CONSUMED_GRACE_MS, MAX_BUNDLE_AGE_FOR_SEALING_MS, REPLENISH_THRESHOLD};
    use rand_core_06::OsRng;

    const NOW: u64 = 1_800_000_000_000;

    fn identity() -> MeshIdentity {
        MeshIdentity::generate(&mut OsRng)
    }

    fn fresh() -> LocalPrekeys {
        let mut local = LocalPrekeys::default();
        assert!(local.refresh(NOW, &mut OsRng));
        local
    }

    #[test]
    fn a_fresh_store_fills_to_a_full_signed_bundle() {
        let me = identity();
        let local = fresh();
        let bundle = local.bundle(&me).expect("bundle");
        assert_eq!(bundle.prekeys.len(), MAX_PREKEYS);
        assert_eq!(bundle.owner_static, me.static_public());
        assert!(bundle.verify(&me.signing_public()));
        assert_eq!(bundle.generated_at, NOW);
    }

    #[test]
    fn mail_sealed_to_a_bundle_opens_once_and_consumes_the_prekey() {
        let (me, sender) = (identity(), identity());
        let mut local = fresh();
        let mut peers = PeerBundles::default();
        assert!(peers.ingest(local.bundle(&me).unwrap(), NOW));

        let prekey = peers.take(&me.static_public(), NOW).expect("sealable");
        let sealed = seal_prekey(b"offline", prekey.id, &prekey.public_key, &sender).unwrap();
        let (payload, from) = local.open(prekey.id, &sealed, &me, NOW).expect("opens");
        assert_eq!(payload, b"offline");
        assert_eq!(from, sender.static_public());

        // Redelivery inside the grace window still opens; after it, never.
        assert!(local.open(prekey.id, &sealed, &me, NOW + CONSUMED_GRACE_MS).is_some());
        assert!(local.open(prekey.id, &sealed, &me, NOW + CONSUMED_GRACE_MS + 1).is_none());

        // The consumed prekey leaves the next bundle, under a newer stamp.
        assert!(local.refresh(NOW + 1, &mut OsRng));
        let next = local.bundle(&me).unwrap();
        assert!(next.prekeys.iter().all(|p| p.id != prekey.id));
        assert!(next.generated_at > NOW);
        assert!(peers.ingest(next, NOW + 1));
    }

    #[test]
    fn the_batch_is_replenished_below_the_threshold() {
        let (me, sender) = (identity(), identity());
        let mut local = fresh();
        let bundle = local.bundle(&me).unwrap();
        let consume = MAX_PREKEYS - REPLENISH_THRESHOLD + 1;
        for prekey in bundle.prekeys.iter().take(consume) {
            let sealed = seal_prekey(b"x", prekey.id, &prekey.public_key, &sender).unwrap();
            assert!(local.open(prekey.id, &sealed, &me, NOW).is_some());
        }
        assert!(local.refresh(NOW + 1, &mut OsRng));
        assert_eq!(local.bundle(&me).unwrap().prekeys.len(), MAX_PREKEYS);
        // Nothing changed since: no stamp bump, no republish.
        local.mark_saved();
        assert!(!local.refresh(NOW + 2, &mut OsRng));
    }

    #[test]
    fn the_store_round_trips_and_refuses_a_foreign_blob() {
        let me = identity();
        let local = fresh();
        let restored = LocalPrekeys::from_bytes(&local.to_bytes()).expect("round trip");
        assert_eq!(
            restored.bundle(&me).unwrap().prekeys,
            local.bundle(&me).unwrap().prekeys
        );
        assert!(LocalPrekeys::from_bytes(b"nope").is_none());
        let mut truncated = local.to_bytes().to_vec();
        truncated.pop();
        assert!(LocalPrekeys::from_bytes(&truncated).is_none());
    }

    #[test]
    fn peer_bundles_take_only_newer_and_only_fresh_enough() {
        let me = identity();
        let mut local = fresh();
        let mut peers = PeerBundles::default();
        let first = local.bundle(&me).unwrap();
        assert!(peers.ingest(first.clone(), NOW));
        assert!(!peers.ingest(first, NOW), "an equal stamp does not replace");

        // Each take is a distinct prekey until they run out.
        let ids: BTreeSet<u32> = (0..MAX_PREKEYS)
            .filter_map(|_| peers.take(&me.static_public(), NOW).map(|p| p.id))
            .collect();
        assert_eq!(ids.len(), MAX_PREKEYS);
        assert!(peers.take(&me.static_public(), NOW).is_some(), "then reused");

        assert!(
            peers
                .take(&me.static_public(), NOW + MAX_BUNDLE_AGE_FOR_SEALING_MS + 1)
                .is_none(),
            "a stale bundle is not sealed to"
        );
        local.dirty = true;
        assert!(local.refresh(NOW + 5, &mut OsRng));
        assert!(peers.ingest(local.bundle(&me).unwrap(), NOW + 5));
    }
}

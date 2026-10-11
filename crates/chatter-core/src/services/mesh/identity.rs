//! The mesh identity, and the Nostr identities derived from it.
//!
//! [`MeshIdentityStore`] owns the long-term keypair. This module owns the one
//! derivation on top of it: the device seed that `chatter_mesh_nostr` hashes
//! into a per-geohash secp256k1 identity.
//!
//! # Why derive the Nostr seed rather than store a second secret
//!
//! `chatter_mesh_nostr::identity::derive_for_geohash` takes a 32-byte device
//! seed and HMACs it with the cell, so the seed is never used as a key itself —
//! only as HMAC input. A separately stored secret would buy nothing over an
//! HKDF expansion of key material we already hold under the same SQLCipher
//! passphrase, and would add a second thing to wipe, migrate, and get wrong.
//!
//! What it must not be is the mesh static secret itself. That secret drives
//! Noise key agreement; feeding it into a second construction as raw input is
//! the same cross-protocol reuse the design doc rejects for the Signal key.
//! HKDF-Expand with a domain separator gives an independent pseudorandom value
//! from which the static secret cannot be recovered.
//!
//! [`MeshIdentityStore`]: crate::storage::mesh_identity_store::MeshIdentityStore

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;

use chatter_mesh_noise::MeshIdentity;
use chatter_mesh_nostr::NostrError;
use chatter_mesh_nostr::identity::{NostrIdentity, derive_for_bridge, derive_for_geohash};
use hkdf::Hkdf;
use sha2::Sha256;

/// Domain separator for the Nostr device seed.
const NOSTR_SEED_INFO: &[u8] = b"chatter-mesh/nostr-device-seed/v1";

/// Derive the Nostr device seed from the mesh identity.
///
/// Deterministic, so the geohash identities this device publishes under survive
/// a restart — a fresh key every launch would make every geohash message look
/// like a new participant.
#[must_use]
pub fn nostr_device_seed(identity: &MeshIdentity) -> [u8; 32] {
    // The signing secret rather than the static secret: the static secret is the
    // Noise DH key and has the strongest reason to stay confined to one
    // construction. Salting with the static *public* key binds the seed to this
    // identity as a whole without exposing either secret to a second use.
    let salt = identity.static_public();
    let hkdf = Hkdf::<Sha256>::new(Some(&salt[..]), &identity.signing_secret()[..]);
    let mut seed = [0u8; 32];
    // `expand` only fails when the output length exceeds 255 * HashLen, which
    // 32 bytes cannot. Falling back to the salted input would be worse than
    // useless, so an unreachable failure leaves the zeroed buffer — and the
    // assertion below is what would catch it in test.
    if hkdf.expand(NOSTR_SEED_INFO, &mut seed).is_err() {
        debug_assert!(false, "HKDF-Expand of 32 bytes cannot fail");
    }
    seed
}

/// The set of Nostr identities this device speaks as, one per geohash cell.
///
/// Cells are entered and left as the user moves, and the geohash identity is
/// the unlinkability boundary: two cells must not share a pubkey, or anyone
/// watching relays can join a user's movements into a track. Caching them is
/// purely to avoid recomputing an HMAC, never to reuse one across cells.
pub struct NostrIdentities {
    seed: [u8; 32],
    per_geohash: BTreeMap<String, NostrIdentity>,
    per_bridge: BTreeMap<String, NostrIdentity>,
}

impl NostrIdentities {
    #[must_use]
    pub fn new(seed: [u8; 32]) -> Self {
        Self {
            seed,
            per_geohash: BTreeMap::new(),
            per_bridge: BTreeMap::new(),
        }
    }

    /// From the mesh identity directly.
    #[must_use]
    pub fn from_mesh(identity: &MeshIdentity) -> Self {
        Self::new(nostr_device_seed(identity))
    }

    /// The chat identity for a geohash cell, deriving it on first use.
    pub fn for_geohash(&mut self, cell: &str) -> Result<&NostrIdentity, NostrError> {
        match self.per_geohash.entry(cell.to_string()) {
            Entry::Occupied(slot) => Ok(slot.into_mut()),
            Entry::Vacant(slot) => Ok(slot.insert(derive_for_geohash(&self.seed, cell)?)),
        }
    }

    /// The mesh-bridge rendezvous identity for a cell.
    ///
    /// Deliberately distinct from [`Self::for_geohash`] for the same cell: the
    /// bridge identity is what a gateway publishes under, and sharing it with
    /// the chat identity would link "who is bridging" to "who is chatting here".
    pub fn for_bridge(&mut self, cell: &str) -> Result<&NostrIdentity, NostrError> {
        match self.per_bridge.entry(cell.to_string()) {
            Entry::Occupied(slot) => Ok(slot.into_mut()),
            Entry::Vacant(slot) => Ok(slot.insert(derive_for_bridge(&self.seed, cell)?)),
        }
    }

    /// Every geohash chat identity currently derived, as `(cell, pubkey hex)`.
    #[must_use]
    pub fn geohash_pubkeys(&self) -> Vec<(String, String)> {
        self.per_geohash
            .iter()
            .map(|(cell, id)| (cell.clone(), id.public_key_hex().to_string()))
            .collect()
    }

    /// Forget a cell's identities. Called on leave, so that a cell the user has
    /// walked out of stops being derivable from live state.
    pub fn forget(&mut self, cell: &str) {
        self.per_geohash.remove(cell);
        self.per_bridge.remove(cell);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::os_rng;

    fn identity() -> MeshIdentity {
        MeshIdentity::generate(&mut os_rng())
    }

    #[test]
    fn the_seed_is_deterministic_for_one_identity() {
        let id = identity();
        assert_eq!(nostr_device_seed(&id), nostr_device_seed(&id));
    }

    #[test]
    fn the_seed_is_not_the_signing_secret_and_not_the_static_secret() {
        let id = identity();
        let seed = nostr_device_seed(&id);
        assert_ne!(&seed, id.signing_secret());
        assert_ne!(&seed, id.static_secret());
        assert_ne!(seed, [0u8; 32]);
    }

    #[test]
    fn distinct_identities_get_distinct_seeds() {
        assert_ne!(
            nostr_device_seed(&identity()),
            nostr_device_seed(&identity())
        );
    }

    #[test]
    fn geohash_identities_differ_between_cells() -> Result<(), NostrError> {
        let mut identities = NostrIdentities::from_mesh(&identity());
        let a = identities
            .for_geohash("u4pruy")?
            .public_key_hex()
            .to_string();
        let b = identities
            .for_geohash("u4pruz")?
            .public_key_hex()
            .to_string();
        assert_ne!(a, b, "two cells must not share a pubkey");
        // Cached, so the same cell keeps the same key.
        assert_eq!(a, identities.for_geohash("u4pruy")?.public_key_hex());
        Ok(())
    }

    #[test]
    fn the_bridge_identity_is_unlinkable_from_the_chat_identity() -> Result<(), NostrError> {
        let mut identities = NostrIdentities::from_mesh(&identity());
        let chat = identities
            .for_geohash("u4pruy")?
            .public_key_hex()
            .to_string();
        let bridge = identities
            .for_bridge("u4pruy")?
            .public_key_hex()
            .to_string();
        assert_ne!(chat, bridge);
        Ok(())
    }

    #[test]
    fn forgetting_a_cell_drops_both_identities() -> Result<(), NostrError> {
        let mut identities = NostrIdentities::from_mesh(&identity());
        identities.for_geohash("u4pruy")?;
        identities.for_bridge("u4pruy")?;
        assert_eq!(identities.geohash_pubkeys().len(), 1);
        identities.forget("u4pruy");
        assert!(identities.geohash_pubkeys().is_empty());
        Ok(())
    }
}

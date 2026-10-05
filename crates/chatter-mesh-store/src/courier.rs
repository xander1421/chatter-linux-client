//! The mailbag: sealed envelopes carried on behalf of people we may never meet.
//!
//! Ported from `Services/Courier/CourierStore.swift` (629 lines).
//!
//! An envelope is opaque ciphertext addressed only by a 16-byte rotating tag. This
//! store never learns the sender, never learns the recipient, and never learns the
//! content — the only identity it holds is the *depositor's* Noise key, and that
//! only because per-depositor quotas cannot exist without it. Nothing here should
//! ever grow a field that would let it learn more.
//!
//! Store and forward here is deposit and take only: an envelope is carried until
//! its recipient is met on an authenticated link, or until it expires. This store
//! does not spray copies to other couriers, hand over speculatively to recipients
//! heard through relays, or park envelopes on Nostr relays.
//!
//! # The properties this module is responsible for
//!
//! **1. The pool is bounded per depositor and in total.** A signature-verified
//! peer (the caller decides that; unverified peers may not deposit at all) may
//! have at most [`MAX_PER_DEPOSITOR`] envelopes resting here, in a pool of
//! [`MAX_ENVELOPES`]. A deposit into a full pool is refused rather than evicting
//! anyone else's mail.
//!
//! **2. Handover requires a Noise-authenticated *ingress link*, not merely a session
//! for the claimed peer ID.** A peer-level session can outlive the physical link it
//! was established on, and a replayed announce can rebind an attacker's link to the
//! victim's ID — so "a session exists for this peer" hands the mail to the attacker.
//! Only a *direct*, signature-verified announce arriving on a link that is still
//! Noise-authenticated releases an envelope; see [`AnnounceIngress`]. A relayed
//! announce releases nothing.
//!
//! **3. Copy budgets are capped.** The wire envelope's `copies` field is clamped to
//! `MAX_COPIES` (8) on deposit and carried unchanged. Nothing here spends or grows
//! it, so this store cannot be turned into an amplifier.
//!
//! # This crate cannot verify anything itself
//!
//! No signature check and no Noise state reaches this module, by design. The
//! authentication facts arrive as an [`AnnounceIngress`] the caller *asserts*, and
//! whether a depositor is verified at all is the caller's decision. The store's job
//! is to enforce resource bounds and the release rule on top of those assertions,
//! not to second-guess them.
//!
//! # Sans-IO
//!
//! Time is always a `now_unix_secs: u64` parameter — envelope expiries are in
//! milliseconds, which is the wire unit, and the conversion happens here. Durability
//! goes through the injected [`Storage`], which only ever sees ciphertext.
//!
//! # Deliberately not ported
//!
//! Swift's `retryDeferredPersistence` / `resolveDeferredLoadLocked` / `merge`
//! reconcile an in-memory pool with a durable snapshot that was unreadable at launch
//! — iOS file protection before first unlock. That transition lives behind the
//! [`Storage`] seam here, whose contract already says an unreadable-yet blob is
//! `Ok(None)`, so the store never sees the half-state that machinery exists to fix.
//! The one piece of `merge` that is policy rather than plumbing — re-applying every
//! bound to a pool this process did not build under the current limits — is ported,
//! as [`CourierStore::restore`]'s bound check.

use std::collections::BTreeMap;

use chatter_mesh::tlv::courier_envelope::{
    CourierEnvelope, MAX_CIPHERTEXT_BYTES, MAX_COPIES, MAX_LIFETIME_SECONDS, TAG_LEN,
    candidate_tags,
};

use crate::error::StoreError;
use crate::storage::{Storage, StorageKey};

/// Total envelopes carried at once. Swift: `Limits.maxVerifiedEnvelopes` (line 104).
pub const MAX_ENVELOPES: usize = 20;
/// Envelopes one signature-verified peer may have resting here.
/// Swift: `Limits.maxPerVerifiedDepositor` (line 106).
pub const MAX_PER_DEPOSITOR: usize = 2;
/// Slack allowed on top of the 24 h lifetime for depositor clock skew. An expiry
/// beyond this is a skew or abuse attempt and the deposit is refused.
/// Swift: `Limits.maxExpirySlack` (line 108).
pub const MAX_EXPIRY_SLACK_SECS: u64 = 60 * 60;

/// Copy budget a freshly sealed envelope advertises on the wire. This store never
/// spends it; it is set for peers running implementations that spray.
/// Swift: `TransportConfig.courierInitialCopies` (TransportConfig.swift line 347).
pub const INITIAL_COPIES: u8 = 4;

const MILLIS_PER_SEC: u64 = 1_000;

/// Longest byte string accepted as a peer's Noise static key. Real ones are 32
/// bytes; the bound exists so an absurd key cannot bloat a quota bucket or the
/// durable pool, whose length prefixes are sized for plausible keys.
const MAX_PEER_KEY_BYTES: usize = 64;

/// Whether a byte string could be a peer's Noise static key at all. Identity is
/// the caller's to establish — this is a shape check, not an authentication one.
fn is_plausible_peer_key(key: &[u8]) -> bool {
    !key.is_empty() && key.len() <= MAX_PEER_KEY_BYTES
}

fn unix_millis(now_unix_secs: u64) -> u64 {
    now_unix_secs.saturating_mul(MILLIS_PER_SEC)
}

/// What the caller **asserts** about the announce that brought a peer's Noise
/// static key to our attention, and about the link it arrived on.
///
/// Both facts are needed and neither can be checked here:
///
/// - `verified_direct_announce` — the announce's signature verified against the
///   peer's announced signing key **and** it arrived without being relayed.
/// - `link_is_noise_authenticated_now` — the physical link it arrived on is,
///   *at this instant*, still Noise-authenticated for that same peer.
///
/// Only both together release an envelope. "A Noise session exists for this peer
/// ID" is explicitly not enough: a session outlives the link it was established
/// on, and a replayed announce can rebind an attacker's link to the victim's ID,
/// so a session-level check hands the mail to the attacker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AnnounceIngress {
    verified_direct_announce: bool,
    link_is_noise_authenticated_now: bool,
}

impl AnnounceIngress {
    /// Both facts stated explicitly. The named constructors below are clearer at a
    /// call site; this exists for callers computing the facts from predicates.
    pub const fn new(
        verified_direct_announce: bool,
        link_is_noise_authenticated_now: bool,
    ) -> Self {
        Self {
            verified_direct_announce,
            link_is_noise_authenticated_now,
        }
    }

    /// The only ingress that releases mail.
    pub const fn verified_direct_on_authenticated_link() -> Self {
        Self::new(true, true)
    }

    /// A verified announce that reached us through at least one relay. Speculative
    /// copies only — the original stays carried.
    pub const fn relayed() -> Self {
        Self::new(false, false)
    }

    /// A direct-looking announce whose link has not (yet) proved possession of the
    /// peer's Noise key. Treated exactly like a relayed one: this is the case the
    /// replayed-announce attack produces.
    pub const fn direct_but_link_not_authenticated() -> Self {
        Self::new(true, false)
    }

    pub const fn verified_direct_announce(self) -> bool {
        self.verified_direct_announce
    }

    pub const fn link_is_noise_authenticated_now(self) -> bool {
        self.link_is_noise_authenticated_now
    }

    /// Whether this ingress is strong enough to hand an envelope over and forget it.
    pub const fn releases_mail(self) -> bool {
        self.verified_direct_announce && self.link_is_noise_authenticated_now
    }
}

/// Outcome of an attempted direct handover.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Handover {
    /// The ingress link was authenticated. `offered` envelopes were presented to the
    /// transport and `accepted` of them left the device and were removed from the
    /// mailbag. A rejected offer stays carried for the next authenticated encounter.
    Released { offered: usize, accepted: usize },
    /// The announce was relayed, or its link is not Noise-authenticated right now.
    /// Nothing was offered and nothing left the mailbag.
    RefusedUnauthenticatedIngress,
}

impl Handover {
    pub const fn accepted(self) -> usize {
        match self {
            Handover::Released { accepted, .. } => accepted,
            Handover::RefusedUnauthenticatedIngress => 0,
        }
    }

    pub const fn is_refused(self) -> bool {
        matches!(self, Handover::RefusedUnauthenticatedIngress)
    }
}

/// One envelope in the bag, plus the bookkeeping the wire format does not carry.
#[derive(Clone, Debug, PartialEq, Eq)]
struct StoredEnvelope {
    recipient_tag: [u8; TAG_LEN],
    expiry_ms: u64,
    ciphertext: Vec<u8>,
    /// Whose quota this envelope occupies. The only identity in the store.
    depositor_noise_key: Vec<u8>,
    stored_at_unix_secs: u64,
    /// The wire copy budget, clamped and carried unchanged.
    copies: u8,
    prekey_id: Option<u32>,
}

impl StoredEnvelope {
    fn envelope(&self) -> CourierEnvelope {
        CourierEnvelope::new(
            self.recipient_tag,
            self.expiry_ms,
            self.ciphertext.clone(),
            self.copies,
            self.prekey_id,
        )
    }

    fn is_expired(&self, now_ms: u64) -> bool {
        now_ms >= self.expiry_ms
    }
}

/// Envelopes this device is carrying for offline third parties.
///
/// Generic over the persistence seam rather than holding a trait object, so a test
/// can drive it with [`crate::MemoryStorage`] and the integration layer can hand it
/// a SQLCipher-backed one without either paying for dynamic dispatch.
///
/// Construction does no IO: [`CourierStore::new`] starts empty and
/// [`CourierStore::restore`] is the explicit load.
#[derive(Debug)]
pub struct CourierStore<S: Storage> {
    storage: S,
    envelopes: Vec<StoredEnvelope>,
}

impl<S: Storage> CourierStore<S> {
    /// An empty mailbag over `storage`. Does not read anything — call
    /// [`CourierStore::restore`] for that.
    pub fn new(storage: S) -> Self {
        Self {
            storage,
            envelopes: Vec::new(),
        }
    }

    pub fn storage(&self) -> &S {
        &self.storage
    }

    /// Recover the storage, e.g. to hand it to a replacement store.
    pub fn into_storage(self) -> S {
        self.storage
    }

    /// Load the durable pool, drop anything expired, and re-apply every bound.
    ///
    /// Returns the number of envelopes now carried. A malformed blob is reported as
    /// [`StoreError::Malformed`] and leaves the bag empty — losing carried mail is
    /// bad, refusing to start is worse, so callers may log and continue.
    ///
    /// Bounds are re-applied on load (Swift does the same after a deferred-load
    /// merge, lines 580–602) because a blob that predates a tightened quota, or one
    /// that was tampered with on disk, must not be able to smuggle the pool past
    /// its caps.
    pub fn restore(&mut self, now_unix_secs: u64) -> Result<usize, StoreError> {
        self.envelopes.clear();
        let Some(bytes) = self.storage.load(StorageKey::CourierPool)? else {
            return Ok(0);
        };
        match decode_pool(&bytes) {
            Ok(envelopes) => {
                self.envelopes = envelopes;
                let pruned = self.prune_locked(now_unix_secs);
                let bounded = self.enforce_bounds();
                if pruned > 0 || bounded > 0 {
                    self.persist()?;
                }
                Ok(self.envelopes.len())
            }
            Err(error) => Err(error),
        }
    }

    /// Fast path so hot code — announce handling — can skip tag computation.
    pub fn is_empty(&self) -> bool {
        self.envelopes.is_empty()
    }

    /// Envelopes currently carried, for a "carrying mail" indicator.
    pub fn carried_count(&self) -> usize {
        self.envelopes.len()
    }

    /// Whether any carried envelope is addressed to this Noise static key. Read
    /// only, so the caller can decide to prove the link before a handover.
    pub fn carries_for(&self, recipient_noise_static_key: &[u8], now_unix_secs: u64) -> bool {
        if self.envelopes.is_empty() {
            return false;
        }
        let tags = candidate_tags(recipient_noise_static_key, now_unix_secs);
        self.envelopes
            .iter()
            .any(|stored| tags.contains(&stored.recipient_tag))
    }

    // MARK: - Depositing (courier side)

    /// Accept an envelope from a depositor.
    ///
    /// Trust policy — whether a depositor may deposit at all — is the caller's
    /// decision; this store only enforces resource bounds on top of it. A refusal
    /// is [`StoreError::DepositRefused`] and is not a failure: the pool is bounded
    /// on purpose and refusing is how the bound is enforced.
    ///
    /// Depositing an envelope whose ciphertext is already carried is idempotent: it
    /// succeeds without consuming a second slot, keeping the larger copy budget.
    pub fn deposit(
        &mut self,
        envelope: &CourierEnvelope,
        depositor_noise_key: &[u8],
        now_unix_secs: u64,
    ) -> Result<(), StoreError> {
        // Quotas are keyed on the depositor, so an empty key would let every
        // anonymous deposit share — and exhaust — one bucket.
        if !is_plausible_peer_key(depositor_noise_key) {
            return Err(StoreError::DepositRefused("no depositor key"));
        }
        if envelope.ciphertext.is_empty() {
            return Err(StoreError::DepositRefused("empty ciphertext"));
        }
        if envelope.ciphertext.len() > MAX_CIPHERTEXT_BYTES {
            return Err(StoreError::DepositRefused("ciphertext over 16 KiB"));
        }

        let now_ms = unix_millis(now_unix_secs);
        if envelope.is_expired(now_ms) {
            return Err(StoreError::DepositRefused("already expired"));
        }
        // An expiry beyond the policy lifetime plus clock-skew slack is a depositor
        // trying to pin storage for longer than the outbox would retain the message
        // itself. Swift lines 184–187.
        let max_expiry_ms = unix_millis(
            now_unix_secs
                .saturating_add(MAX_LIFETIME_SECONDS)
                .saturating_add(MAX_EXPIRY_SLACK_SECS),
        );
        if envelope.expiry_ms > max_expiry_ms {
            return Err(StoreError::DepositRefused("expiry beyond maximum lifetime"));
        }

        self.prune_locked(now_unix_secs);

        // The hard cap, applied here as well as in the envelope constructor: the
        // struct's fields are public, so a budget of 200 can reach us without going
        // through the wire clamp. The excess is refused, not honoured.
        let copies = envelope.copies.clamp(1, MAX_COPIES);

        let duplicate = self
            .envelopes
            .iter()
            .position(|stored| stored.ciphertext == envelope.ciphertext);
        if let Some(index) = duplicate {
            if let Some(stored) = self.envelopes.get_mut(index) {
                stored.copies = stored.copies.max(copies);
            }
            self.persist()?;
            return Ok(());
        }

        let already_deposited = self
            .envelopes
            .iter()
            .filter(|stored| stored.depositor_noise_key == depositor_noise_key)
            .count();
        if already_deposited >= MAX_PER_DEPOSITOR {
            return Err(StoreError::DepositRefused("per-depositor quota reached"));
        }

        // Refused rather than evicting: a crowd of strangers must not be able to
        // push out mail already being carried.
        if self.envelopes.len() >= MAX_ENVELOPES {
            return Err(StoreError::DepositRefused("courier pool full"));
        }

        self.envelopes.push(StoredEnvelope {
            recipient_tag: envelope.recipient_tag,
            expiry_ms: envelope.expiry_ms,
            ciphertext: envelope.ciphertext.clone(),
            depositor_noise_key: depositor_noise_key.to_vec(),
            stored_at_unix_secs: now_unix_secs,
            copies,
            prekey_id: envelope.prekey_id,
        });
        self.persist()
    }

    // MARK: - Handover (on encountering a peer)

    /// Remove and return every envelope addressed to this peer, accepting every
    /// offer. Convenience over [`CourierStore::handover_envelopes`]; transport
    /// callers should use that one so a failed send leaves the mail durable.
    ///
    /// Carries the same ingress requirement, so it cannot be used to sidestep it.
    pub fn take_envelopes(
        &mut self,
        recipient_noise_static_key: &[u8],
        now_unix_secs: u64,
        ingress: AnnounceIngress,
    ) -> Result<Vec<CourierEnvelope>, StoreError> {
        let mut taken = Vec::new();
        self.handover_envelopes(
            recipient_noise_static_key,
            now_unix_secs,
            ingress,
            |envelope| {
                taken.push(envelope.clone());
                true
            },
        )?;
        Ok(taken)
    }

    /// Hand carried mail to the peer it is addressed to.
    ///
    /// **This is the destructive path and it is gated.** `ingress` is the caller's
    /// assertion that the announce was signature-verified, *direct*, and arrived on
    /// a link that is still Noise-authenticated for that peer. Anything weaker
    /// returns [`Handover::RefusedUnauthenticatedIngress`] having touched nothing.
    ///
    /// The durable copy is retired only once `accept` confirms the transport put it
    /// on the intended peer's physical link. A failed encode, a stale binding, or
    /// backpressure leaves the envelope unchanged for the next authenticated
    /// encounter.
    pub fn handover_envelopes<F>(
        &mut self,
        recipient_noise_static_key: &[u8],
        now_unix_secs: u64,
        ingress: AnnounceIngress,
        mut accept: F,
    ) -> Result<Handover, StoreError>
    where
        F: FnMut(&CourierEnvelope) -> bool,
    {
        if !ingress.releases_mail() {
            return Ok(Handover::RefusedUnauthenticatedIngress);
        }

        if self.prune_locked(now_unix_secs) > 0 {
            self.persist()?;
        }

        let tags = candidate_tags(recipient_noise_static_key, now_unix_secs);
        let offered: Vec<CourierEnvelope> = self
            .envelopes
            .iter()
            .filter(|stored| tags.contains(&stored.recipient_tag))
            .map(StoredEnvelope::envelope)
            .collect();

        let mut accepted = 0usize;
        for envelope in &offered {
            if !accept(envelope) {
                continue;
            }
            let carried = self
                .envelopes
                .iter()
                .position(|stored| stored.ciphertext == envelope.ciphertext);
            if let Some(index) = carried {
                self.envelopes.remove(index);
                accepted += 1;
            }
        }
        if accepted > 0 {
            self.persist()?;
        }

        Ok(Handover::Released {
            offered: offered.len(),
            accepted,
        })
    }

    // MARK: - Maintenance

    /// Drop everything past its expiry. Returns how many were dropped.
    pub fn prune_expired(&mut self, now_unix_secs: u64) -> Result<usize, StoreError> {
        let pruned = self.prune_locked(now_unix_secs);
        if pruned > 0 {
            self.persist()?;
        }
        Ok(pruned)
    }

    /// Panic wipe: forget every carried envelope, in memory and on disk.
    pub fn wipe(&mut self) -> Result<(), StoreError> {
        self.envelopes.clear();
        self.storage.wipe(StorageKey::CourierPool)
    }

    // MARK: - Internals

    fn prune_locked(&mut self, now_unix_secs: u64) -> usize {
        let now_ms = unix_millis(now_unix_secs);
        let before = self.envelopes.len();
        self.envelopes.retain(|stored| !stored.is_expired(now_ms));
        before - self.envelopes.len()
    }

    /// Re-apply every bound to the current pool, oldest-first. Returns how many
    /// envelopes were shed. Used after a restore, where the blob's shape is not
    /// something this process produced under the current limits.
    fn enforce_bounds(&mut self) -> usize {
        let before = self.envelopes.len();
        self.envelopes
            .sort_by_key(|stored| stored.stored_at_unix_secs);

        let mut per_depositor: BTreeMap<Vec<u8>, usize> = BTreeMap::new();
        self.envelopes.retain(|stored| {
            let count = per_depositor
                .entry(stored.depositor_noise_key.clone())
                .or_insert(0);
            if *count >= MAX_PER_DEPOSITOR {
                return false;
            }
            *count += 1;
            true
        });

        // Keep the oldest, matching the deposit path, which refuses rather than
        // evicts once the pool is full.
        self.envelopes.truncate(MAX_ENVELOPES);

        before - self.envelopes.len()
    }

    fn persist(&self) -> Result<(), StoreError> {
        if self.envelopes.is_empty() {
            return self.storage.wipe(StorageKey::CourierPool);
        }
        self.storage
            .save(StorageKey::CourierPool, &encode_pool(&self.envelopes))
    }
}

// MARK: - Durable encoding
//
// Hand-rolled rather than serde: this crate's dependency list is `chatter-mesh`,
// `chatter-mesh-noise` and `thiserror`, and a courier pool is a fixed set of scalar
// fields. Everything written here is ciphertext or bookkeeping about ciphertext —
// the `Storage` seam never sees plaintext.

const POOL_MAGIC: &[u8; 4] = b"CRP2";

fn malformed() -> StoreError {
    StoreError::Malformed("courier pool")
}

fn encode_pool(envelopes: &[StoredEnvelope]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + envelopes.len() * 128);
    out.extend_from_slice(POOL_MAGIC);
    out.extend_from_slice(&(envelopes.len() as u32).to_be_bytes());

    for stored in envelopes {
        out.extend_from_slice(&stored.recipient_tag);
        out.extend_from_slice(&stored.expiry_ms.to_be_bytes());

        out.extend_from_slice(&(stored.ciphertext.len() as u32).to_be_bytes());
        out.extend_from_slice(&stored.ciphertext);

        out.extend_from_slice(&(stored.depositor_noise_key.len() as u16).to_be_bytes());
        out.extend_from_slice(&stored.depositor_noise_key);

        out.extend_from_slice(&stored.stored_at_unix_secs.to_be_bytes());
        out.push(stored.copies);

        match stored.prekey_id {
            Some(id) => {
                out.push(1);
                out.extend_from_slice(&id.to_be_bytes());
            }
            None => out.push(0),
        }
    }
    out
}

fn decode_pool(bytes: &[u8]) -> Result<Vec<StoredEnvelope>, StoreError> {
    let mut cursor = Cursor::new(bytes);
    if cursor.take(4)? != &POOL_MAGIC[..] {
        return Err(malformed());
    }
    let count = cursor.u32()? as usize;
    // A length prefix is attacker-influenced only after a disk compromise, but a
    // hostile count must still not make us reserve gigabytes.
    if count > MAX_ENVELOPES {
        return Err(malformed());
    }

    let mut envelopes = Vec::with_capacity(count);
    for _ in 0..count {
        let recipient_tag =
            <[u8; TAG_LEN]>::try_from(cursor.take(TAG_LEN)?).map_err(|_| malformed())?;
        let expiry_ms = cursor.u64()?;

        let ciphertext_len = cursor.u32()? as usize;
        if ciphertext_len == 0 || ciphertext_len > MAX_CIPHERTEXT_BYTES {
            return Err(malformed());
        }
        let ciphertext = cursor.take(ciphertext_len)?.to_vec();

        let depositor_len = cursor.u16()? as usize;
        if depositor_len == 0 || depositor_len > MAX_PEER_KEY_BYTES {
            return Err(malformed());
        }
        let depositor_noise_key = cursor.take(depositor_len)?.to_vec();

        let stored_at_unix_secs = cursor.u64()?;
        let copies = cursor.u8()?.clamp(1, MAX_COPIES);
        let prekey_id = match cursor.u8()? {
            0 => None,
            _ => Some(cursor.u32()?),
        };

        envelopes.push(StoredEnvelope {
            recipient_tag,
            expiry_ms,
            ciphertext,
            depositor_noise_key,
            stored_at_unix_secs,
            copies,
            prekey_id,
        });
    }

    if !cursor.is_empty() {
        return Err(malformed());
    }
    Ok(envelopes)
}

/// Bounds-checked forward cursor. Nothing here indexes, so nothing here panics.
struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn is_empty(&self) -> bool {
        self.pos >= self.buf.len()
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], StoreError> {
        let end = self.pos.checked_add(n).ok_or_else(malformed)?;
        let slice = self.buf.get(self.pos..end).ok_or_else(malformed)?;
        self.pos = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, StoreError> {
        self.take(1)?.first().copied().ok_or_else(malformed)
    }

    fn u16(&mut self) -> Result<u16, StoreError> {
        let raw = <[u8; 2]>::try_from(self.take(2)?).map_err(|_| malformed())?;
        Ok(u16::from_be_bytes(raw))
    }

    fn u32(&mut self) -> Result<u32, StoreError> {
        let raw = <[u8; 4]>::try_from(self.take(4)?).map_err(|_| malformed())?;
        Ok(u32::from_be_bytes(raw))
    }

    fn u64(&mut self) -> Result<u64, StoreError> {
        let raw = <[u8; 8]>::try_from(self.take(8)?).map_err(|_| malformed())?;
        Ok(u64::from_be_bytes(raw))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::MemoryStorage;
    use chatter_mesh::tlv::courier_envelope::{epoch_day, recipient_tag};

    const NOW: u64 = 1_750_000_000;
    const HOUR: u64 = 60 * 60;

    fn key(byte: u8) -> Vec<u8> {
        vec![byte; 32]
    }

    /// Envelopes are identified by ciphertext, so every fixture needs a distinct
    /// one or the store treats them as the same mail.
    fn envelope_at(
        recipient: &[u8],
        nonce: u32,
        lifetime_secs: u64,
        copies: u8,
        now: u64,
    ) -> CourierEnvelope {
        let mut ciphertext = vec![0x5A; 96];
        ciphertext[..4].copy_from_slice(&nonce.to_be_bytes());
        CourierEnvelope::new(
            recipient_tag(recipient, epoch_day(now)),
            (now + lifetime_secs) * 1000,
            ciphertext,
            copies,
            None,
        )
    }

    fn envelope(recipient: &[u8], nonce: u32) -> CourierEnvelope {
        envelope_at(recipient, nonce, HOUR, 1, NOW)
    }

    fn store() -> CourierStore<MemoryStorage> {
        CourierStore::new(MemoryStorage::new())
    }

    fn direct() -> AnnounceIngress {
        AnnounceIngress::verified_direct_on_authenticated_link()
    }

    // MARK: - Deposit and handover

    #[test]
    fn deposit_then_hand_over_to_the_recipient() {
        let mut store = store();
        let recipient = key(0xB0);
        let mail = envelope(&recipient, 1);

        store.deposit(&mail, &key(0xA1), NOW).expect("deposit");

        let taken = store
            .take_envelopes(&recipient, NOW, direct())
            .expect("take");
        assert_eq!(taken, vec![mail]);
        // Delivery removes the envelope.
        assert!(store.is_empty());
        assert!(
            store
                .take_envelopes(&recipient, NOW, direct())
                .expect("take")
                .is_empty()
        );
    }

    #[test]
    fn carries_for_names_only_the_recipient_and_takes_nothing() {
        let mut store = store();
        assert!(!store.carries_for(&key(0xB0), NOW));
        store
            .deposit(&envelope(&key(0xB0), 1), &key(0xA1), NOW)
            .expect("deposit");
        assert!(store.carries_for(&key(0xB0), NOW));
        assert!(!store.carries_for(&key(0xCC), NOW));
        assert_eq!(store.carried_count(), 1);
    }

    #[test]
    fn handover_ignores_other_recipients() {
        let mut store = store();
        store
            .deposit(&envelope(&key(0xB0), 1), &key(0xA1), NOW)
            .expect("deposit");

        assert!(
            store
                .take_envelopes(&key(0xCC), NOW, direct())
                .expect("take")
                .is_empty()
        );
        assert_eq!(
            store
                .take_envelopes(&key(0xB0), NOW, direct())
                .expect("take")
                .len(),
            1
        );
    }

    #[test]
    fn a_rejected_handover_retains_the_envelope_until_an_accepted_retry() {
        let mut store = store();
        let recipient = key(0xB0);
        let mail = envelope(&recipient, 1);
        store.deposit(&mail, &key(0xA1), NOW).expect("deposit");

        let mut offers = Vec::new();
        let outcome = store
            .handover_envelopes(&recipient, NOW, direct(), |offered| {
                offers.push(offered.clone());
                false
            })
            .expect("handover");
        assert_eq!(
            outcome,
            Handover::Released {
                offered: 1,
                accepted: 0
            }
        );
        assert_eq!(offers, vec![mail.clone()]);
        assert!(!store.is_empty());

        let outcome = store
            .handover_envelopes(&recipient, NOW, direct(), |_| true)
            .expect("handover");
        assert_eq!(outcome.accepted(), 1);
        assert!(store.is_empty());
    }

    #[test]
    fn duplicate_deposit_is_idempotent() {
        let mut store = store();
        let recipient = key(0xB0);
        let mail = envelope(&recipient, 1);
        store
            .deposit(&mail, &key(0xA1), NOW)
            .expect("first deposit");
        store
            .deposit(&mail, &key(0xA1), NOW)
            .expect("second deposit");
        assert_eq!(store.carried_count(), 1);
    }

    // MARK: - Non-negotiable 2: the ingress rule

    #[test]
    fn handover_is_refused_without_an_authenticated_direct_ingress() {
        let mut store = store();
        let recipient = key(0xB0);
        store
            .deposit(&envelope(&recipient, 1), &key(0xA1), NOW)
            .expect("deposit");

        // A relayed announce proves nothing about who owns the link.
        let mut offers = 0;
        let outcome = store
            .handover_envelopes(&recipient, NOW, AnnounceIngress::relayed(), |_| {
                offers += 1;
                true
            })
            .expect("handover");
        assert_eq!(outcome, Handover::RefusedUnauthenticatedIngress);
        assert_eq!(offers, 0, "nothing may even be offered on a weak ingress");
        assert_eq!(store.carried_count(), 1);

        // A direct-looking announce whose link has not proved possession of the
        // Noise key is exactly the replayed-announce attack. Also refused.
        let outcome = store
            .handover_envelopes(
                &recipient,
                NOW,
                AnnounceIngress::direct_but_link_not_authenticated(),
                |_| true,
            )
            .expect("handover");
        assert!(outcome.is_refused());
        assert_eq!(store.carried_count(), 1);

        // A session that exists but is not bound to *this* link: the caller states
        // the link fact separately, and only both together release.
        let outcome = store
            .handover_envelopes(&recipient, NOW, AnnounceIngress::new(false, true), |_| true)
            .expect("handover");
        assert!(outcome.is_refused());
        assert_eq!(store.carried_count(), 1);
    }

    #[test]
    fn handover_is_permitted_with_an_authenticated_direct_ingress() {
        let mut store = store();
        let recipient = key(0xB0);
        store
            .deposit(&envelope(&recipient, 1), &key(0xA1), NOW)
            .expect("deposit");

        assert!(direct().releases_mail());
        let outcome = store
            .handover_envelopes(&recipient, NOW, direct(), |_| true)
            .expect("handover");
        assert_eq!(
            outcome,
            Handover::Released {
                offered: 1,
                accepted: 1
            }
        );
        assert!(store.is_empty());
    }

    #[test]
    fn only_both_ingress_facts_together_release_mail() {
        assert!(!AnnounceIngress::relayed().releases_mail());
        assert!(!AnnounceIngress::direct_but_link_not_authenticated().releases_mail());
        assert!(!AnnounceIngress::new(false, true).releases_mail());
        assert!(AnnounceIngress::verified_direct_on_authenticated_link().releases_mail());
    }

    // MARK: - Non-negotiable 1: quotas

    #[test]
    fn per_depositor_quota_is_exactly_two() {
        let mut store = store();
        let depositor = key(0xA1);
        for nonce in 0..MAX_PER_DEPOSITOR as u32 {
            store
                .deposit(&envelope(&key(0xB0), nonce), &depositor, NOW)
                .expect("within quota");
        }
        assert_eq!(MAX_PER_DEPOSITOR, 2);
        assert_eq!(
            store.deposit(&envelope(&key(0xB0), 99), &depositor, NOW),
            Err(StoreError::DepositRefused("per-depositor quota reached"))
        );
        // A different depositor still has room.
        store
            .deposit(&envelope(&key(0xB0), 100), &key(0xA2), NOW)
            .expect("other depositor");
    }

    /// Fill the pool with `count` envelopes, spreading them over as many
    /// depositors as the per-depositor quota requires. Returns the next free nonce.
    fn fill(store: &mut CourierStore<MemoryStorage>, count: usize) -> u32 {
        let mut nonce = 1_000u32;
        for slot in 0..count {
            let depositor = key(0x10 + (slot / MAX_PER_DEPOSITOR) as u8);
            store
                .deposit(&envelope(&key(0xB0), nonce), &depositor, NOW)
                .expect("fill");
            nonce += 1;
        }
        nonce
    }

    #[test]
    fn a_full_pool_refuses_rather_than_evicting() {
        let mut store = store();
        let first_recipient = key(0xD0);
        store
            .deposit(&envelope(&first_recipient, 1), &key(0xA1), NOW)
            .expect("first");
        let nonce = fill(&mut store, MAX_ENVELOPES - 1);
        assert_eq!(MAX_ENVELOPES, 20);
        assert_eq!(store.carried_count(), MAX_ENVELOPES);

        assert_eq!(
            store.deposit(&envelope(&key(0xB0), nonce), &key(0xEE), NOW),
            Err(StoreError::DepositRefused("courier pool full"))
        );
        assert_eq!(store.carried_count(), MAX_ENVELOPES);
        // The oldest mail is still carried: a stranger cannot push it out.
        assert_eq!(
            store
                .take_envelopes(&first_recipient, NOW, direct())
                .expect("take")
                .len(),
            1
        );
    }

    // MARK: - Non-negotiable 3: copy budgets

    #[test]
    fn a_budget_above_the_hard_cap_of_eight_is_clamped() {
        let mut store = store();
        let recipient = key(0xB0);

        // The wire type clamps, but its fields are public — a budget of 200 can
        // still reach the store, and the excess must not be carried.
        let mut greedy = envelope_at(&recipient, 1, HOUR, 1, NOW);
        greedy.copies = 200;
        store.deposit(&greedy, &key(0xA1), NOW).expect("deposit");

        assert_eq!(MAX_COPIES, 8);
        let taken = store
            .take_envelopes(&recipient, NOW, direct())
            .expect("take");
        assert_eq!(
            taken.iter().map(|e| e.copies).collect::<Vec<_>>(),
            vec![MAX_COPIES]
        );
    }

    #[test]
    fn a_duplicate_deposit_keeps_the_larger_budget() {
        let mut store = store();
        let recipient = key(0xB0);
        let carry_only = envelope_at(&recipient, 1, HOUR, 1, NOW);
        store
            .deposit(&carry_only, &key(0xA1), NOW)
            .expect("carry-only arrived first");
        store
            .deposit(&carry_only.with_copies(4), &key(0xA2), NOW)
            .expect("higher-budget original followed");
        assert_eq!(store.carried_count(), 1);

        let taken = store
            .take_envelopes(&recipient, NOW, direct())
            .expect("take");
        assert_eq!(taken.iter().map(|e| e.copies).collect::<Vec<_>>(), vec![4]);
    }

    // MARK: - Validity and expiry

    #[test]
    fn deposits_that_fail_validity_are_refused() {
        let mut store = store();
        let recipient = key(0xB0);

        // Already expired.
        let stale = envelope_at(&recipient, 1, HOUR, 1, NOW - 2 * HOUR);
        assert_eq!(
            store.deposit(&stale, &key(0xA1), NOW),
            Err(StoreError::DepositRefused("already expired"))
        );

        // Oversized ciphertext.
        let mut oversized = envelope(&recipient, 2);
        oversized.ciphertext = vec![0u8; MAX_CIPHERTEXT_BYTES + 1];
        assert_eq!(
            store.deposit(&oversized, &key(0xA1), NOW),
            Err(StoreError::DepositRefused("ciphertext over 16 KiB"))
        );

        // Empty ciphertext.
        let mut empty = envelope(&recipient, 3);
        empty.ciphertext.clear();
        assert_eq!(
            store.deposit(&empty, &key(0xA1), NOW),
            Err(StoreError::DepositRefused("empty ciphertext"))
        );

        // No depositor key: quotas are keyed on it.
        assert_eq!(
            store.deposit(&envelope(&recipient, 4), &[], NOW),
            Err(StoreError::DepositRefused("no depositor key"))
        );

        assert!(store.is_empty());
    }

    #[test]
    fn an_expiry_beyond_the_lifetime_plus_slack_is_refused_as_skew_or_abuse() {
        let mut store = store();
        let recipient = key(0xB0);

        // 24 h plus the hour of clock-skew slack: still accepted.
        let at_limit = envelope_at(
            &recipient,
            1,
            MAX_LIFETIME_SECONDS + MAX_EXPIRY_SLACK_SECS,
            1,
            NOW,
        );
        store
            .deposit(&at_limit, &key(0xA1), NOW)
            .expect("exactly at the limit is fine");

        // One second past it is not.
        let past_limit = envelope_at(
            &recipient,
            2,
            MAX_LIFETIME_SECONDS + MAX_EXPIRY_SLACK_SECS + 1,
            1,
            NOW,
        );
        assert_eq!(
            store.deposit(&past_limit, &key(0xA1), NOW),
            Err(StoreError::DepositRefused("expiry beyond maximum lifetime"))
        );

        // A week of pinned storage certainly is not.
        let pinned = envelope_at(&recipient, 3, 7 * 24 * HOUR, 1, NOW);
        assert_eq!(
            store.deposit(&pinned, &key(0xA1), NOW),
            Err(StoreError::DepositRefused("expiry beyond maximum lifetime"))
        );
        assert_eq!(store.carried_count(), 1);
    }

    #[test]
    fn expired_envelopes_are_pruned_and_never_handed_over() {
        let mut store = store();
        let recipient = key(0xB0);
        store
            .deposit(&envelope_at(&recipient, 1, HOUR, 1, NOW), &key(0xA1), NOW)
            .expect("short-lived");
        store
            .deposit(
                &envelope_at(&recipient, 2, 12 * HOUR, 1, NOW),
                &key(0xA1),
                NOW,
            )
            .expect("long-lived");

        // One second before expiry both are still carried.
        assert_eq!(store.prune_expired(NOW + HOUR - 1).expect("prune"), 0);
        // At the expiry instant the short-lived one goes.
        assert_eq!(store.prune_expired(NOW + HOUR).expect("prune"), 1);
        assert_eq!(store.carried_count(), 1);

        assert!(
            store
                .take_envelopes(&recipient, NOW + 13 * HOUR, direct())
                .expect("take")
                .is_empty(),
            "expired mail is never handed over"
        );
        assert!(store.is_empty());
    }

    // MARK: - Persistence and wipe

    #[test]
    fn the_pool_round_trips_through_storage() {
        let mut store = store();
        store
            .deposit(
                &envelope_at(&key(0xE0), 1, 12 * HOUR, 4, NOW),
                &key(0xA1),
                NOW,
            )
            .expect("deposit");
        store
            .deposit(
                &envelope_at(&key(0xE1), 2, 12 * HOUR, 1, NOW),
                &key(0xA2),
                NOW,
            )
            .expect("deposit");

        let before = store.envelopes.clone();
        let mut reloaded = CourierStore::new(store.into_storage());
        assert_eq!(reloaded.restore(NOW).expect("restore"), 2);
        assert_eq!(reloaded.envelopes, before);
    }

    fn stored(index: u32, depositor: Vec<u8>) -> StoredEnvelope {
        StoredEnvelope {
            recipient_tag: [index as u8; TAG_LEN],
            expiry_ms: (NOW + 12 * HOUR) * 1000,
            ciphertext: index.to_be_bytes().to_vec(),
            depositor_noise_key: depositor,
            stored_at_unix_secs: NOW + u64::from(index),
            copies: 1,
            prekey_id: None,
        }
    }

    #[test]
    fn restore_refuses_a_count_past_the_total_cap() {
        // A blob edited on disk must not smuggle the pool past its cap.
        let envelopes: Vec<_> = (0..(MAX_ENVELOPES + 1) as u32)
            .map(|index| stored(index, vec![index as u8; 32]))
            .collect();
        // Encoded directly: `deposit` would never have let this pool exist.
        let storage = MemoryStorage::new();
        storage
            .save(StorageKey::CourierPool, &encode_pool(&envelopes))
            .expect("save");

        let mut store = CourierStore::new(storage);
        assert_eq!(
            store.restore(NOW),
            Err(StoreError::Malformed("courier pool")),
            "a count past the total cap is refused outright"
        );
    }

    #[test]
    fn restore_re_applies_the_per_depositor_quota() {
        let envelopes: Vec<_> = (0..8u32)
            .map(|index| stored(index, vec![0xA1; 32]))
            .collect();
        let storage = MemoryStorage::new();
        storage
            .save(StorageKey::CourierPool, &encode_pool(&envelopes))
            .expect("save");

        let mut store = CourierStore::new(storage);
        assert_eq!(
            store.restore(NOW).expect("restore"),
            MAX_PER_DEPOSITOR,
            "one depositor may not hold more than its quota, however the blob looked"
        );
    }

    #[test]
    fn restore_drops_expired_mail_and_a_malformed_blob_starts_empty() {
        let mut store = store();
        store
            .deposit(&envelope_at(&key(0xB0), 1, HOUR, 1, NOW), &key(0xA1), NOW)
            .expect("deposit");

        let storage = store.into_storage();
        let mut reloaded = CourierStore::new(storage);
        assert_eq!(reloaded.restore(NOW + 2 * HOUR).expect("restore"), 0);
        assert!(reloaded.is_empty());

        // A corrupt blob is reported, not fatal: the bag is simply empty.
        reloaded
            .storage()
            .save(StorageKey::CourierPool, b"not a courier pool")
            .expect("save");
        assert_eq!(
            reloaded.restore(NOW),
            Err(StoreError::Malformed("courier pool"))
        );
        assert!(reloaded.is_empty());
    }

    #[test]
    fn wipe_drops_everything_in_memory_and_on_disk() {
        let mut store = store();
        let recipient = key(0xB0);
        store
            .deposit(&envelope(&recipient, 1), &key(0xA1), NOW)
            .expect("deposit");
        assert!(!store.storage().is_empty());

        store.wipe().expect("wipe");
        assert!(store.is_empty());
        assert!(
            store.storage().is_empty(),
            "panic wipe leaves nothing behind"
        );
        assert!(
            store
                .take_envelopes(&recipient, NOW, direct())
                .expect("take")
                .is_empty()
        );
    }

    #[test]
    fn emptying_the_pool_removes_the_durable_blob() {
        let mut store = store();
        let recipient = key(0xB0);
        store
            .deposit(&envelope(&recipient, 1), &key(0xA1), NOW)
            .expect("deposit");
        assert!(!store.storage().is_empty());

        store
            .take_envelopes(&recipient, NOW, direct())
            .expect("take");
        assert!(
            store.storage().is_empty(),
            "the last envelope leaving must not leave a stale blob behind"
        );
    }

    // MARK: - Encoding details

    #[test]
    fn a_truncated_blob_never_panics() {
        let stored = StoredEnvelope {
            recipient_tag: [0x11; TAG_LEN],
            expiry_ms: 1_700_000_000_000,
            ciphertext: vec![7; 40],
            depositor_noise_key: key(0xA1),
            stored_at_unix_secs: NOW,
            copies: 4,
            prekey_id: Some(0xDEAD_BEEF),
        };
        let encoded = encode_pool(core::slice::from_ref(&stored));
        assert_eq!(decode_pool(&encoded).expect("decode"), vec![stored]);

        for cut in 0..encoded.len() {
            assert!(
                decode_pool(&encoded[..cut]).is_err(),
                "prefix of {cut} bytes must not decode"
            );
        }
        // Trailing garbage is malformed too.
        let mut extended = encoded.clone();
        extended.push(0);
        assert!(decode_pool(&extended).is_err());
    }

    #[test]
    fn an_empty_pool_round_trips() {
        assert_eq!(decode_pool(&encode_pool(&[])).expect("decode"), vec![]);
    }
}

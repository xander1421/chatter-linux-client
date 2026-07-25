//! Golomb-Coded Set filters for gossip sync.
//!
//! Ported from `Sync/GCSFilter.swift`, and the bit layout is the whole point: this
//! structure is exchanged with real bitchat peers inside REQUEST_SYNC, so a filter
//! that is one bit off does not fail loudly — it decodes into plausible garbage and
//! sync silently stops converging.
//!
//! # The format, restated from their header comment
//!
//! - A packet ID is 16 bytes ([`crate::packet::BitchatPacket::packet_id`]). For
//!   GCS mapping we take `h64` = the first 8 bytes of `SHA-256` over that 16-byte
//!   ID, big-endian, with the top bit cleared.
//! - Map into `[1, M)` as `h64 % M`, remapping `0 -> 1` so no delta is ever zero.
//! - Sort ascending, then encode deltas (the first is the value itself) as
//!   integers `x >= 1`.
//! - Golomb-Rice with parameter `P`: `q = (x - 1) >> P` written as unary — `q`
//!   one-bits followed by a single zero — then the `P`-bit remainder
//!   `r = (x - 1) & ((1 << P) - 1)`.
//! - The bitstream is MSB-first within each byte, and the final byte is
//!   left-aligned: leftover bits are padded with zeros on the right.
//!
//! # Why decoding can invent an element
//!
//! Zero padding in the last byte is itself a decodable prefix — `0` unary means
//! `q = 0`, and `P` zero bits mean `r = 0`, so `x = 1`. Whenever `P + 1` padding
//! bits survive (only possible for `P <= 6`), the decoder yields one extra value at
//! `acc + 1`. That is inherent to the format, both sides do it, and it costs one
//! spurious false-positive bucket. Do not "fix" it: a length-prefixed or
//! terminator-based variant would not interoperate.

use sha2::{Digest, Sha256};

/// Highest Golomb-Rice parameter accepted from the wire.
///
/// `P` maps to a false-positive rate of about `1 / 2^P`; past 32 the remainder is
/// wider than any practical filter and the shifts in decode would silently
/// overflow into garbage.
pub const MAX_P: u32 = 32;

/// An encoded filter, plus the parameters a peer needs to interpret it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GcsParams {
    /// Golomb-Rice parameter.
    pub p: u32,
    /// Modulus every element was mapped into.
    pub m: u32,
    /// The Golomb-Rice bitstream.
    pub data: Vec<u8>,
    /// How many of the input IDs, *in input order*, this filter actually encodes.
    ///
    /// Below `ids.len()` when the encoding overflowed the byte budget and the tail
    /// was trimmed. Callers deriving a since-cursor need this: trimming always
    /// drops from the tail, so the first `included` inputs are exactly the covered
    /// set — a contiguous prefix, never a hash-ordered arbitrary subset.
    pub included: usize,
}

/// Derives `P` from a target false-positive rate, `P = ceil(log2(1 / fpr))`.
///
/// The rate is clamped to `[0.000001, 0.25]` first, which is what actually bounds
/// the result: it puts `P` in `[2, 20]`, comfortably under [`MAX_P`]. The clamp to
/// `MAX_P` afterwards is belt-and-braces, and matches the ceiling the decoder
/// enforces on inbound parameters.
///
/// A non-finite `fpr` collapses to the 0.25 end of the clamp, as it does in Swift
/// (`min(0.25, nan)` returns 0.25 there, and `f64::min` does the same here).
// Not `f64::clamp`: clamp propagates NaN, and this must not. Swift's
// `max(0.000001, min(0.25, fpr))` folds NaN to 0.25 because its comparisons all
// fail, and `f64::min`/`f64::max` reproduce that by returning the non-NaN operand.
// Swapping in `clamp` here would turn a NaN rate into P = 1 instead of P = 2.
#[allow(clippy::manual_clamp)]
pub fn derive_p(target_fpr: f64) -> u32 {
    let f = target_fpr.min(0.25).max(0.000001);
    // `as u32` saturates in Rust, so no value of `f` can produce a wild result.
    let p = (1.0 / f).log2().ceil() as u32;
    p.clamp(1, MAX_P)
}

/// Roughly how many elements fit in `size_bytes`, at about `P + 2` bits each.
pub fn estimate_max_elements(size_bytes: usize, p: u32) -> usize {
    let bits = size_bytes.saturating_mul(8).max(8);
    let per = (p as usize).saturating_add(2).max(3);
    (bits / per).max(1)
}

/// Builds a filter covering as many of `ids` as `max_bytes` allows.
///
/// `ids` must be ordered newest-first. When the encoding overflows the budget the
/// tail is trimmed — dropping the oldest — and [`GcsParams::included`] reports how
/// many survived.
pub fn build_filter(ids: &[[u8; 16]], max_bytes: usize, target_fpr: f64) -> GcsParams {
    let p = derive_p(target_fpr);
    if ids.is_empty() {
        return GcsParams {
            p,
            m: 1,
            data: Vec::new(),
            included: 0,
        };
    }

    let cap = estimate_max_elements(max_bytes, p);
    // The modulus is fixed to the *initial* candidate count, so `m` stays stable
    // while the tail is trimmed below. Recomputing it per attempt would change
    // every bucket on every retry and make `included` meaningless.
    let range = hash_range(ids.len().min(cap), p).max(1);
    let modulo = u64::from(range);

    let encode_first = |count: usize| -> Vec<u8> {
        let mut mapped: Vec<u64> = ids
            .iter()
            .take(count)
            .map(|id| map_hash(h64(id), modulo))
            .collect();
        mapped.sort_unstable();
        let normalized = normalize_mapped_values(&mapped, modulo);
        if normalized.is_empty() {
            Vec::new()
        } else {
            encode(&normalized, p)
        }
    };

    let mut count = ids.len().min(cap);
    let mut encoded = encode_first(count);
    while encoded.len() > max_bytes && count > 1 {
        // Shrink by 10% per attempt, as Swift does. Not a binary search: the
        // encoded length is monotone in `count`, but overshooting downward costs
        // coverage, and the loop converges in a handful of passes anyway.
        count = (count.saturating_mul(9) / 10).max(1);
        encoded = encode_first(count);
    }

    // A single element that still overflows cannot be represented at all.
    if encoded.len() > max_bytes {
        return GcsParams {
            p,
            m: range,
            data: Vec::new(),
            included: 0,
        };
    }

    let included = if encoded.is_empty() { 0 } else { count };
    GcsParams {
        p,
        m: range,
        data: encoded,
        included,
    }
}

/// Decodes a filter back to its ascending bucket values.
///
/// Returns an empty set for out-of-range parameters rather than decoding garbage:
/// callers read that as "the peer has nothing" and fall back to sending data,
/// which is the safe direction to be wrong in.
///
/// Truncated input simply stops early — every value it does return was in the
/// original set.
pub fn decode_to_sorted(p: u32, m: u32, data: &[u8]) -> Vec<u64> {
    if !(1..=MAX_P).contains(&p) || m <= 1 {
        return Vec::new();
    }

    let modulus = u64::from(m);
    let mut values: Vec<u64> = Vec::new();
    let mut reader = BitReader::new(data);
    let mut acc: u64 = 0;

    while let Some(q) = reader.read_unary() {
        let Some(r) = reader.read_bits(p) else { break };
        // Wrapping throughout: a hostile stream of one-bits can drive `q` high
        // enough to overflow, and Swift's `&+=` on the accumulator says the
        // encoder's authors expected exactly that. Values past the modulus stop
        // the loop on the next line regardless.
        let x = (q << p).wrapping_add(r).wrapping_add(1);
        acc = acc.wrapping_add(x);
        if acc >= modulus {
            break;
        }
        values.push(acc);
    }

    values
}

/// Binary search for `candidate` in a sorted bucket list.
pub fn contains(sorted: &[u64], candidate: u64) -> bool {
    sorted.binary_search(&candidate).is_ok()
}

/// The bucket a packet ID falls into for a given modulus.
///
/// Zero is never a bucket — `h64 % m == 0` remaps to 1 — because a zero would
/// encode as a zero-length delta and desynchronize the stream.
pub fn bucket(id: &[u8; 16], modulus: u32) -> u64 {
    let modulo = u64::from(modulus.max(1));
    if modulo <= 1 {
        return 0;
    }
    map_hash(h64(id), modulo)
}

/// First 8 bytes of `SHA-256(id)` read big-endian, with the sign bit cleared.
///
/// The mask is not cosmetic: Swift's `%` on a signed `Int` would differ on
/// negative values, and clearing bit 63 is how they sidestepped it. Reproduce it
/// or every bucket above 2^63 lands somewhere else.
fn h64(id16: &[u8; 16]) -> u64 {
    let digest = Sha256::digest(id16);
    let mut x: u64 = 0;
    for b in digest.iter().take(8) {
        x = (x << 8) | u64::from(*b);
    }
    x & 0x7fff_ffff_ffff_ffff
}

/// `count * 2^p`, saturating at `u32::MAX`.
fn hash_range(count: usize, p: u32) -> u32 {
    if count == 0 {
        return 1;
    }
    if p >= 64 {
        return u32::MAX;
    }
    let multiplier = 1u64 << p;
    let Some(product) = (count as u64).checked_mul(multiplier) else {
        return u32::MAX;
    };
    if product == 0 {
        return 1;
    }
    u32::try_from(product).unwrap_or(u32::MAX)
}

fn map_hash(hash: u64, modulo: u64) -> u64 {
    if modulo <= 1 {
        return 0;
    }
    let value = hash % modulo;
    if value == 0 { 1 } else { value }
}

/// Clamps into range and drops non-increasing values.
///
/// Input must already be sorted ascending. Two IDs that map to the same bucket
/// collapse to one element here — which is why `included` counts *inputs*, not
/// encoded values, and why the decoded set can be smaller than the input set.
fn normalize_mapped_values(values: &[u64], modulo: u64) -> Vec<u64> {
    if modulo <= 1 || values.is_empty() {
        return Vec::new();
    }
    let mut result = Vec::with_capacity(values.len());
    let mut last: u64 = 0;
    for value in values {
        let normalized = (*value).min(modulo - 1);
        if normalized > last {
            result.push(normalized);
            last = normalized;
        }
    }
    result
}

/// Golomb-Rice encodes a strictly increasing, non-zero value list.
///
/// Callers must pass the output of [`normalize_mapped_values`]. That guarantee is
/// what bounds the unary run: every delta is at least 1, so `q` sums to at most
/// `modulo >> p`, i.e. the element count.
fn encode(sorted: &[u64], p: u32) -> Vec<u8> {
    let mut writer = BitWriter::new();
    let mut prev: u64 = 0;
    let mask: u64 = if p >= 64 { u64::MAX } else { (1u64 << p) - 1 };

    for v in sorted {
        let delta = v.wrapping_sub(prev);
        prev = *v;
        let x = delta.wrapping_sub(1);
        let q = x >> p;
        let r = x & mask;
        // Unary: `q` ones, then the terminating zero. Reversing those two — ones
        // for the terminator, or a zero-terminated run of zeros — still decodes
        // *something*, which is why this has to be read off the Swift rather than
        // recalled from the textbook.
        writer.write_ones(q);
        writer.write_bit(0);
        writer.write_bits(r, p);
    }

    writer.into_data()
}

/// MSB-first bit writer. The final byte is left-aligned and zero-padded.
struct BitWriter {
    buf: Vec<u8>,
    cur: u8,
    nbits: u32,
}

impl BitWriter {
    fn new() -> Self {
        Self {
            buf: Vec::new(),
            cur: 0,
            nbits: 0,
        }
    }

    fn write_bit(&mut self, bit: u8) {
        // `cur` holds at most 7 bits before this, so the shift never drops one.
        self.cur = (self.cur << 1) | (bit & 1);
        self.nbits += 1;
        if self.nbits == 8 {
            self.buf.push(self.cur);
            self.cur = 0;
            self.nbits = 0;
        }
    }

    fn write_ones(&mut self, count: u64) {
        for _ in 0..count {
            self.write_bit(1);
        }
    }

    fn write_bits(&mut self, value: u64, count: u32) {
        for i in (0..count).rev() {
            // `count` is `p <= MAX_P`, so the shift stays in range.
            let bit = if i < 64 { (value >> i) & 1 } else { 0 };
            self.write_bit(bit as u8);
        }
    }

    fn into_data(mut self) -> Vec<u8> {
        if self.nbits > 0 && self.nbits < 8 {
            self.buf.push(self.cur << (8 - self.nbits));
        }
        self.buf
    }
}

/// MSB-first bit reader. Returns `None` the moment the input runs out, which is
/// how a truncated filter degrades to a prefix of itself instead of an error.
struct BitReader<'a> {
    data: &'a [u8],
    idx: usize,
    cur: u8,
    left: u32,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        let (cur, left) = match data.first() {
            Some(b) => (*b, 8),
            None => (0, 0),
        };
        Self {
            data,
            idx: 0,
            cur,
            left,
        }
    }

    fn read_bit(&mut self) -> Option<u8> {
        if self.idx >= self.data.len() {
            return None;
        }
        let bit = (self.cur >> 7) & 1;
        self.cur <<= 1;
        self.left = self.left.saturating_sub(1);
        if self.left == 0 {
            self.idx += 1;
            if let Some(b) = self.data.get(self.idx) {
                self.cur = *b;
                self.left = 8;
            }
        }
        Some(bit)
    }

    fn read_unary(&mut self) -> Option<u64> {
        let mut q: u64 = 0;
        // Saturating so a hostile all-ones stream cannot overflow the counter.
        while self.read_bit()? == 1 {
            q = q.saturating_add(1);
        }
        Some(q)
    }

    fn read_bits(&mut self, count: u32) -> Option<u64> {
        let mut v: u64 = 0;
        for _ in 0..count {
            let bit = self.read_bit()?;
            v = (v << 1) | u64::from(bit);
        }
        Some(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Expected bytes in this module were produced by a line-by-line transcription
    /// of `GCSFilter.swift` into a scratch script, not by running their Swift —
    /// we have no Swift toolchain here. They pin the bit layout against
    /// regression; they are not independent proof of interop. Vector B is small
    /// enough to verify by hand, and is: one element, bucket 5, `P = 7`, so the
    /// stream is `0` (unary q=0) then `0000100` (r=4) = `0b0000_0100` = `0x04`.
    fn ids_0_through(n: u8) -> Vec<[u8; 16]> {
        (0..n).map(|i| [i; 16]).collect()
    }

    #[test]
    fn derive_p_is_ceil_log2_of_the_inverse_rate() {
        assert_eq!(derive_p(0.25), 2);
        assert_eq!(derive_p(0.1), 4);
        assert_eq!(derive_p(0.01), 7);
        assert_eq!(derive_p(0.001), 10);
    }

    #[test]
    fn derive_p_clamps_the_rate_at_both_ends() {
        // Above 0.25 clamps down to 0.25.
        assert_eq!(derive_p(0.5), 2);
        assert_eq!(derive_p(1.0), 2);
        // Below 1e-6 clamps up, capping P at 20 — well under MAX_P.
        assert_eq!(derive_p(1e-9), 20);
        assert_eq!(derive_p(0.0), 20);
        assert_eq!(derive_p(-1.0), 20);
        // Non-finite input must not panic or produce a wild parameter.
        assert_eq!(derive_p(f64::NAN), 2);
        assert_eq!(derive_p(f64::INFINITY), 2);
        assert_eq!(derive_p(f64::NEG_INFINITY), 20);
        for fpr in [0.5, 0.25, 0.1, 0.01, 1e-9, 0.0, f64::NAN] {
            assert!((1..=MAX_P).contains(&derive_p(fpr)));
        }
    }

    #[test]
    fn estimate_max_elements_uses_p_plus_two_bits() {
        assert_eq!(estimate_max_elements(1024, 7), 8192 / 9);
        assert_eq!(estimate_max_elements(64, 7), 512 / 9);
        // Floors: at least 8 bits of budget, at least 3 bits per element, at
        // least one element.
        assert_eq!(estimate_max_elements(0, 7), 1);
        assert_eq!(estimate_max_elements(1, 0), 2);
        assert_eq!(estimate_max_elements(usize::MAX, 7), usize::MAX / 9);
    }

    #[test]
    fn single_element_filter_is_byte_exact() {
        // Verified by hand: P=7, M=128, the one bucket is 5, so x=5, q=0, r=4.
        let ids = [[0xABu8; 16]];
        let params = build_filter(&ids, 64, 0.01);
        assert_eq!(params.p, 7);
        assert_eq!(params.m, 128);
        assert_eq!(params.data, vec![0x04]);
        assert_eq!(params.included, 1);
        assert_eq!(decode_to_sorted(params.p, params.m, &params.data), vec![5]);
        assert_eq!(bucket(&ids[0], params.m), 5);
    }

    #[test]
    fn eight_element_filter_is_byte_exact() {
        let ids = ids_0_through(8);
        let params = build_filter(&ids, 1024, 0.01);
        assert_eq!(params.p, 7);
        assert_eq!(params.m, 1024); // 8 elements << 7
        assert_eq!(params.included, 8);
        assert_eq!(
            params.data,
            vec![0x4c, 0x4c, 0xc8, 0xc5, 0x98, 0x08, 0xd0, 0xe4, 0xe0]
        );
        assert_eq!(
            decode_to_sorted(params.p, params.m, &params.data),
            vec![77, 154, 446, 469, 566, 602, 670, 838]
        );
    }

    #[test]
    fn every_included_id_is_a_member() {
        let ids = ids_0_through(64);
        let params = build_filter(&ids, 1024, 0.01);
        let decoded = decode_to_sorted(params.p, params.m, &params.data);
        assert_eq!(params.included, ids.len());
        for id in ids.iter().take(params.included) {
            assert!(
                contains(&decoded, bucket(id, params.m)),
                "an encoded ID was not a member of its own filter"
            );
        }
    }

    #[test]
    fn duplicate_ids_collapse_to_one_element() {
        // Their `buildFilterWithDuplicateIdsProducesStableEncoding`.
        let ids = vec![[0xABu8; 16]; 64];
        let params = build_filter(&ids, 128, 0.01);
        assert_eq!(params.m, 8192); // modulus still uses the *input* count
        assert_eq!(params.included, 64); // ...and `included` counts inputs
        assert_eq!(params.data, vec![0xff, 0xff, 0xff, 0xff, 0xe0, 0x80]);
        let decoded = decode_to_sorted(params.p, params.m, &params.data);
        assert_eq!(decoded, vec![4485]);
    }

    #[test]
    fn byte_budget_is_respected_and_the_tail_is_trimmed() {
        // Their `buildFilterTrimsTailWhenBudgetExceeded`.
        let ids: Vec<[u8; 16]> = (0..200u32)
            .map(|i| {
                let mut id = [0u8; 16];
                for (j, byte) in id.iter_mut().enumerate() {
                    *byte = ((i.wrapping_mul(31)).wrapping_add(j as u32) & 0xFF) as u8;
                }
                id
            })
            .collect();
        let params = build_filter(&ids, 32, 0.01);
        assert!(params.included > 0);
        assert!(params.included < ids.len());
        assert!(params.data.len() <= 32);
        assert_eq!(params.included, 28);
        assert_eq!(params.data.len(), 31);
    }

    #[test]
    fn budget_is_respected_across_a_range_of_sizes() {
        let ids = ids_0_through(200);
        for max_bytes in [1usize, 2, 8, 16, 33, 64, 100, 255, 1024] {
            let params = build_filter(&ids, max_bytes, 0.01);
            assert!(
                params.data.len() <= max_bytes,
                "budget {max_bytes} overrun: {} bytes",
                params.data.len()
            );
            assert!(params.included <= ids.len());
            if params.data.is_empty() {
                assert_eq!(params.included, 0);
            }
        }
    }

    #[test]
    fn trimmed_coverage_is_a_contiguous_newest_first_prefix() {
        let ids = ids_0_through(200);
        let params = build_filter(&ids, 24, 0.01);
        assert!(params.included < ids.len());
        let decoded = decode_to_sorted(params.p, params.m, &params.data);
        // Everything in the covered prefix must be a member...
        for id in ids.iter().take(params.included) {
            assert!(contains(&decoded, bucket(id, params.m)));
        }
        // ...and the same filter must be reproducible from that prefix alone,
        // which is what makes a since-cursor exact.
        let reencoded = build_filter(
            ids.get(..params.included).unwrap_or_default(),
            24,
            0.01,
        );
        assert_eq!(reencoded.data, params.data);
    }

    #[test]
    fn empty_input_produces_an_empty_filter() {
        let params = build_filter(&[], 128, 0.01);
        assert_eq!(params.p, 7);
        assert_eq!(params.m, 1);
        assert!(params.data.is_empty());
        assert_eq!(params.included, 0);
        // m <= 1 is rejected by the decoder, so this reads as "peer has nothing".
        assert!(decode_to_sorted(params.p, params.m, &params.data).is_empty());
    }

    #[test]
    fn decode_rejects_out_of_range_parameters() {
        // Their `decodeRejectsOutOfRangeParameters`.
        let junk = vec![0xFFu8; 64];
        assert!(decode_to_sorted(0, 1000, &junk).is_empty());
        assert!(decode_to_sorted(MAX_P + 1, 1000, &junk).is_empty());
        assert!(decode_to_sorted(255, u32::MAX, &junk).is_empty());
        assert!(decode_to_sorted(8, 0, &junk).is_empty());
        assert!(decode_to_sorted(8, 1, &junk).is_empty());
    }

    #[test]
    fn decode_of_all_ones_terminates_without_overflowing() {
        // A stream of one-bits is an unterminated unary run: the reader hits the
        // end of the data and stops. Nothing here may panic.
        let junk = vec![0xFFu8; 4096];
        let decoded = decode_to_sorted(MAX_P, u32::MAX, &junk);
        assert!(decoded.is_empty());
        assert!(decode_to_sorted(1, 5, &junk).len() < 5);
    }

    #[test]
    fn decode_of_empty_data_is_empty() {
        assert!(decode_to_sorted(7, 1024, &[]).is_empty());
    }

    #[test]
    fn truncated_data_decodes_to_a_subset() {
        // Their `decodeOfTruncatedDataReturnsOnlyCompleteValues`.
        let ids = ids_0_through(32);
        let params = build_filter(&ids, 128, 0.01);
        let full = decode_to_sorted(params.p, params.m, &params.data);
        let half = params.data.len() / 2;
        let truncated = decode_to_sorted(
            params.p,
            params.m,
            params.data.get(..half).unwrap_or_default(),
        );
        assert!(truncated.len() <= full.len());
        assert!(truncated.iter().all(|v| full.contains(v)));
    }

    #[test]
    fn decoded_values_are_ascending_so_binary_search_is_valid() {
        let ids = ids_0_through(50);
        let params = build_filter(&ids, 1024, 0.01);
        let decoded = decode_to_sorted(params.p, params.m, &params.data);
        assert!(decoded.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn contains_finds_every_member_and_no_gap() {
        let sorted = [1u64, 5, 9, 100, 4000];
        for v in sorted {
            assert!(contains(&sorted, v));
        }
        for v in [0u64, 2, 6, 99, 101, 3999, 4001, u64::MAX] {
            assert!(!contains(&sorted, v));
        }
        assert!(!contains(&[], 1));
    }

    #[test]
    fn bucket_is_never_zero_and_stays_in_range() {
        // Their `bucketAvoidsZeroCandidate`.
        let id = [0x01u8; 16];
        assert_eq!(bucket(&id, 2), 1);
        for m in [2u32, 3, 7, 128, 1024, u32::MAX] {
            let b = bucket(&id, m);
            assert!(b != 0);
            assert!(b < u64::from(m));
        }
        // A degenerate modulus has no buckets at all.
        assert_eq!(bucket(&id, 0), 0);
        assert_eq!(bucket(&id, 1), 0);
    }

    #[test]
    fn bucket_matches_the_documented_hash() {
        // h64 = first 8 bytes of SHA-256(id), big-endian, top bit cleared.
        let id = [0xABu8; 16];
        let digest = Sha256::digest(id);
        let mut expected: u64 = 0;
        for b in digest.iter().take(8) {
            expected = (expected << 8) | u64::from(*b);
        }
        expected &= 0x7fff_ffff_ffff_ffff;
        assert_eq!(bucket(&id, 4096), expected % 4096);
    }

    /// Deterministic PRNG so the false-positive measurement is reproducible.
    struct SplitMix64(u64);

    impl SplitMix64 {
        fn next_u64(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }

        /// `tag` occupies byte 0, keeping member and non-member streams disjoint.
        fn id(&mut self, tag: u8) -> [u8; 16] {
            let mut out = [0u8; 16];
            out[..8].copy_from_slice(&self.next_u64().to_le_bytes());
            out[8..].copy_from_slice(&self.next_u64().to_le_bytes());
            out[0] = tag;
            out
        }
    }

    #[test]
    fn false_positive_rate_is_near_the_target() {
        const MEMBERS: usize = 500;
        const TRIALS: usize = 40_000;
        const TARGET: f64 = 0.01;

        let mut rng = SplitMix64(0x5EED_1234_ABCD_0001);
        let members: Vec<[u8; 16]> = (0..MEMBERS).map(|_| rng.id(0x00)).collect();

        let params = build_filter(&members, 4096, TARGET);
        assert_eq!(params.included, MEMBERS);
        assert!(params.data.len() <= 4096);
        let decoded = decode_to_sorted(params.p, params.m, &params.data);

        // No member may ever be missing: false negatives are not allowed.
        for id in &members {
            assert!(contains(&decoded, bucket(id, params.m)));
        }

        let mut false_positives = 0usize;
        for _ in 0..TRIALS {
            let candidate = rng.id(0xFF);
            if contains(&decoded, bucket(&candidate, params.m)) {
                false_positives += 1;
            }
        }

        // Expected rate is |distinct buckets| / M ~= 2^-P = 1/128 = 0.0078. The
        // bounds are wide enough to be robust and tight enough to catch a filter
        // that is empty, saturated, or built with the wrong P.
        let rate = false_positives as f64 / TRIALS as f64;
        assert!(
            rate > 0.002 && rate < 0.02,
            "false-positive rate {rate} is not near the {TARGET} target"
        );
    }

    #[test]
    fn round_trip_holds_across_parameters() {
        for fpr in [0.25, 0.1, 0.01, 0.001] {
            for n in [1usize, 2, 3, 17, 64, 129] {
                let mut rng = SplitMix64(0xC0FF_EE00_0000_0000 + n as u64);
                let ids: Vec<[u8; 16]> = (0..n).map(|_| rng.id(0x00)).collect();
                let params = build_filter(&ids, 8192, fpr);
                assert_eq!(params.included, n, "fpr {fpr}, n {n}");
                let decoded = decode_to_sorted(params.p, params.m, &params.data);
                for id in &ids {
                    assert!(
                        contains(&decoded, bucket(id, params.m)),
                        "member missing at fpr {fpr}, n {n}"
                    );
                }
            }
        }
    }

    #[test]
    fn bit_writer_pads_the_final_byte_on_the_right() {
        let mut w = BitWriter::new();
        w.write_bit(1);
        w.write_bit(0);
        w.write_bit(1);
        assert_eq!(w.into_data(), vec![0b1010_0000]);

        let mut w = BitWriter::new();
        for _ in 0..8 {
            w.write_bit(1);
        }
        // Exactly on the boundary: no padding byte is appended.
        assert_eq!(w.into_data(), vec![0xFF]);
    }

    #[test]
    fn bit_writer_writes_remainders_most_significant_bit_first() {
        let mut w = BitWriter::new();
        w.write_bits(0b101, 3);
        w.write_bits(0b11, 2);
        assert_eq!(w.into_data(), vec![0b1011_1000]);
    }

    #[test]
    fn bit_reader_mirrors_the_writer() {
        let mut w = BitWriter::new();
        w.write_ones(3);
        w.write_bit(0);
        w.write_bits(0b1001, 4);
        let data = w.into_data();

        let mut r = BitReader::new(&data);
        assert_eq!(r.read_unary(), Some(3));
        assert_eq!(r.read_bits(4), Some(0b1001));
        assert_eq!(r.read_bit(), None);
    }

    #[test]
    fn bit_reader_on_empty_input_yields_nothing() {
        let mut r = BitReader::new(&[]);
        assert_eq!(r.read_bit(), None);
        assert_eq!(r.read_unary(), None);
        assert_eq!(r.read_bits(4), None);
    }

    #[test]
    fn hash_range_saturates_instead_of_wrapping() {
        assert_eq!(hash_range(0, 7), 1);
        assert_eq!(hash_range(8, 7), 1024);
        assert_eq!(hash_range(usize::MAX, 7), u32::MAX);
        assert_eq!(hash_range(1, 64), u32::MAX);
        assert_eq!(hash_range(1 << 30, 7), u32::MAX);
    }

    #[test]
    fn normalize_drops_duplicates_and_clamps_into_range() {
        assert_eq!(normalize_mapped_values(&[1, 1, 2, 2, 3], 100), vec![1, 2, 3]);
        assert_eq!(normalize_mapped_values(&[5, 200], 100), vec![5, 99]);
        assert_eq!(normalize_mapped_values(&[0, 1], 100), vec![1]);
        assert!(normalize_mapped_values(&[1, 2], 1).is_empty());
        assert!(normalize_mapped_values(&[], 100).is_empty());
    }
}

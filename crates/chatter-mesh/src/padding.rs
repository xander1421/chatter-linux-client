//! PKCS#7-style message padding.
//!
//! Ported byte-for-byte from `BitFoundation/MessagePadding.swift`.
//!
//! The point of the padding is traffic analysis, not integrity: every frame that
//! fits is rounded up to one of four block sizes so an observer counting bytes on
//! the air learns less about what was said. Nothing here is authenticated, and
//! [`unpad`] is a heuristic — see its docs.
//!
//! Two fallbacks in here look like they could be tidied away and cannot be:
//!
//! * [`pad`] returns the input **unchanged** when the gap is 0 or larger than 255,
//!   because the pad length has to fit in the single trailing marker byte.
//! * [`optimal_block_size`] returns the *input length* when nothing fits, which
//!   makes [`pad`] a no-op for oversized frames (they get fragmented anyway).
//!
//! Both are load-bearing for interop: a real bitchat peer emits unpadded frames in
//! exactly those two cases, and a "fixed" implementation that always pads would
//! produce different bytes on the wire.

/// The block sizes a frame is rounded up to, smallest first.
pub const BLOCK_SIZES: [usize; 4] = [256, 512, 1024, 2048];

/// Assumed AEAD tag overhead, added before choosing a block.
///
/// Swift's comment says "~16 bytes for AES-GCM tag". The frame being padded here
/// is not itself encrypted, but the constant is part of the wire behaviour: it
/// shifts the boundary between blocks by 16 bytes, so dropping it would change the
/// padded length of every frame within 16 bytes of a block edge.
const TAG_OVERHEAD: usize = 16;

/// Appends PKCS#7 padding so the result is exactly `target` bytes long.
///
/// Returns the input unchanged when `target` is not strictly larger than the input,
/// or when the gap exceeds 255 — the pad length must fit in one byte. Both
/// fallbacks match `MessagePadding.pad(_:toSize:)` and are relied upon by the
/// encoder for frames above 2048 bytes.
pub fn pad(data: &[u8], target: usize) -> Vec<u8> {
    if data.len() >= target {
        return data.to_vec();
    }

    let padding_needed = target - data.len();
    // Constrained to 255 so the pad length fits a single-byte marker.
    if padding_needed == 0 || padding_needed > 255 {
        return data.to_vec();
    }

    let mut padded = Vec::with_capacity(target);
    padded.extend_from_slice(data);
    // PKCS#7: every pad byte equals the pad length.
    padded.resize(target, padding_needed as u8);
    padded
}

/// Strips PKCS#7 padding, or returns the input unchanged if it is not padded.
///
/// The check is the full PKCS#7 one: the last byte gives the count, and *all*
/// trailing bytes must equal it. Anything that fails the check — a zero marker, a
/// count longer than the buffer, a mismatched tail — is treated as unpadded data.
///
/// This is inherently ambiguous and known to be so: a payload that genuinely ends
/// in `0x01` is indistinguishable from one byte of padding. That is why
/// [`crate::codec::decode`] parses the raw bytes *first* and only falls back to
/// unpadding, rather than the other way round.
pub fn unpad(data: &[u8]) -> Vec<u8> {
    let Some(&last) = data.last() else {
        return data.to_vec();
    };

    let padding_length = usize::from(last);
    // Must be at least one pad byte and must not exceed the buffer.
    if padding_length == 0 || padding_length > data.len() {
        return data.to_vec();
    }

    let start = data.len() - padding_length;
    // `start <= data.len()` by the guard above, so this slice cannot panic.
    let tail = &data[start..];
    if tail.iter().any(|&b| b != last) {
        return data.to_vec();
    }
    data[..start].to_vec()
}

/// The smallest block size that fits `len` plus tag overhead, or `len` itself.
///
/// Returning the raw length for anything that does not fit in 2048 bytes is
/// deliberate: those frames are fragmented downstream, so padding them buys no
/// privacy and only wastes air time.
pub fn optimal_block_size(len: usize) -> usize {
    // Account for encryption overhead (~16 bytes for an AEAD tag).
    let total_size = len.saturating_add(TAG_OVERHEAD);

    for block_size in BLOCK_SIZES {
        if total_size <= block_size {
            return block_size;
        }
    }

    // Very large messages are left alone; they will be fragmented anyway.
    len
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pad_appends_pkcs7_bytes_to_reach_target() {
        let data = vec![0xAA; 250];
        let padded = pad(&data, 256);
        assert_eq!(padded.len(), 256);
        assert_eq!(&padded[..250], &data[..]);
        assert!(padded[250..].iter().all(|&b| b == 6));
    }

    #[test]
    fn pad_of_exactly_one_byte_short_writes_a_single_0x01() {
        let data = vec![0x11; 255];
        let padded = pad(&data, 256);
        assert_eq!(padded.len(), 256);
        assert_eq!(padded[255], 0x01);
    }

    /// TRAP: `pad` has a hard 255 cap and returns the data UNCHANGED above it.
    #[test]
    fn trap_pad_returns_input_unchanged_when_gap_exceeds_255() {
        let data = vec![0x42; 1];
        // 512 - 1 = 511 pad bytes, which cannot be encoded in the marker byte.
        let padded = pad(&data, 512);
        assert_eq!(padded, data, "gap > 255 must leave the frame untouched");

        // 256 - 1 = 255 is exactly at the cap and IS padded.
        let at_cap = pad(&data, 256);
        assert_eq!(at_cap.len(), 256);
        assert!(at_cap[1..].iter().all(|&b| b == 255));
    }

    #[test]
    fn pad_returns_input_unchanged_when_already_at_or_over_target() {
        let data = vec![0x01; 256];
        assert_eq!(pad(&data, 256), data);
        assert_eq!(pad(&data, 100), data);
        assert_eq!(pad(&[], 0), Vec::<u8>::new());
    }

    #[test]
    fn unpad_strips_valid_padding() {
        let data = vec![0x99; 200];
        let padded = pad(&data, 256);
        assert_eq!(unpad(&padded), data);
    }

    /// TRAP: `unpad` verifies EVERY trailing byte, not just the marker.
    #[test]
    fn trap_unpad_returns_input_unchanged_when_tail_does_not_match_marker() {
        let mut padded = pad(&vec![0x99; 250], 256);
        assert_eq!(padded.len(), 256);
        // Corrupt one interior pad byte; PKCS#7 verification must fail.
        padded[252] = 0x05;
        assert_eq!(unpad(&padded), padded, "mismatched tail must not be stripped");

        // Corrupting the marker itself is caught the same way.
        let mut bad_marker = pad(&vec![0x99; 250], 256);
        bad_marker[255] = 0x05;
        assert_eq!(unpad(&bad_marker), bad_marker);
    }

    #[test]
    fn unpad_rejects_zero_marker_oversized_marker_and_empty_input() {
        assert_eq!(unpad(&[]), Vec::<u8>::new());
        assert_eq!(unpad(&[0x00]), vec![0x00]);
        // Marker claims 9 bytes of padding in a 3-byte buffer.
        assert_eq!(unpad(&[0x01, 0x02, 0x09]), vec![0x01, 0x02, 0x09]);
    }

    #[test]
    fn unpad_consumes_the_whole_buffer_when_it_is_all_padding() {
        let all_pad = vec![0x04; 4];
        assert_eq!(unpad(&all_pad), Vec::<u8>::new());
    }

    #[test]
    fn unpad_is_ambiguous_by_design_for_data_ending_in_0x01() {
        // Documented hazard, not a bug: a genuine trailing 0x01 looks like padding.
        // The codec's raw-first decode order is what makes this harmless.
        assert_eq!(unpad(&[0xDE, 0xAD, 0x01]), vec![0xDE, 0xAD]);
    }

    #[test]
    fn optimal_block_size_adds_sixteen_before_choosing() {
        // 240 + 16 == 256 exactly, so it still fits the first block.
        assert_eq!(optimal_block_size(240), 256);
        // 241 + 16 == 257 does not.
        assert_eq!(optimal_block_size(241), 512);
        assert_eq!(optimal_block_size(0), 256);
        assert_eq!(optimal_block_size(496), 512);
        assert_eq!(optimal_block_size(497), 1024);
        assert_eq!(optimal_block_size(1008), 1024);
        assert_eq!(optimal_block_size(1009), 2048);
        assert_eq!(optimal_block_size(2032), 2048);
    }

    /// TRAP: nothing fits above 2048 → the input length is returned, so `pad`
    /// becomes a no-op for large frames.
    #[test]
    fn trap_optimal_block_size_returns_input_length_when_nothing_fits() {
        assert_eq!(optimal_block_size(2033), 2033);
        assert_eq!(optimal_block_size(10_000), 10_000);
        assert_eq!(optimal_block_size(usize::MAX), usize::MAX);

        // And the two fallbacks compose: a large frame is emitted unchanged.
        let big = vec![0x77; 3000];
        assert_eq!(pad(&big, optimal_block_size(big.len())), big);
    }

    #[test]
    fn pad_then_unpad_round_trips_across_every_block_boundary() {
        for len in [0usize, 1, 100, 239, 240, 241, 255, 500, 1000, 2031] {
            let data = vec![0x5A; len];
            let target = optimal_block_size(len);
            let padded = pad(&data, target);
            if padded.len() == data.len() {
                // Gap was > 255; nothing was added, so nothing is stripped either
                // unless the data itself looks like padding (it does not here,
                // 0x5A > any plausible pad length for these sizes).
                continue;
            }
            assert_eq!(padded.len(), target, "len {len} should pad to {target}");
            assert_eq!(unpad(&padded), data, "round trip failed for len {len}");
        }
    }
}

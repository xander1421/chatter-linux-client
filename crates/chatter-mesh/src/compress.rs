//! Payload compression.
//!
//! Ported from `BitFoundation/CompressionUtil.swift` and `Constants.swift`.
//!
//! # This is RAW DEFLATE, not zlib
//!
//! bitchat compresses with Apple's `compression_encode_buffer(..., COMPRESSION_ZLIB)`.
//! Despite the name, that entry point emits a **bare RFC 1951 deflate stream**: no
//! two-byte RFC 1950 header, no adler32 trailer. So we use
//! [`flate2::write::DeflateEncoder`] and [`flate2::Decompress::new(false)`], never
//! the `Zlib*` variants. Getting this wrong makes every compressed packet from a
//! real peer undecodable while our own tests pass happily.
//!
//! # The threshold is 100 bytes
//!
//! `BinaryProtocol.swift`'s header comment claims "automatic compression for
//! payloads > 256 bytes". That comment is **wrong**. The code is
//! `Constants.compressionThresholdBytes = 100` and `shouldCompress` guards on
//! `>= 100`. Source wins over documentation: an implementation built from their
//! docs disagrees with real peers on every payload between 100 and 256 bytes.

use std::io::Write;

use flate2::write::DeflateEncoder;
use flate2::{Compression, Decompress, FlushDecompress};

/// Payloads below this are never compressed. 100, not the 256 their docs claim.
pub const COMPRESSION_THRESHOLD_BYTES: usize = 100;

/// The entropy gate's denominator is capped here; the numerator is not. See
/// [`should_compress`].
const ENTROPY_SAMPLE_CAP: usize = 256;

/// Above this fraction of unique bytes the payload is assumed already compressed.
const MAX_UNIQUE_BYTE_RATIO: f64 = 0.9;

/// Whether compressing this payload is worth attempting.
///
/// Two gates, both ported literally:
///
/// 1. `len >= 100` (see the module docs on why it is not 256).
/// 2. An entropy heuristic: `unique_byte_count / min(len, 256) < 0.9`.
///
/// The second one is asymmetric on purpose — or at least, asymmetric in the
/// shipping code, which is what we have to match. The numerator counts distinct
/// byte *values* across the **whole** payload, while the denominator is capped at
/// 256. So for any payload longer than 256 bytes the ratio is really
/// `unique / 256`, and a 4 KiB payload using 240 distinct values is judged
/// "high entropy" (0.94) even though it is only using 5% of its length in unique
/// values. Do not "fix" this: the flag it controls is on the wire.
pub fn should_compress(data: &[u8]) -> bool {
    // 1. Too small to be worth it.
    if data.len() < COMPRESSION_THRESHOLD_BYTES {
        return false;
    }

    // 2. A high diversity of byte values usually means the payload is already
    //    compressed or encrypted. We only need the count of distinct values, not
    //    full frequency counts. `seen` is a fixed 256-entry table indexed by a
    //    `u8`, so the index is in bounds by construction.
    let mut seen = [false; 256];
    let mut unique_byte_count = 0usize;
    for &byte in data {
        let slot = &mut seen[byte as usize];
        if !*slot {
            *slot = true;
            unique_byte_count += 1;
        }
    }

    let sample_size = data.len().min(ENTROPY_SAMPLE_CAP);
    // `sample_size >= COMPRESSION_THRESHOLD_BYTES > 0` after the first guard.
    let unique_byte_ratio = unique_byte_count as f64 / sample_size as f64;
    unique_byte_ratio < MAX_UNIQUE_BYTE_RATIO
}

/// Raw-deflates `data`, or `None` when compression is not worth it.
///
/// Returns `None` when the payload is below the threshold, when deflate fails, or
/// when the result is not strictly smaller than the input. That last condition
/// mirrors Swift exactly: it sizes its destination buffer at `data.count` and then
/// guards `compressedSize > 0 && compressedSize < data.count`, so an output that
/// would grow the payload is discarded rather than sent.
pub fn compress(data: &[u8]) -> Option<Vec<u8>> {
    // Skip compression for small data.
    if data.len() < COMPRESSION_THRESHOLD_BYTES {
        return None;
    }

    let mut encoder = DeflateEncoder::new(Vec::with_capacity(data.len()), Compression::default());
    encoder.write_all(data).ok()?;
    let compressed = encoder.finish().ok()?;

    if compressed.is_empty() || compressed.len() >= data.len() {
        return None;
    }
    Some(compressed)
}

/// Inflates a raw deflate stream into a buffer of exactly `original_size` bytes.
///
/// The output buffer is preallocated at `original_size` and never grows, which is
/// what makes this safe to call on hostile input: a stream that would expand past
/// the declared size is simply cut off at it. Callers must still compare the
/// returned length against `original_size` — [`crate::codec::decode`] does — since
/// a truncated stream yields a short buffer rather than an error.
///
/// `original_size` is attacker-controlled, so callers must bound it *before*
/// calling (the codec caps it at the framed-file limit and rejects implausible
/// compression ratios first).
pub fn decompress(data: &[u8], original_size: usize) -> Option<Vec<u8>> {
    // Apple's `compression_decode_buffer` returns 0 for an empty destination, and
    // the caller treats 0 as failure.
    if original_size == 0 {
        return None;
    }

    let mut out = vec![0u8; original_size];
    // `false` => raw deflate, no zlib wrapper. See the module docs.
    let mut inflater = Decompress::new(false);
    inflater
        .decompress(data, &mut out, FlushDecompress::Finish)
        .ok()?;

    let produced = usize::try_from(inflater.total_out()).ok()?;
    if produced == 0 || produced > out.len() {
        return None;
    }
    out.truncate(produced);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    /// A deterministic pseudo-random byte source, so "incompressible" test inputs
    /// do not depend on the platform RNG.
    fn pseudo_random(len: usize) -> Vec<u8> {
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        (0..len)
            .map(|_| {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (state >> 33) as u8
            })
            .collect()
    }

    /// TRAP 1: the wire format is RAW DEFLATE (RFC 1951), never zlib (RFC 1950).
    #[test]
    fn trap_output_is_raw_deflate_not_zlib() {
        let data = b"compress-me".repeat(50);
        let compressed = compress(&data).expect("repetitive data compresses");

        // A raw deflate decoder reads it back exactly.
        let mut raw = flate2::read::DeflateDecoder::new(&compressed[..]);
        let mut round_tripped = Vec::new();
        raw.read_to_end(&mut round_tripped)
            .expect("raw deflate decoder accepts our stream");
        assert_eq!(round_tripped, data);

        // A zlib decoder must NOT: there is no 0x78-style header and no adler32.
        let mut zlib = flate2::read::ZlibDecoder::new(&compressed[..]);
        let mut sink = Vec::new();
        assert!(
            zlib.read_to_end(&mut sink).is_err(),
            "output must not be a valid zlib stream — it is a bare deflate stream"
        );

        // And our own decompressor agrees.
        assert_eq!(
            decompress(&compressed, data.len()).as_deref(),
            Some(&data[..])
        );
    }

    /// TRAP 2: the threshold is 100 bytes. The doc comment saying 256 is wrong.
    #[test]
    fn trap_threshold_is_100_bytes_not_256() {
        let just_under = vec![0x41u8; 99];
        let at_threshold = vec![0x41u8; 100];
        let between = vec![0x41u8; 200];

        assert!(!should_compress(&just_under), "99 bytes is below threshold");
        assert!(should_compress(&at_threshold), ">= 100 is the real guard");
        assert!(
            should_compress(&between),
            "payloads between 100 and 256 bytes DO compress — this is where an \
             implementation built from bitchat's docs diverges from real peers"
        );

        assert!(compress(&just_under).is_none());
        assert!(compress(&at_threshold).is_some());
    }

    /// TRAP 3: numerator counts unique bytes over the WHOLE payload, denominator
    /// is capped at 256. Port the asymmetry, do not normalise it.
    #[test]
    fn trap_entropy_gate_numerator_is_uncapped_denominator_is_capped() {
        // 300 bytes containing 250 distinct values.
        //   as shipped:      250 / min(300, 256) = 250/256 = 0.977 -> NOT compressed
        //   if "fixed":      250 / 300           = 0.833          -> would compress
        let mut asymmetric: Vec<u8> = (0u8..250).collect();
        asymmetric.extend(std::iter::repeat_n(0u8, 50));
        assert_eq!(asymmetric.len(), 300);
        assert!(
            !should_compress(&asymmetric),
            "the capped denominator must make this look high-entropy"
        );

        // Same length, only 128 distinct values: 128/256 = 0.5 -> compressed.
        let low_entropy: Vec<u8> = (0..300).map(|i| (i % 128) as u8).collect();
        assert!(should_compress(&low_entropy));

        // Exactly at the boundary: 230/256 = 0.898 < 0.9 compresses,
        // 231/256 = 0.902 does not.
        let mut at_boundary: Vec<u8> = (0u8..230).collect();
        at_boundary.extend(std::iter::repeat_n(0u8, 26));
        assert_eq!(at_boundary.len(), 256);
        assert!(should_compress(&at_boundary));

        let mut over_boundary: Vec<u8> = (0u8..231).collect();
        over_boundary.extend(std::iter::repeat_n(0u8, 25));
        assert_eq!(over_boundary.len(), 256);
        assert!(!should_compress(&over_boundary));
    }

    #[test]
    fn should_compress_rejects_high_entropy_payloads() {
        // 4096 random bytes hit all 256 values: ratio 1.0, well over the gate.
        assert!(!should_compress(&pseudo_random(4096)));

        // A short random payload is a false negative of the same asymmetry as
        // TRAP 3: ~180 distinct values in 300 bytes is 0.70, under the gate, so
        // the heuristic says "compress" even though the data is incompressible.
        // That is the shipped behaviour and it is harmless — `compress` then
        // declines because the deflate output is not smaller.
        assert!(should_compress(&pseudo_random(300)));
        assert!(compress(&pseudo_random(300)).is_none());
    }

    #[test]
    fn compress_refuses_to_grow_a_payload() {
        let incompressible = pseudo_random(4096);
        assert!(
            compress(&incompressible).is_none(),
            "deflate expands random data; the encoder must decline"
        );

        // Invariant for everything it does accept.
        for len in [100usize, 512, 4096] {
            let data = vec![0x7Eu8; len];
            let compressed = compress(&data).expect("constant data compresses");
            assert!(compressed.len() < data.len());
        }
    }

    #[test]
    fn round_trips_at_a_range_of_sizes() {
        for len in [100usize, 101, 255, 256, 1000, 65_536] {
            let data: Vec<u8> = (0..len).map(|i| (i % 17) as u8).collect();
            let compressed = compress(&data).expect("low-entropy data compresses");
            let restored = decompress(&compressed, data.len()).expect("round trip");
            assert_eq!(restored, data, "failed at len {len}");
        }
    }

    #[test]
    fn decompress_rejects_zero_original_size() {
        let compressed = compress(&[0x01u8; 200]).expect("compresses");
        assert!(decompress(&compressed, 0).is_none());
    }

    #[test]
    fn decompress_rejects_garbage() {
        assert!(decompress(&[0xFF, 0xFF, 0xFF, 0xFF], 64).is_none());
        assert!(decompress(&[], 64).is_none());
    }

    #[test]
    fn decompress_never_writes_past_the_declared_original_size() {
        let data = vec![0x33u8; 4096];
        let compressed = compress(&data).expect("compresses");

        // Understating the size must truncate, never overflow the buffer.
        let short = decompress(&compressed, 64).expect("partial inflate still yields bytes");
        assert_eq!(short.len(), 64);
        assert!(short.iter().all(|&b| b == 0x33));

        // Overstating it yields fewer bytes than claimed; the codec's equality
        // check against `original_size` is what rejects that.
        let over = decompress(&compressed, 8192).expect("inflates what it can");
        assert_eq!(over.len(), 4096);
        assert_ne!(over.len(), 8192);
    }

    #[test]
    fn decompress_of_a_truncated_stream_does_not_reproduce_the_original() {
        let data = vec![0x44u8; 4096];
        let compressed = compress(&data).expect("compresses");
        let truncated = &compressed[..compressed.len() / 2];
        let result = decompress(truncated, data.len());
        assert_ne!(
            result.as_deref(),
            Some(&data[..]),
            "half a stream must not inflate to the whole payload"
        );
    }
}

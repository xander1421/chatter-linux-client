//! Conformance suite: drives `vectors/*.json` against the codec.
//!
//! # What these vectors are, and what they are not
//!
//! Every byte string in `vectors/` comes from one of exactly two places, and each
//! vector says which in its own `derivation` field:
//!
//! 1. **Transcribed** — a byte or hex literal that bitchat's own Swift test suite
//!    hardcodes. These pin the wire format as its authors pinned it.
//! 2. **Hand-derived** — computed by executing the documented Swift algorithm on
//!    paper (or in a scratch script transcribing it line by line) against inputs
//!    lifted from their tests. These pin the format as the *source* specifies it.
//!
//! Nothing here is a guess. Where a value could not be obtained either way — most
//! importantly the exact output of Apple's DEFLATE encoder — no vector exists and
//! the gap is stated rather than papered over. That is why the compressed vectors
//! assert the decode direction only: DEFLATE output is encoder-specific, so a
//! compressed frame's exact bytes are not a property of the protocol at all. What
//! *is* a property of the protocol, and is asserted, is the framing around the
//! stream and the fact that the stream is raw DEFLATE (RFC 1951) rather than zlib.
//!
//! Round-trip assertions (encode → decode → equal) prove self-consistency only.
//! They are here because they catch real mistakes cheaply, not because they say
//! anything about interoperating with a real phone.

use std::fs;
use std::path::PathBuf;

use serde::Deserialize;

use chatter_mesh::codec;
use chatter_mesh::compress;
use chatter_mesh::dedup::MessageDeduplicator;
use chatter_mesh::gcs;
use chatter_mesh::ids::PeerId;
use chatter_mesh::packet::{BitchatPacket, SENDER_ID_LEN, SIGNATURE_LEN};
use chatter_mesh::padding;
use chatter_mesh::tlv::capabilities::PeerCapabilities;
use chatter_mesh::tlv::courier_envelope::{self, CourierEnvelope};

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

fn load<T: for<'de> Deserialize<'de>>(name: &str) -> T {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("vectors");
    path.push(name);
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading vector file {}: {e}", path.display()));
    serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("parsing vector file {}: {e}", path.display()))
}

fn unhex(s: &str) -> Vec<u8> {
    hex::decode(s).unwrap_or_else(|e| panic!("bad hex in vector {s:?}: {e}"))
}

fn unhex_n<const N: usize>(s: &str) -> [u8; N] {
    let bytes = unhex(s);
    assert_eq!(bytes.len(), N, "vector field {s:?} should be {N} bytes");
    let mut out = [0u8; N];
    out.copy_from_slice(&bytes);
    out
}

// ---------------------------------------------------------------------------
// Packet vectors
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct PacketSpec {
    version: u8,
    msg_type: u8,
    ttl: u8,
    timestamp: u64,
    sender_id: String,
    recipient_id: Option<String>,
    payload_hex: String,
    signature_hex: Option<String>,
    route: Option<Vec<String>>,
    #[serde(default)]
    is_rsr: bool,
}

impl PacketSpec {
    fn build(&self) -> BitchatPacket {
        BitchatPacket {
            version: self.version,
            msg_type: self.msg_type,
            ttl: self.ttl,
            timestamp: self.timestamp,
            sender_id: unhex_n::<SENDER_ID_LEN>(&self.sender_id),
            recipient_id: self
                .recipient_id
                .as_deref()
                .map(unhex_n::<SENDER_ID_LEN>),
            payload: unhex(&self.payload_hex),
            signature: self.signature_hex.as_deref().map(unhex_n::<SIGNATURE_LEN>),
            route: self
                .route
                .as_ref()
                .map(|hops| hops.iter().map(|h| unhex_n::<SENDER_ID_LEN>(h)).collect()),
            is_rsr: self.is_rsr,
        }
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize)]
struct FrameVector {
    name: String,
    note: String,
    packet: PacketSpec,
    padding: bool,
    encoded_hex: String,
    #[serde(default = "default_true")]
    check_encode: bool,
    #[serde(default = "default_true")]
    check_decode: bool,
    /// When present, the value the header's payload-length field must carry.
    /// Named separately because it is the field most likely to be got subtly
    /// wrong: it excludes route bytes and includes the original-size field.
    #[serde(default)]
    header_payload_length: Option<u32>,
    /// Compressed vectors decode to a payload that is not the bytes on the wire.
    #[serde(default)]
    decoded_payload_hex: Option<String>,
}

#[derive(Debug, Deserialize)]
struct FrameFile {
    vectors: Vec<FrameVector>,
}

fn check_frames(file: &str) {
    let vectors: FrameFile = load(file);
    assert!(!vectors.vectors.is_empty(), "{file} has no vectors");

    for v in &vectors.vectors {
        let expected = unhex(&v.encoded_hex);
        let packet = v.packet.build();

        if let Some(want) = v.header_payload_length {
            let field: u32 = if packet.version == 2 {
                u32::from_be_bytes([expected[12], expected[13], expected[14], expected[15]])
            } else {
                u32::from(u16::from_be_bytes([expected[12], expected[13]]))
            };
            assert_eq!(
                field, want,
                "[{file}::{}] header payload-length field. {}",
                v.name, v.note
            );
        }

        if v.check_encode {
            let got = codec::encode(&packet, v.padding).unwrap_or_else(|e| {
                panic!("[{file}::{}] encode failed: {e}. {}", v.name, v.note)
            });
            assert_eq!(
                hex::encode(&got),
                v.encoded_hex,
                "[{file}::{}] encode mismatch. {}",
                v.name,
                v.note
            );
        }

        if v.check_decode {
            let got = codec::decode(&expected).unwrap_or_else(|e| {
                panic!("[{file}::{}] decode failed: {e}. {}", v.name, v.note)
            });
            let mut want = packet.clone();
            if let Some(payload) = &v.decoded_payload_hex {
                want.payload = unhex(payload);
            }
            assert_eq!(got, want, "[{file}::{}] decode mismatch. {}", v.name, v.note);
        }
    }
}

#[test]
fn binary_protocol_vectors() {
    check_frames("binary_protocol.json");
}

/// Compressed frames: decode direction only. See the module comment for why the
/// encode direction is not — and cannot be — pinned byte-for-byte.
#[test]
fn compressed_frame_vectors() {
    check_frames("compressed.json");
}

/// A compressible payload must still survive our own encode → decode, even
/// though the intermediate bytes are ours rather than Apple's.
#[test]
fn compressed_frames_round_trip_through_our_own_encoder() {
    let vectors: FrameFile = load("compressed.json");
    for v in &vectors.vectors {
        let mut packet = v.packet.build();
        if let Some(payload) = &v.decoded_payload_hex {
            packet.payload = unhex(payload);
        }
        let encoded = codec::encode(&packet, false)
            .unwrap_or_else(|e| panic!("[{}] encode failed: {e}", v.name));

        let flags = encoded[codec::FLAGS_OFFSET];
        assert_ne!(
            flags & codec::flags::IS_COMPRESSED,
            0,
            "[{}] payload is compressible and above the 100-byte threshold, so \
             IS_COMPRESSED must be set",
            v.name
        );
        assert!(
            encoded.len() < codec::V2_HEADER_SIZE + SENDER_ID_LEN + packet.payload.len(),
            "[{}] compression that does not shrink the frame is pointless",
            v.name
        );

        let decoded = codec::decode(&encoded)
            .unwrap_or_else(|e| panic!("[{}] decode failed: {e}", v.name));
        assert_eq!(decoded, packet, "[{}] round trip", v.name);
    }
}

// ---------------------------------------------------------------------------
// Malformed frames
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct MalformedVector {
    name: String,
    note: String,
    encoded_hex: String,
}

#[derive(Debug, Deserialize)]
struct MalformedFile {
    vectors: Vec<MalformedVector>,
}

#[test]
fn malformed_frames_are_rejected_without_panicking() {
    let file: MalformedFile = load("malformed.json");
    assert!(!file.vectors.is_empty());
    for v in &file.vectors {
        let bytes = unhex(&v.encoded_hex);
        assert!(
            codec::decode(&bytes).is_err(),
            "[malformed::{}] decoded successfully but must not. {}",
            v.name,
            v.note
        );
    }
}

// ---------------------------------------------------------------------------
// Signing preimage
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct SigningVector {
    name: String,
    note: String,
    packet: PacketSpec,
    preimage_hex: String,
}

#[derive(Debug, Deserialize)]
struct SigningFile {
    vectors: Vec<SigningVector>,
}

#[test]
fn signing_preimage_is_the_padded_ttl_zero_unsigned_encoding() {
    let file: SigningFile = load("signing.json");
    assert!(!file.vectors.is_empty());
    for v in &file.vectors {
        let packet = v.packet.build();
        let got = packet
            .signing_preimage()
            .unwrap_or_else(|e| panic!("[signing::{}] preimage failed: {e}", v.name));
        assert_eq!(
            hex::encode(&got),
            v.preimage_hex,
            "[signing::{}] {}",
            v.name,
            v.note
        );

        // The three mutable fields must make no difference to the preimage.
        let relayed = BitchatPacket {
            ttl: packet.ttl.saturating_sub(1),
            is_rsr: !packet.is_rsr,
            signature: Some([0x5A; SIGNATURE_LEN]),
            ..packet.clone()
        };
        assert_eq!(
            relayed.signing_preimage().ok(),
            Some(got),
            "[signing::{}] a relayed copy must produce the identical preimage — \
             ttl, IS_RSR and the signature are all excluded",
            v.name
        );
    }
}

// ---------------------------------------------------------------------------
// Padding
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct BlockSizeCase {
    len: usize,
    block: usize,
}

#[derive(Debug, Deserialize)]
struct PadCase {
    name: String,
    input_hex: String,
    target: usize,
    output_hex: String,
    note: String,
}

#[derive(Debug, Deserialize)]
struct UnpadCase {
    name: String,
    input_hex: String,
    output_hex: String,
    note: String,
}

#[derive(Debug, Deserialize)]
struct PaddingFile {
    optimal_block_size: Vec<BlockSizeCase>,
    pad: Vec<PadCase>,
    unpad: Vec<UnpadCase>,
}

#[test]
fn padding_vectors() {
    let file: PaddingFile = load("padding.json");

    for c in &file.optimal_block_size {
        assert_eq!(
            padding::optimal_block_size(c.len),
            c.block,
            "optimal_block_size({})",
            c.len
        );
    }

    for c in &file.pad {
        let input = unhex(&c.input_hex);
        assert_eq!(
            hex::encode(padding::pad(&input, c.target)),
            c.output_hex,
            "[pad::{}] {}",
            c.name,
            c.note
        );
    }

    for c in &file.unpad {
        let input = unhex(&c.input_hex);
        assert_eq!(
            hex::encode(padding::unpad(&input)),
            c.output_hex,
            "[unpad::{}] {}",
            c.name,
            c.note
        );
    }
}

// ---------------------------------------------------------------------------
// Compression heuristic
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ShouldCompressCase {
    name: String,
    input_hex: String,
    expected: bool,
    unique_bytes: usize,
    sample_size: usize,
    note: String,
}

#[derive(Debug, Deserialize)]
struct DecompressCase {
    name: String,
    raw_deflate_hex: String,
    original_size: usize,
    expected_hex: String,
    note: String,
}

#[derive(Debug, Deserialize)]
struct CompressFile {
    should_compress: Vec<ShouldCompressCase>,
    decompress: Vec<DecompressCase>,
}

#[test]
fn should_compress_vectors() {
    let file: CompressFile = load("compress.json");
    for c in &file.should_compress {
        let input = unhex(&c.input_hex);
        assert_eq!(
            compress::should_compress(&input),
            c.expected,
            "[should_compress::{}] {} unique={} sample={}",
            c.name,
            c.note,
            c.unique_bytes,
            c.sample_size
        );
    }
}

/// The raw-DEFLATE-vs-zlib trap, isolated. These streams carry no zlib header
/// and no adler32 trailer; a `ZlibDecoder` rejects every one of them.
#[test]
fn decompress_accepts_raw_deflate_streams() {
    let file: CompressFile = load("compress.json");
    for c in &file.decompress {
        let stream = unhex(&c.raw_deflate_hex);
        let got = compress::decompress(&stream, c.original_size).unwrap_or_else(|| {
            panic!(
                "[decompress::{}] raw DEFLATE stream rejected — zlib framing? {}",
                c.name, c.note
            )
        });
        assert_eq!(hex::encode(&got), c.expected_hex, "[decompress::{}]", c.name);
    }
}

/// Our own compressor must emit something our own decompressor reads back, and
/// must emit raw DEFLATE: a zlib stream starts 0x78 and carries a 4-byte adler32
/// tail, so a round trip alone would not catch the framing mistake. The check
/// that does catch it is `decompress_accepts_raw_deflate_streams` above; this one
/// guards the far cheaper failure of not shrinking at all.
#[test]
fn compress_round_trips_and_shrinks() {
    let file: CompressFile = load("compress.json");
    for c in &file.decompress {
        let original = unhex(&c.expected_hex);
        let compressed = compress::compress(&original)
            .unwrap_or_else(|| panic!("[compress::{}] refused a compressible payload", c.name));
        assert!(
            compressed.len() < original.len(),
            "[compress::{}] output is not smaller than input",
            c.name
        );
        assert_eq!(
            compress::decompress(&compressed, original.len()).as_deref(),
            Some(original.as_slice()),
            "[compress::{}] round trip",
            c.name
        );
    }
}

// ---------------------------------------------------------------------------
// Packet ID
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct PacketIdCase {
    name: String,
    msg_type: u8,
    sender_id: String,
    timestamp: u64,
    payload_hex: String,
    packet_id_hex: String,
    note: String,
}

#[derive(Debug, Deserialize)]
struct PacketIdFile {
    vectors: Vec<PacketIdCase>,
}

#[test]
fn packet_id_vectors() {
    let file: PacketIdFile = load("packet_id.json");
    for c in &file.vectors {
        let packet = BitchatPacket {
            version: 1,
            msg_type: c.msg_type,
            // Deliberately noisy: every field below the hash's four inputs is set
            // to something arbitrary, because none of them may reach the digest.
            ttl: 7,
            timestamp: c.timestamp,
            sender_id: unhex_n::<SENDER_ID_LEN>(&c.sender_id),
            recipient_id: Some([0x77; SENDER_ID_LEN]),
            payload: unhex(&c.payload_hex),
            signature: Some([0x5A; SIGNATURE_LEN]),
            route: Some(vec![[0x33; SENDER_ID_LEN]]),
            is_rsr: true,
        };
        assert_eq!(
            hex::encode(packet.packet_id()),
            c.packet_id_hex,
            "[packet_id::{}] {}",
            c.name,
            c.note
        );
    }
}

// ---------------------------------------------------------------------------
// Dedup keys
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct DedupCase {
    name: String,
    sender_id: String,
    timestamp: u64,
    msg_type: u8,
    payload_hex: String,
    key: String,
    note: String,
}

#[derive(Debug, Deserialize)]
struct DedupFile {
    vectors: Vec<DedupCase>,
}

#[test]
fn dedup_key_vectors() {
    let file: DedupFile = load("dedup.json");
    for c in &file.vectors {
        let packet = BitchatPacket {
            version: 1,
            msg_type: c.msg_type,
            ttl: 4,
            timestamp: c.timestamp,
            sender_id: unhex_n::<SENDER_ID_LEN>(&c.sender_id),
            recipient_id: None,
            payload: unhex(&c.payload_hex),
            signature: None,
            route: None,
            is_rsr: false,
        };
        assert_eq!(
            MessageDeduplicator::dedup_key(&packet),
            c.key,
            "[dedup::{}] {}",
            c.name,
            c.note
        );
    }
}

/// The trap the digest exists for: three packets from one post-handshake flush
/// share a sender, a millisecond, and (for two of them) a type. All three must
/// pass the deduplicator.
#[test]
fn post_handshake_flush_survives_deduplication() {
    let file: DedupFile = load("dedup.json");
    let flush: Vec<&DedupCase> = file
        .vectors
        .iter()
        .filter(|c| c.timestamp == 1_700_000_000_000)
        .collect();
    assert!(
        flush.len() >= 3,
        "expected at least three same-millisecond vectors"
    );

    let mut dedup = MessageDeduplicator::new(1000, 300);
    for c in &flush {
        assert!(
            !dedup.is_duplicate(&c.key, 1_000),
            "[dedup::{}] collapsed into an earlier packet from the same \
             millisecond. {}",
            c.name,
            c.note
        );
    }
    // And every one of them is a duplicate the second time around.
    for c in &flush {
        assert!(
            dedup.is_duplicate(&c.key, 1_000),
            "[dedup::{}] a genuine replay was let through",
            c.name
        );
    }
}

// ---------------------------------------------------------------------------
// GCS filter
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct GcsBuildCase {
    name: String,
    ids_hex: Vec<String>,
    max_bytes: usize,
    target_fpr: f64,
    p: u32,
    m: u32,
    data_hex: String,
    included_count: usize,
    decoded: Vec<u64>,
    #[serde(default)]
    decoded_truncated_half: Option<Vec<u64>>,
    note: String,
}

#[derive(Debug, Deserialize)]
struct GcsBucketCase {
    name: String,
    id_hex: String,
    modulus: u32,
    expected: u64,
    note: String,
}

#[derive(Debug, Deserialize)]
struct GcsRejectCase {
    name: String,
    p: u32,
    m: u32,
    data_hex: String,
}

#[derive(Debug, Deserialize)]
struct GcsFile {
    build_filter: Vec<GcsBuildCase>,
    bucket: Vec<GcsBucketCase>,
    decode_rejects: Vec<GcsRejectCase>,
}

#[test]
fn gcs_build_filter_vectors() {
    let file: GcsFile = load("gcs.json");
    for c in &file.build_filter {
        let ids: Vec<[u8; 16]> = c.ids_hex.iter().map(|h| unhex_n::<16>(h)).collect();
        let params = gcs::build_filter(&ids, c.max_bytes, c.target_fpr);

        assert_eq!(params.p, c.p, "[gcs::{}] p. {}", c.name, c.note);
        assert_eq!(params.m, c.m, "[gcs::{}] m. {}", c.name, c.note);
        assert_eq!(
            hex::encode(&params.data),
            c.data_hex,
            "[gcs::{}] encoded bitstream. {}",
            c.name,
            c.note
        );
        assert_eq!(
            params.included, c.included_count,
            "[gcs::{}] included count. {}",
            c.name, c.note
        );

        let decoded = gcs::decode_to_sorted(params.p, params.m, &params.data);
        assert_eq!(decoded, c.decoded, "[gcs::{}] decoded set", c.name);
        assert!(
            decoded.windows(2).all(|w| w[0] < w[1]),
            "[gcs::{}] decoded values must be strictly ascending",
            c.name
        );
        for value in &decoded {
            assert!(
                gcs::contains(&decoded, *value),
                "[gcs::{}] contains() missed a value it decoded",
                c.name
            );
        }

        if let Some(half) = &c.decoded_truncated_half {
            let cut = params.data.len() / 2;
            let truncated = gcs::decode_to_sorted(params.p, params.m, &params.data[..cut]);
            assert_eq!(
                &truncated, half,
                "[gcs::{}] truncated decode. Truncation must yield a prefix of \
                 the full set and never invent a value.",
                c.name
            );
        }
    }
}

#[test]
fn gcs_bucket_vectors() {
    let file: GcsFile = load("gcs.json");
    for c in &file.bucket {
        let id = unhex_n::<16>(&c.id_hex);
        assert_eq!(
            gcs::bucket(&id, c.modulus),
            c.expected,
            "[gcs::bucket::{}] {}",
            c.name,
            c.note
        );
    }
}

#[test]
fn gcs_decode_rejects_out_of_range_parameters() {
    let file: GcsFile = load("gcs.json");
    for c in &file.decode_rejects {
        let data = unhex(&c.data_hex);
        assert!(
            gcs::decode_to_sorted(c.p, c.m, &data).is_empty(),
            "[gcs::reject::{}] decoded garbage instead of returning empty",
            c.name
        );
    }
}

// ---------------------------------------------------------------------------
// Courier envelope
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct EnvelopeSpec {
    recipient_tag: String,
    expiry: u64,
    ciphertext_hex: String,
    copies: u8,
    prekey_id: Option<u32>,
}

impl EnvelopeSpec {
    fn build(&self) -> CourierEnvelope {
        CourierEnvelope::new(
            unhex_n::<16>(&self.recipient_tag),
            self.expiry,
            unhex(&self.ciphertext_hex),
            self.copies,
            self.prekey_id,
        )
    }
}

#[derive(Debug, Deserialize)]
struct EnvelopeVector {
    name: String,
    note: String,
    envelope: EnvelopeSpec,
    encoded_hex: String,
    check_encode: bool,
    check_decode: bool,
}

#[derive(Debug, Deserialize)]
struct EnvelopeRejectCase {
    name: String,
    encoded_hex: String,
    note: String,
}

#[derive(Debug, Deserialize)]
struct RecipientTagCase {
    noise_static_key: String,
    epoch_day: u32,
    tag_hex: String,
}

#[derive(Debug, Deserialize)]
struct EnvelopeFile {
    vectors: Vec<EnvelopeVector>,
    decode_rejects: Vec<EnvelopeRejectCase>,
    recipient_tags: Vec<RecipientTagCase>,
}

#[test]
fn courier_envelope_vectors() {
    let file: EnvelopeFile = load("courier_envelope.json");
    for v in &file.vectors {
        let envelope = v.envelope.build();
        if v.check_encode {
            let got = envelope
                .encode()
                .unwrap_or_else(|e| panic!("[courier::{}] encode failed: {e}", v.name));
            assert_eq!(
                hex::encode(&got),
                v.encoded_hex,
                "[courier::{}] {}",
                v.name,
                v.note
            );
        }
        if v.check_decode {
            let got = CourierEnvelope::decode(&unhex(&v.encoded_hex))
                .unwrap_or_else(|e| panic!("[courier::{}] decode failed: {e}", v.name));
            assert_eq!(got, envelope, "[courier::{}] {}", v.name, v.note);
        }
    }
}

#[test]
fn courier_envelope_decode_rejects() {
    let file: EnvelopeFile = load("courier_envelope.json");
    for c in &file.decode_rejects {
        assert!(
            CourierEnvelope::decode(&unhex(&c.encoded_hex)).is_err(),
            "[courier::reject::{}] {}",
            c.name,
            c.note
        );
    }
}

#[test]
fn courier_envelope_encode_rejects_bad_ciphertext() {
    // The tag-length rejection from `encodeRejectsInvalidFields` is unreachable
    // here by construction: our recipient tag is a `[u8; 16]`, so a short tag has
    // no representation to reject. The other two are live.
    let tag = [0xAB; courier_envelope::TAG_LEN];
    let empty = CourierEnvelope::new(tag, 1_900_000_000_000, Vec::new(), 1, None);
    assert!(empty.encode().is_err(), "empty ciphertext must be refused");

    let oversized = CourierEnvelope::new(
        tag,
        1_900_000_000_000,
        vec![0u8; courier_envelope::MAX_CIPHERTEXT_BYTES + 1],
        1,
        None,
    );
    assert!(
        oversized.encode().is_err(),
        "ciphertext above {} bytes must be refused",
        courier_envelope::MAX_CIPHERTEXT_BYTES
    );

    let at_limit = CourierEnvelope::new(
        tag,
        1_900_000_000_000,
        vec![0x01; courier_envelope::MAX_CIPHERTEXT_BYTES],
        1,
        None,
    );
    let encoded = at_limit
        .encode()
        .expect("exactly maxCiphertextBytes is legal");
    assert_eq!(
        CourierEnvelope::decode(&encoded).ok(),
        Some(at_limit),
        "round trip at the ciphertext limit"
    );
}

#[test]
fn courier_recipient_tag_vectors() {
    let file: EnvelopeFile = load("courier_envelope.json");
    for c in &file.recipient_tags {
        let key = unhex(&c.noise_static_key);
        assert_eq!(
            hex::encode(courier_envelope::recipient_tag(&key, c.epoch_day)),
            c.tag_hex,
            "recipient tag for day {}",
            c.epoch_day
        );
    }
    // Determinism and rotation, the two properties the tag exists for.
    let key = [0x11u8; 32];
    assert_eq!(
        courier_envelope::recipient_tag(&key, 20_000),
        courier_envelope::recipient_tag(&key, 20_000)
    );
    assert_ne!(
        courier_envelope::recipient_tag(&key, 20_000),
        courier_envelope::recipient_tag(&key, 20_001)
    );
    assert_ne!(
        courier_envelope::recipient_tag(&key, 20_000),
        courier_envelope::recipient_tag(&[0x22u8; 32], 20_000)
    );
}

// ---------------------------------------------------------------------------
// Peer capabilities
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct CapabilityEncodeCase {
    name: String,
    bits: u64,
    encoded_hex: String,
    note: String,
}

#[derive(Debug, Deserialize)]
struct CapabilityDecodeCase {
    name: String,
    encoded_hex: String,
    bits: u64,
    note: String,
}

#[derive(Debug, Deserialize)]
struct CapabilityFile {
    encode: Vec<CapabilityEncodeCase>,
    decode: Vec<CapabilityDecodeCase>,
}

#[test]
fn peer_capabilities_vectors() {
    let file: CapabilityFile = load("capabilities.json");
    for c in &file.encode {
        let caps = PeerCapabilities::from_bits(c.bits);
        let got = caps
            .encode()
            .unwrap_or_else(|e| panic!("[caps::{}] encode failed: {e}", c.name));
        assert_eq!(
            hex::encode(&got),
            c.encoded_hex,
            "[caps::{}] {}",
            c.name,
            c.note
        );
        assert_eq!(
            PeerCapabilities::decode(&got).map(|d| d.bits()).ok(),
            Some(c.bits),
            "[caps::{}] round trip",
            c.name
        );
    }
    for c in &file.decode {
        let got = PeerCapabilities::decode(&unhex(&c.encoded_hex))
            .unwrap_or_else(|e| panic!("[caps::{}] decode failed: {e}", c.name));
        assert_eq!(got.bits(), c.bits, "[caps::{}] {}", c.name, c.note);
    }
}

// ---------------------------------------------------------------------------
// Peer IDs
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct PeerIdFromKeyCase {
    name: String,
    public_key_hex: String,
    short_id: String,
    routing_data_hex: String,
    note: String,
}

#[derive(Debug, Deserialize)]
struct PeerIdRoutingCase {
    name: String,
    id: String,
    routing_data_hex: String,
    note: String,
}

#[derive(Debug, Deserialize)]
struct PeerIdFile {
    from_public_key: Vec<PeerIdFromKeyCase>,
    routing: Vec<PeerIdRoutingCase>,
}

#[test]
fn peer_id_vectors() {
    let file: PeerIdFile = load("peer_id.json");

    for c in &file.from_public_key {
        let key = unhex(&c.public_key_hex);
        let derived = PeerId::from_noise_key(&key);
        assert_eq!(derived.as_str(), c.short_id, "[peer_id::{}] {}", c.name, c.note);
        assert_eq!(
            derived.to_routing_bytes().map(hex::encode),
            Some(c.routing_data_hex.clone()),
            "[peer_id::{}] routing form",
            c.name
        );
        // The bug the Swift regression test documents: truncating the raw key
        // instead of hashing it must not land on the same ID.
        let mut first_eight = [0u8; SENDER_ID_LEN];
        first_eight.copy_from_slice(&key[..SENDER_ID_LEN]);
        let truncated = PeerId::from_bytes(&first_eight);
        assert_ne!(
            truncated, derived,
            "[peer_id::{}] the first 8 key bytes must not equal the fingerprint",
            c.name
        );
    }

    for c in &file.routing {
        let peer = PeerId::from_str(&c.id);
        assert_eq!(
            peer.to_routing_bytes().map(hex::encode),
            Some(c.routing_data_hex.clone()),
            "[peer_id::{}] {}",
            c.name,
            c.note
        );
        // Round trip through the wire form.
        let bytes = unhex_n::<SENDER_ID_LEN>(&c.routing_data_hex);
        assert_eq!(
            PeerId::from_bytes(&bytes).to_routing_bytes(),
            Some(bytes),
            "[peer_id::{}] wire round trip",
            c.name
        );
    }
}

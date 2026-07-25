//! Peer identity.
//!
//! Ported from `BitFoundation/PeerID.swift`. (The gossip-sync packet ID from
//! `Sync/PacketIdUtil.swift` lives on [`crate::packet::BitchatPacket::packet_id`],
//! not here.)
//!
//! A peer ID is a *string* with an optional typed namespace prefix, and the string
//! is the identity — it travels in TLVs, in UI state, and as a map key. Only some
//! of them are routable: 16 hex characters (8 bytes) is the mesh routing form, and
//! everything else either reduces to that form or is not a mesh peer at all.
//!
//! # Case
//!
//! Swift lowercases `bare` at construction and leaves `prefix` as the literal it
//! matched. Since every prefix literal is already lowercase, the resulting `id` is
//! exactly `input.lowercased()`, which is what we store.
//!
//! One consequence differs from Swift: `PeerID(str: "MESH:AB")` keeps the *empty*
//! prefix over there (`hasPrefix` is case-sensitive, so nothing matched before the
//! lowercasing), while its `id` still renders as `"mesh:ab"`. We store one string
//! and re-derive the prefix from it, so we report the `mesh:` prefix for that
//! input. The two disagree only for inputs whose prefix arrives uppercased, which
//! no wire format or UI path produces; the `id` string — the thing peers compare —
//! is identical either way. Swift's own `Equatable` is on `(prefix, bare)`, which
//! makes `"MESH:AB"` and `"mesh:AB"` *unequal* there despite identical `id`s. Ours
//! calls them equal, which is the more defensible of the two behaviours.

use core::fmt;
use core::fmt::Write as _;

use sha2::{Digest, Sha256};

/// A prefix-less ID this long (in hex characters) is a mesh routing ID.
const HEX_ID_LEN: usize = 16;
/// Maximum ID length, and also the exact length of a full Noise key in hex.
const MAX_ID_LEN: usize = 64;

/// `"mesh:"` — an explicitly mesh-scoped peer.
pub const PREFIX_MESH: &str = "mesh:";
/// `"name:"` — a nickname, not a key.
pub const PREFIX_NAME: &str = "name:";
/// `"noise:"` (+ 64 hex) — a full Noise static public key.
pub const PREFIX_NOISE: &str = "noise:";
/// `"nostr_"` (+ 16 hex) — a geohash DM conversation key.
pub const PREFIX_GEO_DM: &str = "nostr_";
/// `"nostr:"` (+ 8 hex) — a geohash chat participant.
pub const PREFIX_GEO_CHAT: &str = "nostr:";
/// `"group_"` (+ 32 hex) — a virtual conversation ID for a private group.
/// Never routed to a single peer.
pub const PREFIX_GROUP: &str = "group_";
/// `"bridge:"` (+ 16 hex) — a sender reached across a mesh bridge, identified by
/// their rendezvous Nostr pubkey. Not a routable mesh peer.
pub const PREFIX_BRIDGE: &str = "bridge:";

/// Every non-empty prefix, in `PeerID.Prefix.allCases` order.
///
/// The order is the match order in Swift's `first(where:)`. No two of these are a
/// prefix of each other, so the order does not actually change any outcome — but
/// keeping it identical means it stays correct if bitchat ever adds an overlapping
/// one.
const PREFIXES: [&str; 7] = [
    PREFIX_MESH,
    PREFIX_NAME,
    PREFIX_NOISE,
    PREFIX_GEO_DM,
    PREFIX_GEO_CHAT,
    PREFIX_GROUP,
    PREFIX_BRIDGE,
];

/// A bitchat peer ID: an optional namespace prefix plus a bare body.
///
/// Ordering is over the whole `id` string, matching Swift's `Comparable`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PeerId(String);

impl PeerId {
    /// Parses any peer ID string. Never fails — an unrecognized shape is still a
    /// `PeerId`, it is just not [`is_valid`](Self::is_valid).
    #[allow(clippy::should_implement_trait)] // mirrors Swift's `init(str:)`; parsing is infallible
    pub fn from_str(s: &str) -> Self {
        Self(s.to_lowercase())
    }

    /// The stable 16-hex mesh ID derived from a Noise static public key:
    /// `SHA256(public_key)` rendered as hex, first 16 characters.
    ///
    /// Note what this is *not*: the first 8 bytes of the key itself. bitchat used
    /// to truncate the raw key and it broke file transfer in both directions
    /// (`PeerIDTests.fileTransfer_truncatedRawKeyDoesNotMatchMyPeerID` is the
    /// regression test they kept).
    pub fn from_noise_key(public_key: &[u8]) -> Self {
        let digest = Sha256::digest(public_key);
        // First 8 bytes of the digest == first 16 characters of its hex rendering.
        Self(hex_encode(digest.iter().take(8)))
    }

    /// Builds a peer ID from its 8-byte wire routing form.
    pub fn from_bytes(id: &[u8; 8]) -> Self {
        Self::from_str(&hex_encode(id.iter()))
    }

    /// The full ID string, prefix included.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The namespace prefix, or `None` when there is none.
    pub fn prefix(&self) -> Option<&str> {
        PREFIXES.into_iter().find(|p| self.0.starts_with(p))
    }

    /// The ID with its namespace prefix stripped.
    pub fn bare(&self) -> &str {
        match self.prefix() {
            Some(p) => self.0.get(p.len()..).unwrap_or(""),
            None => &self.0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// True when the ID starts with `"nostr:"`.
    pub fn is_geo_chat(&self) -> bool {
        self.prefix() == Some(PREFIX_GEO_CHAT)
    }

    /// True when the ID starts with `"nostr_"`.
    pub fn is_geo_dm(&self) -> bool {
        self.prefix() == Some(PREFIX_GEO_DM)
    }

    /// True when the ID starts with `"group_"`.
    pub fn is_group(&self) -> bool {
        self.prefix() == Some(PREFIX_GROUP)
    }

    /// True when the ID starts with `"bridge:"`.
    pub fn is_bridged(&self) -> bool {
        self.prefix() == Some(PREFIX_BRIDGE)
    }

    /// The 16-byte group ID behind a `"group_"` peer ID, if this is one.
    pub fn group_id(&self) -> Option<[u8; 16]> {
        if !self.is_group() {
            return None;
        }
        let bare = self.bare();
        if bare.chars().count() != 32 {
            return None;
        }
        let bytes = hex_decode(bare)?;
        let mut out = [0u8; 16];
        if bytes.len() != out.len() {
            return None;
        }
        out.copy_from_slice(&bytes);
        Some(out)
    }

    /// The full Noise static public key, when the bare part is exactly 64 hex
    /// characters.
    pub fn noise_key(&self) -> Option<Vec<u8>> {
        let bare = self.bare();
        if bare.chars().count() != MAX_ID_LEN {
            return None;
        }
        hex_decode(bare)
    }

    /// Reduces a full 64-hex Noise key to its 16-hex short form; returns `self`
    /// unchanged for anything else.
    pub fn to_short(&self) -> PeerId {
        match self.noise_key() {
            Some(key) => PeerId::from_noise_key(&key),
            None => self.clone(),
        }
    }

    /// The 8-byte wire routing form, when this ID has one.
    ///
    /// Tries, in Swift's order: the whole ID as hex, then the bare part as hex,
    /// then the short (fingerprint) form.
    ///
    /// **Deviation, deliberate:** Swift's `routingData` returns `Data?` and its
    /// final branch can hand back a byte count other than 8 — for a `PeerID` whose
    /// bare part is, say, 4 hex characters it returns 2 bytes. Every consumer then
    /// either rejects it (`init?(routingData:)` requires 8) or would write a short
    /// field. Our return type is `[u8; 8]`, so a non-8 result is `None`: the same
    /// outcome at every real call site, made explicit at the source.
    pub fn to_routing_bytes(&self) -> Option<[u8; 8]> {
        if let Some(eight) = hex_decode(&self.0).as_deref().and_then(to_eight) {
            return Some(eight);
        }
        if let Some(eight) = hex_decode(self.bare()).as_deref().and_then(to_eight) {
            return Some(eight);
        }
        hex_decode(self.to_short().as_str()).as_deref().and_then(to_eight)
    }

    /// Whether the bare part is entirely hex digits.
    ///
    /// Swift uses `Character.isHexDigit`, which also accepts full-width Unicode
    /// digits; we require ASCII, matching what `Data(hexString:)` will actually
    /// decode. The stricter rule is the one that agrees with `noise_key`.
    pub fn is_hex(&self) -> bool {
        self.bare().chars().all(|c| c.is_ascii_hexdigit())
    }

    /// A short routing ID: exactly 16 hex characters.
    pub fn is_short(&self) -> bool {
        let bare = self.bare();
        bare.chars().count() == HEX_ID_LEN && bare.chars().all(|c| c.is_ascii_hexdigit())
    }

    /// A full Noise key in hex: exactly 64 hex characters.
    pub fn is_noise_key_hex(&self) -> bool {
        self.noise_key().is_some()
    }

    /// Validates an ID from any source: short 16-hex, full 64-hex, or an internal
    /// alphanumeric/`-`/`_` form shorter than 64 characters.
    ///
    /// Prefixes are peeled and the *bare* value is what gets validated, recursively
    /// — `"name:mesh:abcdefg"` is valid because `"abcdefg"` is. Swift recurses;
    /// we loop, because the input is attacker-supplied and a string of a hundred
    /// thousand stacked `"name:"`s would otherwise be a stack overflow.
    pub fn is_valid(&self) -> bool {
        let mut id: &str = &self.0;
        // Peel prefixes. Each iteration consumes at least five characters, so this
        // terminates; `bare` of a prefix-less ID is the ID itself.
        while let Some(p) = PREFIXES.into_iter().find(|p| id.starts_with(p)) {
            match id.get(p.len()..) {
                Some(rest) => id = rest,
                None => return false,
            }
        }

        let len = id.chars().count();

        // Short routing IDs (exact 16-hex) and full Noise keys (exact 64-hex).
        // The two use different tests in Swift — `isShort` walks the characters,
        // `isNoiseKeyHex` goes through `Data(hexString:)` — and the difference is
        // visible: a 64-character string of `"0x"` plus 62 hex digits decodes, so
        // it validates as a key. Reproduced rather than tidied up, because
        // "tidying" here means rejecting an ID a real peer considers valid.
        let is_short = len == HEX_ID_LEN && id.chars().all(|c| c.is_ascii_hexdigit());
        let is_noise_key = len == MAX_ID_LEN && hex_decode(id).is_some();
        if is_short || is_noise_key {
            return true;
        }

        // Right length for a key but not hex: reject rather than fall through to
        // the permissive internal form.
        if len == HEX_ID_LEN || len == MAX_ID_LEN {
            return false;
        }

        // Internal format: alphanumerics plus dash and underscore, under 64.
        !id.is_empty()
            && len < MAX_ID_LEN
            && id.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_')
    }
}

impl fmt::Display for PeerId {
    /// Renders the full `id`, matching Swift's `CustomStringConvertible` — which
    /// is what string-interpolates into the dedup key, so this is load-bearing.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<PeerId> for String {
    fn from(value: PeerId) -> Self {
        value.0
    }
}

fn to_eight(bytes: &[u8]) -> Option<[u8; 8]> {
    let mut out = [0u8; 8];
    if bytes.len() != out.len() {
        return None;
    }
    out.copy_from_slice(bytes);
    Some(out)
}

/// Lowercase hex, no separators — matches `Data.hexEncodedString()`'s `"%02x"`.
pub(crate) fn hex_encode<'a, I: IntoIterator<Item = &'a u8>>(bytes: I) -> String {
    let iter = bytes.into_iter();
    let (lower, _) = iter.size_hint();
    let mut out = String::with_capacity(lower * 2);
    for b in iter {
        // Writing to a String is infallible; the Result exists only to satisfy
        // the `fmt::Write` signature.
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Mirrors `Data(hexString:)`: trims surrounding whitespace, accepts an optional
/// `0x`/`0X` prefix, requires an even length, and rejects any non-hex character.
/// An empty string decodes to an empty vector, exactly as it does there.
// `usize::is_multiple_of` would read better but landed in 1.87, and this crate
// declares `rust-version = "1.85"`.
#[allow(clippy::manual_is_multiple_of)]
pub(crate) fn hex_decode(s: &str) -> Option<Vec<u8>> {
    let trimmed = s.trim();
    let body = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
        .unwrap_or(trimmed);

    let bytes = body.as_bytes();
    if bytes.len() % 2 != 0 {
        return None;
    }
    if bytes.is_empty() {
        return Some(Vec::new());
    }

    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks_exact(2) {
        let (Some(hi), Some(lo)) = (pair.first(), pair.get(1)) else {
            return None;
        };
        // Any byte of a multi-byte UTF-8 character is >= 0x80 and fails here, so
        // operating on bytes cannot split a character into something decodable.
        let hi = char::from(*hi).to_digit(16)?;
        let lo = char::from(*lo).to_digit(16)?;
        out.push(((hi << 4) | lo) as u8);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEX16: &str = "0011223344556677";
    const HEX64: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

    #[test]
    fn no_prefix_ids_keep_their_whole_string_as_bare() {
        for id in [HEX16, HEX64] {
            let peer = PeerId::from_str(id);
            assert_eq!(peer.as_str(), id);
            assert_eq!(peer.bare(), id);
            assert_eq!(peer.prefix(), None);
        }
    }

    #[test]
    fn every_prefix_splits_correctly() {
        let cases = [
            (PREFIX_MESH, HEX16),
            (PREFIX_MESH, HEX64),
            (PREFIX_NAME, "some_name"),
            (PREFIX_NOISE, HEX16),
            (PREFIX_NOISE, HEX64),
            (PREFIX_GEO_DM, HEX16),
            (PREFIX_GEO_DM, HEX64),
            (PREFIX_GEO_CHAT, HEX16),
            (PREFIX_GEO_CHAT, HEX64),
            (PREFIX_GROUP, HEX16),
            (PREFIX_BRIDGE, HEX16),
        ];
        for (prefix, bare) in cases {
            let full = format!("{prefix}{bare}");
            let peer = PeerId::from_str(&full);
            assert_eq!(peer.as_str(), full);
            assert_eq!(peer.prefix(), Some(prefix));
            assert_eq!(peer.bare(), bare);
        }
    }

    #[test]
    fn unknown_prefix_falls_back_to_no_prefix() {
        let raw = format!("unknown:{HEX16}");
        let peer = PeerId::from_str(&raw);
        assert_eq!(peer.as_str(), raw);
        assert_eq!(peer.bare(), raw);
        assert_eq!(peer.prefix(), None);
    }

    #[test]
    fn prefix_with_empty_bare_is_still_a_prefix() {
        let peer = PeerId::from_str("mesh:");
        assert_eq!(peer.as_str(), "mesh:");
        assert_eq!(peer.bare(), "");
        assert_eq!(peer.prefix(), Some(PREFIX_MESH));
    }

    #[test]
    fn construction_lowercases() {
        assert_eq!(PeerId::from_str("AAA"), PeerId::from_str("aaa"));
        assert_eq!(PeerId::from_str("nostr_AABB").bare(), "aabb");
        assert_eq!(PeerId::from_str("AABBCCDDEEFF0011").as_str(), "aabbccddeeff0011");
    }

    #[test]
    fn type_predicates_distinguish_the_two_nostr_prefixes() {
        assert!(PeerId::from_str("nostr:abcdef").is_geo_chat());
        assert!(!PeerId::from_str("nostr_abcdef").is_geo_chat());
        assert!(PeerId::from_str("nostr_abcdef").is_geo_dm());
        assert!(!PeerId::from_str("nostr:abcdef").is_geo_dm());
        assert!(PeerId::from_str("group_abcdef").is_group());
        assert!(!PeerId::from_str("nostr_abcdef").is_group());
        assert!(PeerId::from_str("bridge:abcdef").is_bridged());
        assert!(!PeerId::from_str("mesh:abcdef").is_bridged());
    }

    #[test]
    fn from_bytes_hex_encodes() {
        let peer = PeerId::from_bytes(&[0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77]);
        assert_eq!(peer.as_str(), HEX16);
        assert_eq!(peer.to_routing_bytes(), Some([0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77]));
    }

    #[test]
    fn from_noise_key_is_sha256_not_truncation() {
        // Expected values are SHA-256 of the key, first 8 bytes as hex — the same
        // derivation `PeerIDTests.fileTransfer_*` asserts.
        assert_eq!(PeerId::from_noise_key(&[0xAB; 32]).as_str(), "9a2db2e23f1504cd");
        assert_eq!(PeerId::from_noise_key(&[0xCD; 32]).as_str(), "7969ec0fcb8b648d");
        // The old bug: truncating the raw key instead of hashing it.
        assert_ne!(PeerId::from_noise_key(&[0xEF; 32]), PeerId::from_bytes(&[0xEF; 8]));
    }

    #[test]
    fn from_noise_key_accepts_arbitrary_length_input() {
        // `PeerIDTests.publicKey_derivesFingerprint` hashes the *UTF-8 of* a
        // 64-character hex string, i.e. 64 bytes, not the 32 bytes it encodes.
        assert_eq!(PeerId::from_noise_key(HEX64.as_bytes()).as_str(), "2a8abfa8cb990629");
    }

    #[test]
    fn to_short_reduces_a_noise_key_and_leaves_everything_else() {
        let expected = "4773d12e2371bb93"; // SHA-256 of the 32 bytes HEX64 encodes.
        assert_eq!(PeerId::from_str(HEX64).to_short().as_str(), expected);
        let prefixed = PeerId::from_str(&format!("noise:{HEX64}"));
        assert_eq!(prefixed.to_short().as_str(), expected);
        assert_eq!(prefixed.prefix(), Some(PREFIX_NOISE));

        let opaque = PeerId::from_str("some_random_key");
        assert_eq!(opaque.to_short(), opaque);
    }

    #[test]
    fn routing_bytes_prefer_the_direct_hex_form() {
        let short = PeerId::from_str(HEX16);
        assert_eq!(
            short.to_routing_bytes(),
            hex_decode(HEX16).as_deref().and_then(to_eight)
        );

        // A 64-hex ID has no 8-byte reading, so it falls through to the short form.
        let long = PeerId::from_str(HEX64);
        let expected = hex_decode(long.to_short().as_str())
            .as_deref()
            .and_then(to_eight);
        assert_eq!(long.to_routing_bytes(), expected);
        assert!(expected.is_some());

        // A prefixed short ID: the whole string is not hex, the bare part is.
        let prefixed = PeerId::from_str(&format!("mesh:{HEX16}"));
        assert_eq!(prefixed.to_routing_bytes(), short.to_routing_bytes());
    }

    #[test]
    fn routing_bytes_are_none_when_no_eight_byte_form_exists() {
        assert_eq!(PeerId::from_str("name:alice").to_routing_bytes(), None);
        assert_eq!(PeerId::from_str("").to_routing_bytes(), None);
        // 4 hex characters decode to 2 bytes, which is not a routing ID.
        assert_eq!(PeerId::from_str("abcd").to_routing_bytes(), None);
    }

    #[test]
    fn routing_round_trip() {
        let raw = [0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77];
        assert_eq!(PeerId::from_bytes(&raw).to_routing_bytes(), Some(raw));
    }

    #[test]
    fn validation_accepts_short_hex() {
        assert!(PeerId::from_str("0011223344556677").is_valid());
        assert!(PeerId::from_str("aabbccddeeff0011").is_valid());
    }

    #[test]
    fn validation_accepts_full_noise_key_hex() {
        assert!(PeerId::from_str(&"ab".repeat(32)).is_valid());
    }

    #[test]
    fn validation_accepts_internal_alnum_dash_underscore() {
        assert!(PeerId::from_str("peer_123-ABC").is_valid());
        assert!(PeerId::from_str("nostr_user_01").is_valid());
    }

    #[test]
    fn validation_rejects_bad_characters_and_lengths() {
        assert!(!PeerId::from_str("peer!@#").is_valid());
        // Right length for a short ID but not hex.
        assert!(!PeerId::from_str("gggggggggggggggg").is_valid());
        assert!(!PeerId::from_str(&"a".repeat(65)).is_valid());
        assert!(!PeerId::from_str("").is_valid());
    }

    #[test]
    fn validation_peels_prefixes_recursively() {
        // Transcribed from `PeerIDTests.prefixes`.
        let hex64 = "a".repeat(64);
        let hex63 = "a".repeat(63);
        let hex16 = "a".repeat(16);
        let hex8 = "a".repeat(8);
        for body in [&hex64, &hex63, &hex16, &hex8] {
            for prefix in [PREFIX_NOISE, PREFIX_GEO_CHAT, PREFIX_GEO_DM] {
                assert!(PeerId::from_str(&format!("{prefix}{body}")).is_valid(), "{prefix}{body}");
            }
        }

        assert!(PeerId::from_str("name:mesh:abcdefg").is_valid());
        assert!(PeerId::from_str("name:name:some_name").is_valid());
        // The inner bare part still contains a colon, which is not in the charset.
        assert!(!PeerId::from_str("name:name:bad:name").is_valid());

        let hex65 = "a".repeat(65);
        for prefix in [PREFIX_NOISE, PREFIX_GEO_CHAT, PREFIX_GEO_DM] {
            assert!(!PeerId::from_str(&format!("{prefix}{hex65}")).is_valid());
        }
    }

    #[test]
    fn validation_terminates_on_pathological_stacked_prefixes() {
        // The reason this is a loop and not recursion.
        let id = PeerId::from_str(&"name:".repeat(50_000));
        assert!(!id.is_valid());
    }

    #[test]
    fn short_and_noise_key_predicates() {
        assert!(PeerId::from_str(HEX16).is_short());
        assert!(!PeerId::from_str("abcd").is_short());

        let hex64 = "ab".repeat(32);
        assert!(PeerId::from_str(&hex64).is_noise_key_hex());
        assert!(PeerId::from_str(&format!("noise:{hex64}")).is_noise_key_hex());
        assert_eq!(PeerId::from_str(&hex64).noise_key(), Some(vec![0xAB; 32]));

        let bad = "z".repeat(64);
        assert!(!PeerId::from_str(&bad).is_noise_key_hex());
        assert_eq!(PeerId::from_str(&bad).noise_key(), None);
    }

    #[test]
    fn group_id_decodes_only_for_well_formed_group_ids() {
        let group = PeerId::from_str(&format!("group_{}", "11".repeat(16)));
        assert!(group.is_group());
        assert_eq!(group.group_id(), Some([0x11; 16]));

        assert_eq!(PeerId::from_str("group_1234").group_id(), None);
        assert_eq!(PeerId::from_str(&"11".repeat(16)).group_id(), None);
    }

    #[test]
    fn ordering_is_over_the_full_id() {
        let mut ids = [PeerId::from_str("bbb"), PeerId::from_str("aaa")];
        ids.sort();
        assert_eq!(ids[0].as_str(), "aaa");
        assert_eq!(PeerId::from_str("BBB"), PeerId::from_str("bbb"));
    }

    #[test]
    fn hex_decode_matches_data_hex_string_semantics() {
        assert_eq!(hex_decode("00ff"), Some(vec![0x00, 0xFF]));
        assert_eq!(hex_decode("0x00ff"), Some(vec![0x00, 0xFF]));
        assert_eq!(hex_decode("0X00FF"), Some(vec![0x00, 0xFF]));
        assert_eq!(hex_decode("  00ff \n"), Some(vec![0x00, 0xFF]));
        assert_eq!(hex_decode(""), Some(Vec::new()));
        assert_eq!(hex_decode("abc"), None); // odd length
        assert_eq!(hex_decode("zz"), None); // not hex
        assert_eq!(hex_decode("çç"), None); // multi-byte characters never decode
    }

    #[test]
    fn hex_encode_is_lowercase_and_zero_padded() {
        assert_eq!(hex_encode([0x00u8, 0x0F, 0xAB, 0xFF].iter()), "000fabff");
        assert_eq!(hex_encode([0u8; 0].iter()), "");
    }
}

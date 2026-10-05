//! Event identity: the NIP-01 ID hash, BIP-340 Schnorr signing/verification, and
//! the JSON encoding that both of those depend on.
//!
//! # Why the serialization is the load-bearing part
//!
//! An event's ID is `SHA-256` over the *compact JSON* of
//! `[0, pubkey, created_at, kind, tags, content]`, and the signature is over that
//! hash. So a single byte of difference in how a string is escaped changes the ID,
//! which invalidates every signature we produce and rejects every signature bitchat
//! produces. There is no partial credit here.
//!
//! bitchat builds that array with Foundation's `JSONSerialization` and the
//! `.withoutEscapingSlashes` option. That means, exactly:
//!
//! - no whitespace anywhere — no space after `:` or `,`;
//! - `"` → `\"` and `\` → `\\`;
//! - `\u{08} \u{09} \u{0a} \u{0c} \u{0d}` use the short forms `\b \t \n \f \r`;
//! - every other codepoint below `\u{20}` uses `\u00xx` with **lowercase** hex;
//! - `/` is **not** escaped (that is what the option means);
//! - `\u{7f}` (DEL) is not escaped;
//! - non-ASCII is emitted as raw UTF-8, never `\uXXXX` — NIP-01 requires this;
//! - `created_at` and `kind` are bare integers.
//!
//! `serde_json`'s compact `Formatter` matches that list byte for byte: its escape
//! table escapes only `"`, `\`, and `0x00..=0x1f`, with the same five short forms
//! and the same lowercase `\u00xx` fallback, and it never escapes `/` or non-ASCII.
//! That was checked against the crate's `ESCAPE` table and `write_char_escape`, and
//! it is pinned from below by the vectors in this module's tests — so this file uses
//! `serde_json` rather than a hand-rolled writer. If a future `serde_json` ever
//! diverges, `escaping_matches_foundation_json_serialization` fails loudly.

use secp256k1::schnorr::Signature;
use secp256k1::{Keypair, SECP256K1, SecretKey, XOnlyPublicKey};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::error::NostrError;
use crate::types::NostrEvent;

/// The NIP-01 ID preimage: `[0, pubkey, created_at, kind, tags, content]`, compact.
///
/// Kept separate from [`event_id`] so the tests can pin the exact bytes rather than
/// only the hash of them — when this drifts, the failure should name the escaping,
/// not just an opaque digest mismatch.
fn serialize_for_id(event: &NostrEvent) -> Result<String, NostrError> {
    serde_json::to_string(&(
        0u8,
        &event.pubkey,
        event.created_at,
        event.kind,
        &event.tags,
        &event.content,
    ))
    .map_err(|e| NostrError::Json(e.to_string()))
}

/// `SHA-256` of the ID preimage.
///
/// Note this hashes the event's *current* fields; it does not read `event.id`. That
/// is the whole point — [`verify_event`] recomputes it and compares.
pub fn event_id(event: &NostrEvent) -> Result<[u8; 32], NostrError> {
    let serialized = serialize_for_id(event)?;
    let digest = Sha256::digest(serialized.as_bytes());
    let mut id = [0u8; 32];
    id.copy_from_slice(digest.as_slice());
    Ok(id)
}

/// [`event_id`] as lowercase hex — the form that travels in the `id` field.
pub fn event_id_hex(event: &NostrEvent) -> Result<String, NostrError> {
    Ok(hex::encode(event_id(event)?))
}

/// Compute the ID, sign it with BIP-340 Schnorr, and return the completed event.
///
/// The signature covers the ID hash, and the ID covers `pubkey` — so signing with a
/// key that does not match `event.pubkey` yields an event that can never verify.
/// bitchat's `sign(with:)` happily does that; we don't. An empty `pubkey` is filled
/// in from the secret key, and a populated one that disagrees is an error rather
/// than a silently dead event. For every call site that pairs the key with its own
/// pubkey — which is all of them — the output is byte-identical to bitchat's.
///
/// bitchat signs with 32 bytes of fresh randomness as BIP-340 aux, so signatures are
/// **not** deterministic. Two signings of the same event produce different `sig`
/// values and both verify. Never assert on signature bytes.
pub fn sign_event(event: NostrEvent, secret_key: &[u8; 32]) -> Result<NostrEvent, NostrError> {
    let secret =
        SecretKey::from_byte_array(secret_key).map_err(|_| NostrError::InvalidPrivateKey)?;
    let keypair = Keypair::from_secret_key(SECP256K1, &secret);
    let (xonly, _parity) = keypair.x_only_public_key();
    let pubkey_hex = hex::encode(xonly.serialize());

    let mut event = event;
    if event.pubkey.is_empty() {
        event.pubkey = pubkey_hex;
    } else if event.pubkey != pubkey_hex {
        return Err(NostrError::InvalidPublicKey);
    }

    let id = event_id(&event)?;
    let signature = SECP256K1.sign_schnorr(&id, &keypair);

    event.id = hex::encode(id);
    event.sig = Some(hex::encode(signature.to_byte_array()));
    Ok(event)
}

/// Does this event's `sig` verify against its `pubkey`, over an ID that actually
/// matches its contents?
///
/// Every failure mode collapses to `false`: no signature, non-hex or wrong-length
/// signature or key, a key that is not a valid curve point, an `id` that disagrees
/// with the fields, or a signature that simply does not verify. This is the function
/// that meets input from strangers, so it never returns an error and never panics.
///
/// The `id` comparison is byte-exact against lowercase hex, matching bitchat. An
/// event whose `id` is spelled in uppercase is rejected, exactly as bitchat rejects
/// it — the ID is a hash digest, not a case-insensitive identifier.
pub fn verify_event(event: &NostrEvent) -> bool {
    let Some(sig_hex) = event.sig.as_deref() else {
        return false;
    };
    let Ok(sig_bytes) = hex::decode(sig_hex) else {
        return false;
    };
    let Ok(sig_bytes) = <[u8; 64]>::try_from(sig_bytes.as_slice()) else {
        return false;
    };
    let Ok(pubkey_bytes) = hex::decode(&event.pubkey) else {
        return false;
    };
    let Ok(pubkey_bytes) = <[u8; 32]>::try_from(pubkey_bytes.as_slice()) else {
        return false;
    };
    let Ok(xonly) = XOnlyPublicKey::from_byte_array(&pubkey_bytes) else {
        return false;
    };
    let Ok(id) = event_id(event) else {
        return false;
    };
    if hex::encode(id) != event.id {
        return false;
    }

    let signature = Signature::from_byte_array(sig_bytes);
    SECP256K1.verify_schnorr(&signature, &id, &xonly).is_ok()
}

/// The wire form of a whole event: `{"id":…,"pubkey":…,"created_at":…,"kind":…,
/// "tags":…,"content":…,"sig":…}`, with `sig` omitted when absent.
///
/// Field order follows the struct declaration, matching the order Swift's
/// `JSONEncoder` produces for bitchat's `Codable` `NostrEvent`. Relays do not care
/// about key order — this is only for readability when diffing against bitchat.
pub fn to_json(event: &NostrEvent) -> Result<String, NostrError> {
    serde_json::to_string(event).map_err(|e| NostrError::Json(e.to_string()))
}

/// Parse an event from a JSON object.
///
/// Deliberately lenient in the two places bitchat's `init?(from dict:)` is lenient:
/// a missing `id` becomes `""` and a missing `sig` becomes `None`, so an unsigned
/// rumor parses. Everything else is required and strictly typed — a tag that is not
/// an array of strings rejects the whole event rather than being silently dropped.
///
/// Parsing never authenticates anything. Call [`verify_event`] on the result before
/// trusting `pubkey`.
pub fn from_json(json: &str) -> Result<NostrEvent, NostrError> {
    let value: Value = serde_json::from_str(json).map_err(|e| NostrError::Json(e.to_string()))?;
    from_value(&value)
}

/// [`from_json`] for a value that has already been parsed — relay frames arrive as
/// `["EVENT", <sub_id>, {…}]`, so the object is in hand before the event is.
pub fn from_value(value: &Value) -> Result<NostrEvent, NostrError> {
    let pubkey = value
        .get("pubkey")
        .and_then(Value::as_str)
        .ok_or(NostrError::InvalidEvent("missing or non-string pubkey"))?;

    let created_at =
        value
            .get("created_at")
            .and_then(Value::as_i64)
            .ok_or(NostrError::InvalidEvent(
                "missing or non-integer created_at",
            ))?;

    let kind = value
        .get("kind")
        .and_then(Value::as_i64)
        .ok_or(NostrError::InvalidEvent("missing or non-integer kind"))?;
    let kind = u16::try_from(kind).map_err(|_| NostrError::InvalidEvent("kind out of range"))?;

    let tag_values = value
        .get("tags")
        .and_then(Value::as_array)
        .ok_or(NostrError::InvalidEvent("missing or non-array tags"))?;
    let mut tags = Vec::with_capacity(tag_values.len());
    for tag in tag_values {
        let entries = tag
            .as_array()
            .ok_or(NostrError::InvalidEvent("tag is not an array"))?;
        let mut values = Vec::with_capacity(entries.len());
        for entry in entries {
            let entry = entry
                .as_str()
                .ok_or(NostrError::InvalidEvent("tag entry is not a string"))?;
            values.push(entry.to_owned());
        }
        tags.push(values);
    }

    let content = value
        .get("content")
        .and_then(Value::as_str)
        .ok_or(NostrError::InvalidEvent("missing or non-string content"))?;

    Ok(NostrEvent {
        id: value
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        pubkey: pubkey.to_owned(),
        created_at,
        kind,
        tags,
        content: content.to_owned(),
        sig: value
            .get("sig")
            .and_then(Value::as_str)
            .map(|sig| sig.to_owned()),
    })
}

/// The x-only (BIP-340) public key for a secret key, as lowercase hex.
///
/// x-only means the 32-byte X coordinate with the Y parity dropped — a Nostr
/// `pubkey` is never the 33-byte compressed form.
pub fn public_key_hex(secret_key: &[u8; 32]) -> Result<String, NostrError> {
    let secret =
        SecretKey::from_byte_array(secret_key).map_err(|_| NostrError::InvalidPrivateKey)?;
    let keypair = Keypair::from_secret_key(SECP256K1, &secret);
    let (xonly, _parity) = keypair.x_only_public_key();
    Ok(hex::encode(xonly.serialize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Golden events generated with the BIP-340 reference implementation from the
    /// BIP text (aux_rand = 32 zero bytes, so these signatures are reproducible)
    /// over a serializer written by hand from the `JSONSerialization` rules and
    /// cross-checked against a second independent JSON encoder.
    ///
    /// They are *not* lifted from `NostrProtocolTests.swift` — that file contains no
    /// literal events at all. Every test there generates a fresh `NostrIdentity` and
    /// asserts on round-trips, so it pins behaviour but not bytes. These vectors
    /// exist to pin the bytes.
    struct Golden {
        secret: &'static str,
        pubkey: &'static str,
        created_at: i64,
        kind: u16,
        tags: &'static [&'static [&'static str]],
        content: &'static str,
        serialized: &'static str,
        id: &'static str,
        sig: &'static str,
    }

    const GOLDEN: &[Golden] = &[
        // A geohash chat message: the common case, kind 20000 with g/n/t tags.
        Golden {
            secret: "0000000000000000000000000000000000000000000000000000000000000003",
            pubkey: "f9308a019258c31049344f85f89d5229b531c845836f99b08601f113bce036f9",
            created_at: 1735689600,
            kind: 20000,
            tags: &[&["g", "u4pruyd"], &["n", "alice"], &["t", "teleport"]],
            content: "hello from the mesh",
            serialized: "[0,\"f9308a019258c31049344f85f89d5229b531c845836f99b08601f113bce036f9\",1735689600,20000,[[\"g\",\"u4pruyd\"],[\"n\",\"alice\"],[\"t\",\"teleport\"]],\"hello from the mesh\"]",
            id: "8e1445c6e94474d96125e993b16c34768dda548c30d53ae887035b35e30d6a1a",
            sig: "35e0d78bb76cfec6abf7ace18e15a8d35fb2c123858910725b61d1da136bf9e7acfc796bd7415b4e77db73088df1b9a1d74be800eeeff137ae65d43b99accd01",
        },
        // A presence heartbeat: empty content, single tag. Pins the `""` and the
        // single-element tag array.
        Golden {
            secret: "b7e151628aed2a6abf7158809cf4f3c762e7160f38b4da56a784d9045190cfef",
            pubkey: "dff1d77f2a671c5f36183726db2341be58feae1da2deced843240f7b502ba659",
            created_at: 1700000000,
            kind: 20001,
            tags: &[&["g", "u4pr"]],
            content: "",
            serialized: "[0,\"dff1d77f2a671c5f36183726db2341be58feae1da2deced843240f7b502ba659\",1700000000,20001,[[\"g\",\"u4pr\"]],\"\"]",
            id: "e2976d14c7351d859345a3d84f6803622ce5e8d423cb5a1e08033209f89b9909",
            sig: "bb21fa394ce71048775be28b35a810893c4240e7e02368bbf616721707178e1b841f8ba4ecf108c926efc8d6b49f841577ed7284cb89ecee81bb6a248d6c862a",
        },
        // A location note whose content mixes accents, symbols, an emoji, Hebrew, a
        // forward slash, a backslash, a quote and a tab — one event that would break
        // on any of the escaping rules at once.
        Golden {
            secret: "c90fdaa22168c234c4c6628b80dc1cd129024e088a67cc74020bbea63b14e5c9",
            pubkey: "dd308afec5777e13121fa72b9cc1b7cc0139715309b086c960e18fd969774eb8",
            created_at: 1234567890,
            kind: 1,
            tags: &[&["g", "9q8yy"]],
            content: "caf\u{e9} \u{2615} \u{1f389} \u{5e9}\u{5dc}\u{5d5}\u{5dd} \u{5e2}\u{5d5}\u{5dc}\u{5dd} / a\\b \"quoted\" \tend",
            serialized: "[0,\"dd308afec5777e13121fa72b9cc1b7cc0139715309b086c960e18fd969774eb8\",1234567890,1,[[\"g\",\"9q8yy\"]],\"caf\u{e9} \u{2615} \u{1f389} \u{5e9}\u{5dc}\u{5d5}\u{5dd} \u{5e2}\u{5d5}\u{5dc}\u{5dd} / a\\\\b \\\"quoted\\\" \\tend\"]",
            id: "090a33ad991efe6ef7477df8b735207435f2157eff4d45747c0fce7782cea52f",
            sig: "d40470cec82d6913640752677dc2105538bb1d4ce54215edde8784c18059f3e817eea3e4399649368e13e1bec0fe0da1c1af6b4951f67de1de7d2a1ae8a79058",
        },
        // A gift wrap: kind 1059, `p` tag, `v2:` ciphertext content. Pins that the
        // colon in the `v2:` prefix is not special.
        Golden {
            secret: "0000000000000000000000000000000000000000000000000000000000000001",
            pubkey: "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798",
            created_at: 1712345678,
            kind: 1059,
            tags: &[&[
                "p",
                "3bf0c63fcb93463407af97a5e5ee64fa883d107ef9e558472c4eb9aaaefa459d",
            ]],
            content: "v2:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            serialized: "[0,\"79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798\",1712345678,1059,[[\"p\",\"3bf0c63fcb93463407af97a5e5ee64fa883d107ef9e558472c4eb9aaaefa459d\"]],\"v2:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\"]",
            id: "2d01ff7ba847d92ad877094b68e91ad44ed06b50c3b6a4b476468cdd3059e3a5",
            sig: "f9fc64cc45bfca81a8df93fadca5926311cbca6309500f6a57ec999799c3277016eeaeaa0b43ef82c60815ab4d521bfaa0ddfc99bfd471dc398034755ab1ccee",
        },
    ];

    /// `(name, content, expected serialization, expected id)` with `pubkey` = 64
    /// zeros, `created_at` = 0, `kind` = 1, `tags` = `[]`, so only the content
    /// escaping varies.
    ///
    /// The all-zero pubkey is not a valid curve point; that is fine and deliberate.
    /// ID computation must not validate it — bitchat's does not, and an event with a
    /// junk pubkey has to hash to something before it can be rejected.
    const ESCAPING: &[(&str, &str, &str, &str)] = &[
        (
            "quote",
            "say \"hi\"",
            "[0,\"0000000000000000000000000000000000000000000000000000000000000000\",0,1,[],\"say \\\"hi\\\"\"]",
            "786c223f8133574027627f9b486a393231e0f463e423c268d2b0672fc610b41a",
        ),
        (
            "backslash",
            "a\\b",
            "[0,\"0000000000000000000000000000000000000000000000000000000000000000\",0,1,[],\"a\\\\b\"]",
            "29c80051531c33eefcb02a6f02087f6af9436cf461c4d991a7075b1a74c9eccb",
        ),
        (
            "slash",
            "https://example.com/path",
            "[0,\"0000000000000000000000000000000000000000000000000000000000000000\",0,1,[],\"https://example.com/path\"]",
            "7d20e143e6355298b9c82ce524789a9f347df74dcd6e8d3e9e1f08207d93dc39",
        ),
        (
            "newline",
            "line1\nline2",
            "[0,\"0000000000000000000000000000000000000000000000000000000000000000\",0,1,[],\"line1\\nline2\"]",
            "bd074562120d45321d3f4bd2a566a7315d5616d54d8422ec72a1b831e087daf4",
        ),
        (
            "tab",
            "a\tb",
            "[0,\"0000000000000000000000000000000000000000000000000000000000000000\",0,1,[],\"a\\tb\"]",
            "a298a932065d20f716d5f81aa7c24f6be918afb57c8b4eedafb85f3e74594c85",
        ),
        (
            "carriage_return",
            "a\rb",
            "[0,\"0000000000000000000000000000000000000000000000000000000000000000\",0,1,[],\"a\\rb\"]",
            "b1fe842d3d28fb315ce194fb7afc0f7f75becb259dce61e7039d80faf95dc326",
        ),
        (
            "backspace",
            "a\u{8}b",
            "[0,\"0000000000000000000000000000000000000000000000000000000000000000\",0,1,[],\"a\\bb\"]",
            "bd9054f5ec9c5640159c0c69e2caa7534d5bf72c0696e49b257be6c14a5bcd91",
        ),
        (
            "form_feed",
            "a\u{c}b",
            "[0,\"0000000000000000000000000000000000000000000000000000000000000000\",0,1,[],\"a\\fb\"]",
            "6a86e6dfc0a5b98ba437b0e32b75b8145146469a7655493a86c6233be5779c05",
        ),
        (
            "nul",
            "a\u{0}b",
            "[0,\"0000000000000000000000000000000000000000000000000000000000000000\",0,1,[],\"a\\u0000b\"]",
            "1d60f8f83dbfad5e321aa29303d77b3a5ca8f92725a0912148185b8218905a6d",
        ),
        (
            "control_0x01",
            "a\u{1}b",
            "[0,\"0000000000000000000000000000000000000000000000000000000000000000\",0,1,[],\"a\\u0001b\"]",
            "ea18e9f81f9532e128213791e41b686d85480f537d404353c0910084f2c6ff34",
        ),
        (
            "control_0x1f",
            "a\u{1f}b",
            "[0,\"0000000000000000000000000000000000000000000000000000000000000000\",0,1,[],\"a\\u001fb\"]",
            "3dacac6bae1fd16594ab8bdc174de3cfea39d6f1f7f60fb659f4750990422cc0",
        ),
        (
            "del_0x7f_is_not_escaped",
            "a\u{7f}b",
            "[0,\"0000000000000000000000000000000000000000000000000000000000000000\",0,1,[],\"a\u{7f}b\"]",
            "00b23738be6449cfbd746eeabeabad37e48c5fa66f986321a1d382fa6dd00a50",
        ),
        (
            "emoji",
            "gm \u{1f30a}\u{1f41f}",
            "[0,\"0000000000000000000000000000000000000000000000000000000000000000\",0,1,[],\"gm \u{1f30a}\u{1f41f}\"]",
            "fde07b2c968bcaf7a9794667b53775aa810c5c65ce31533d22451e3ee5653977",
        ),
        (
            "accented",
            "caf\u{e9} na\u{ef}ve",
            "[0,\"0000000000000000000000000000000000000000000000000000000000000000\",0,1,[],\"caf\u{e9} na\u{ef}ve\"]",
            "8c6662ce19cdeb569694e62a1e254ab2b46e946f6c4953bbd088ef2bc1c5ba54",
        ),
        (
            "rtl_override",
            "a\u{202e}RTL\u{202c}b",
            "[0,\"0000000000000000000000000000000000000000000000000000000000000000\",0,1,[],\"a\u{202e}RTL\u{202c}b\"]",
            "c589432e0dfd7a0c23ac269f4dc6c21d9a2181ce628940164bb7716a05d5a954",
        ),
        (
            "rtl_mark",
            "a\u{200f}b",
            "[0,\"0000000000000000000000000000000000000000000000000000000000000000\",0,1,[],\"a\u{200f}b\"]",
            "d13b446e8bbae7494e213a1c5337855ed265869ec894b624bc206932ad79ae15",
        ),
        (
            "hebrew",
            "\u{5e9}\u{5dc}\u{5d5}\u{5dd}",
            "[0,\"0000000000000000000000000000000000000000000000000000000000000000\",0,1,[],\"\u{5e9}\u{5dc}\u{5d5}\u{5dd}\"]",
            "4692b78e22c37301e04c714d28d7bfea31f72a4bffa292b536bc351780dbfd09",
        ),
        (
            "arabic",
            "\u{645}\u{631}\u{62d}\u{628}\u{627}",
            "[0,\"0000000000000000000000000000000000000000000000000000000000000000\",0,1,[],\"\u{645}\u{631}\u{62d}\u{628}\u{627}\"]",
            "bd15c1a1ce707ceef5b2383f3045abf99a23c7a19132ed89730ec8c5262d2985",
        ),
        (
            "line_and_paragraph_separator",
            "a\u{2028}b\u{2029}c",
            "[0,\"0000000000000000000000000000000000000000000000000000000000000000\",0,1,[],\"a\u{2028}b\u{2029}c\"]",
            "349c04ba6cfff3d30d5ee2f6bfac395aaeed53bb03d4b7366330d67428a7f8e2",
        ),
        (
            "all_of_it_at_once",
            "\"\\/\n\t\u{0}\r\u{8}\u{c} \u{1f389} \u{645}\u{631}\u{62d}\u{628}\u{627}",
            "[0,\"0000000000000000000000000000000000000000000000000000000000000000\",0,1,[],\"\\\"\\\\/\\n\\t\\u0000\\r\\b\\f \u{1f389} \u{645}\u{631}\u{62d}\u{628}\u{627}\"]",
            "08bde01f166b84326a3d9d41abc65e6b18f34c0763d45ffef2066dfa5e18afcc",
        ),
    ];

    const ZERO_PUBKEY: &str = "0000000000000000000000000000000000000000000000000000000000000000";

    /// Tags go through the same escaper as content, including empty tags and tags
    /// with more than two entries.
    const TAG_SER: &str = "[0,\"0000000000000000000000000000000000000000000000000000000000000000\",7,20000,[[],[\"g\"],[\"n\",\"a\\\"b\"],[\"x\",\"y/z\",\"\\n\"]],\"\"]";
    const TAG_ID: &str = "abaff7dd12491146ec4164ce9931019757d52301c61970d53bb89015765cc360";

    fn secret_bytes(hex_str: &str) -> [u8; 32] {
        let bytes = hex::decode(hex_str).unwrap();
        <[u8; 32]>::try_from(bytes.as_slice()).unwrap()
    }

    fn owned_tags(tags: &[&[&str]]) -> Vec<Vec<String>> {
        tags.iter()
            .map(|tag| tag.iter().map(|v| (*v).to_string()).collect())
            .collect()
    }

    impl Golden {
        fn unsigned(&self) -> NostrEvent {
            NostrEvent {
                id: String::new(),
                pubkey: self.pubkey.to_string(),
                created_at: self.created_at,
                kind: self.kind,
                tags: owned_tags(self.tags),
                content: self.content.to_string(),
                sig: None,
            }
        }

        fn signed(&self) -> NostrEvent {
            let mut event = self.unsigned();
            event.id = self.id.to_string();
            event.sig = Some(self.sig.to_string());
            event
        }
    }

    fn content_event(content: &str) -> NostrEvent {
        NostrEvent {
            id: String::new(),
            pubkey: ZERO_PUBKEY.to_string(),
            created_at: 0,
            kind: 1,
            tags: Vec::new(),
            content: content.to_string(),
            sig: None,
        }
    }

    // ---- ID serialization -------------------------------------------------

    #[test]
    fn golden_events_serialize_and_hash_exactly() {
        for golden in GOLDEN {
            let event = golden.unsigned();
            assert_eq!(
                serialize_for_id(&event).unwrap(),
                golden.serialized,
                "ID preimage drifted for kind {}",
                golden.kind
            );
            assert_eq!(
                event_id_hex(&event).unwrap(),
                golden.id,
                "event id drifted for kind {}",
                golden.kind
            );
        }
    }

    #[test]
    fn escaping_matches_foundation_json_serialization() {
        for &(name, content, serialized, id) in ESCAPING {
            let event = content_event(content);
            assert_eq!(
                serialize_for_id(&event).unwrap(),
                serialized,
                "escaping drifted for case `{name}`"
            );
            assert_eq!(event_id_hex(&event).unwrap(), id, "id drifted for `{name}`");
        }
    }

    #[test]
    fn forward_slash_is_never_escaped() {
        // `.withoutEscapingSlashes`. If this ever fails, every URL-bearing message
        // we sign is unreadable by bitchat and vice versa.
        let event = content_event("a/b//c");
        assert!(serialize_for_id(&event).unwrap().contains("\"a/b//c\""));
        assert!(!serialize_for_id(&event).unwrap().contains("\\/"));
        assert!(!to_json(&event).unwrap().contains("\\/"));
    }

    #[test]
    fn non_ascii_is_raw_utf8_not_escaped() {
        let event = content_event("\u{1f30a} caf\u{e9}");
        let serialized = serialize_for_id(&event).unwrap();
        assert!(serialized.contains('\u{1f30a}'));
        assert!(!serialized.contains("\\u"));
        assert!(to_json(&event).unwrap().contains('\u{1f30a}'));
    }

    #[test]
    fn preimage_has_no_whitespace() {
        let golden = &GOLDEN[0];
        let serialized = serialize_for_id(&golden.unsigned()).unwrap();
        assert!(!serialized.contains(", "));
        assert!(!serialized.contains(": "));
        assert!(!serialized.contains('\n'));
    }

    #[test]
    fn created_at_and_kind_are_bare_integers() {
        let mut event = content_event("");
        event.created_at = -17;
        event.kind = 20001;
        let serialized = serialize_for_id(&event).unwrap();
        assert!(serialized.contains(",-17,20001,"), "{serialized}");
    }

    #[test]
    fn tags_use_the_same_escaper_as_content() {
        let mut event = content_event("");
        event.created_at = 7;
        event.kind = 20000;
        event.tags = vec![
            vec![],
            vec!["g".to_string()],
            vec!["n".to_string(), "a\"b".to_string()],
            vec!["x".to_string(), "y/z".to_string(), "\n".to_string()],
        ];
        assert_eq!(serialize_for_id(&event).unwrap(), TAG_SER);
        assert_eq!(event_id_hex(&event).unwrap(), TAG_ID);
    }

    #[test]
    fn event_id_ignores_the_id_and_sig_fields() {
        let golden = &GOLDEN[0];
        let mut event = golden.unsigned();
        let baseline = event_id_hex(&event).unwrap();
        event.id = "not a hash at all".to_string();
        event.sig = Some("ff".repeat(64));
        assert_eq!(event_id_hex(&event).unwrap(), baseline);
    }

    // ---- keys -------------------------------------------------------------

    #[test]
    fn golden_secret_keys_derive_their_x_only_pubkeys() {
        for golden in GOLDEN {
            assert_eq!(
                public_key_hex(&secret_bytes(golden.secret)).unwrap(),
                golden.pubkey
            );
        }
    }

    #[test]
    fn public_key_hex_is_thirty_two_bytes_of_lowercase_hex() {
        let hex_key = public_key_hex(&secret_bytes(GOLDEN[0].secret)).unwrap();
        assert_eq!(hex_key.len(), 64);
        assert!(
            hex_key
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
    }

    #[test]
    fn out_of_range_secret_keys_are_rejected() {
        assert_eq!(
            public_key_hex(&[0u8; 32]),
            Err(NostrError::InvalidPrivateKey)
        );
        // The group order itself is not a valid scalar.
        let order =
            secret_bytes("fffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364141");
        assert_eq!(public_key_hex(&order), Err(NostrError::InvalidPrivateKey));
        assert_eq!(
            public_key_hex(&[0xffu8; 32]),
            Err(NostrError::InvalidPrivateKey)
        );
    }

    // ---- signing ----------------------------------------------------------

    #[test]
    fn golden_signatures_verify() {
        for golden in GOLDEN {
            assert!(
                verify_event(&golden.signed()),
                "golden signature failed to verify for kind {}",
                golden.kind
            );
        }
    }

    #[test]
    fn resigning_a_golden_event_reproduces_its_id() {
        for golden in GOLDEN {
            let signed = sign_event(golden.unsigned(), &secret_bytes(golden.secret)).unwrap();
            assert_eq!(signed.id, golden.id);
            assert!(verify_event(&signed));
        }
    }

    #[test]
    fn signatures_are_not_deterministic_but_both_verify() {
        // bitchat signs with 32 bytes of fresh aux randomness. Anything asserting on
        // signature bytes would be asserting on a coin flip.
        let golden = &GOLDEN[0];
        let secret = secret_bytes(golden.secret);
        let first = sign_event(golden.unsigned(), &secret).unwrap();
        let second = sign_event(golden.unsigned(), &secret).unwrap();
        assert_eq!(first.id, second.id);
        assert_ne!(first.sig, second.sig);
        assert!(verify_event(&first));
        assert!(verify_event(&second));
    }

    #[test]
    fn signing_fills_in_an_empty_pubkey() {
        let golden = &GOLDEN[0];
        let mut event = golden.unsigned();
        event.pubkey = String::new();
        let signed = sign_event(event, &secret_bytes(golden.secret)).unwrap();
        assert_eq!(signed.pubkey, golden.pubkey);
        assert_eq!(signed.id, golden.id);
        assert!(verify_event(&signed));
    }

    #[test]
    fn signing_with_a_key_that_does_not_match_the_pubkey_is_rejected() {
        let event = GOLDEN[0].unsigned();
        let other = secret_bytes(GOLDEN[1].secret);
        assert_eq!(sign_event(event, &other), Err(NostrError::InvalidPublicKey));
    }

    #[test]
    fn signing_with_an_invalid_secret_key_is_rejected() {
        let event = GOLDEN[0].unsigned();
        assert_eq!(
            sign_event(event, &[0u8; 32]),
            Err(NostrError::InvalidPrivateKey)
        );
    }

    // ---- verification -----------------------------------------------------

    #[test]
    fn tampering_with_any_signed_field_breaks_verification() {
        let golden = &GOLDEN[0];
        let secret = secret_bytes(golden.secret);
        let signed = sign_event(golden.unsigned(), &secret).unwrap();
        assert!(verify_event(&signed));

        fn breaks(signed: &NostrEvent, name: &str, mutate: impl FnOnce(&mut NostrEvent)) {
            let mut tampered = signed.clone();
            mutate(&mut tampered);
            assert!(
                !verify_event(&tampered),
                "tampering with {name} still verified"
            );
        }

        breaks(&signed, "content appended", |e| e.content.push('!'));
        breaks(&signed, "content emptied", |e| e.content.clear());
        breaks(&signed, "kind", |e| e.kind = 20001);
        breaks(&signed, "created_at", |e| e.created_at += 1);
        breaks(&signed, "tag appended", |e| {
            e.tags.push(vec!["p".to_string(), "evil".to_string()]);
        });
        breaks(&signed, "tags cleared", |e| e.tags.clear());
        breaks(&signed, "tag value edited", |e| {
            e.tags[0][1] = "u4pruye".to_string();
        });
        breaks(&signed, "pubkey", |e| {
            e.pubkey = GOLDEN[1].pubkey.to_string();
        });
        breaks(&signed, "id", |e| e.id = "deadbeef".to_string());
        breaks(&signed, "id uppercased", |e| {
            let upper = e.id.to_uppercase();
            e.id = upper;
        });
    }

    #[test]
    fn tampering_that_also_recomputes_the_id_still_fails() {
        // The interesting attack: rewrite the content *and* the id so they agree.
        // Only the signature stops this.
        let golden = &GOLDEN[0];
        let mut forged = sign_event(golden.unsigned(), &secret_bytes(golden.secret)).unwrap();
        forged.content = "you owe me money".to_string();
        forged.id = event_id_hex(&forged).unwrap();
        assert!(!verify_event(&forged));
    }

    #[test]
    fn malformed_signatures_are_rejected_without_panicking() {
        let golden = &GOLDEN[0];
        let signed = golden.signed();

        let cases = [
            ("missing", None),
            ("empty", Some(String::new())),
            ("truncated", Some(golden.sig[..126].to_string())),
            ("odd length hex", Some(golden.sig[..127].to_string())),
            ("one byte", Some("ab".to_string())),
            ("too long", Some(format!("{}00", golden.sig))),
            ("non hex", Some("z".repeat(128))),
            ("unicode", Some("\u{1f30a}".repeat(32))),
            ("all zeros", Some("00".repeat(64))),
            ("all ones", Some("ff".repeat(64))),
            ("other event's sig", Some(GOLDEN[1].sig.to_string())),
        ];

        for (name, sig) in cases {
            let mut event = signed.clone();
            event.sig = sig;
            assert!(!verify_event(&event), "signature case `{name}` verified");
        }
    }

    #[test]
    fn malformed_pubkeys_are_rejected_without_panicking() {
        let golden = &GOLDEN[0];
        let signed = golden.signed();

        for (name, pubkey) in [
            ("empty", String::new()),
            ("truncated", golden.pubkey[..62].to_string()),
            ("odd length hex", golden.pubkey[..63].to_string()),
            ("non hex", "q".repeat(64)),
            ("too long", format!("{}00", golden.pubkey)),
            // Valid hex, valid length, but not an x coordinate on the curve.
            ("not on the curve", ZERO_PUBKEY.to_string()),
            ("above the field order", "f".repeat(64)),
            ("uppercased", golden.pubkey.to_uppercase()),
        ] {
            let mut event = signed.clone();
            event.pubkey = pubkey;
            assert!(!verify_event(&event), "pubkey case `{name}` verified");
        }
    }

    #[test]
    fn an_unsigned_event_never_verifies() {
        assert!(!verify_event(&GOLDEN[0].unsigned()));
    }

    // ---- JSON round trip --------------------------------------------------

    #[test]
    fn to_json_emits_the_expected_shape() {
        let golden = &GOLDEN[1];
        assert_eq!(
            to_json(&golden.signed()).unwrap(),
            format!(
                "{{\"id\":\"{}\",\"pubkey\":\"{}\",\"created_at\":{},\"kind\":{},\"tags\":[[\"g\",\"u4pr\"]],\"content\":\"\",\"sig\":\"{}\"}}",
                golden.id, golden.pubkey, golden.created_at, golden.kind, golden.sig
            )
        );
    }

    #[test]
    fn to_json_omits_a_missing_signature() {
        let json = to_json(&GOLDEN[0].unsigned()).unwrap();
        assert!(!json.contains("\"sig\""), "{json}");
    }

    #[test]
    fn json_round_trip_preserves_every_golden_event() {
        for golden in GOLDEN {
            let signed = golden.signed();
            let parsed = from_json(&to_json(&signed).unwrap()).unwrap();
            assert_eq!(parsed, signed);
            assert!(verify_event(&parsed));
        }
    }

    #[test]
    fn json_round_trip_survives_hostile_content() {
        for &(name, content, _, _) in ESCAPING {
            let event = content_event(content);
            let parsed = from_json(&to_json(&event).unwrap()).unwrap();
            assert_eq!(parsed, event, "round trip lost bytes for `{name}`");
            assert_eq!(
                event_id_hex(&parsed).unwrap(),
                event_id_hex(&event).unwrap()
            );
        }
    }

    #[test]
    fn from_json_defaults_a_missing_id_and_sig() {
        let parsed =
            from_json(r#"{"pubkey":"ab","created_at":1,"kind":14,"tags":[],"content":"rumor"}"#)
                .unwrap();
        assert_eq!(parsed.id, "");
        assert_eq!(parsed.sig, None);
        assert_eq!(parsed.kind, 14);
    }

    #[test]
    fn from_json_ignores_unknown_fields() {
        let parsed = from_json(
            r#"{"id":"aa","pubkey":"bb","created_at":1,"kind":1,"tags":[],"content":"","sig":"cc","relay_hint":"wss://x","ots":123}"#,
        )
        .unwrap();
        assert_eq!(parsed.sig.as_deref(), Some("cc"));
    }

    #[test]
    fn from_json_rejects_malformed_input_without_panicking() {
        let bad = [
            "",
            "   ",
            "null",
            "[]",
            "\"a string\"",
            "42",
            "{",
            "{\"pubkey\":}",
            // missing required fields
            r#"{}"#,
            r#"{"created_at":1,"kind":1,"tags":[],"content":""}"#,
            r#"{"pubkey":"ab","kind":1,"tags":[],"content":""}"#,
            r#"{"pubkey":"ab","created_at":1,"tags":[],"content":""}"#,
            r#"{"pubkey":"ab","created_at":1,"kind":1,"content":""}"#,
            r#"{"pubkey":"ab","created_at":1,"kind":1,"tags":[]}"#,
            // wrong types
            r#"{"pubkey":42,"created_at":1,"kind":1,"tags":[],"content":""}"#,
            r#"{"pubkey":"ab","created_at":"1","kind":1,"tags":[],"content":""}"#,
            r#"{"pubkey":"ab","created_at":1.5,"kind":1,"tags":[],"content":""}"#,
            r#"{"pubkey":"ab","created_at":1,"kind":"1","tags":[],"content":""}"#,
            r#"{"pubkey":"ab","created_at":1,"kind":1,"tags":{},"content":""}"#,
            r#"{"pubkey":"ab","created_at":1,"kind":1,"tags":["g"],"content":""}"#,
            r#"{"pubkey":"ab","created_at":1,"kind":1,"tags":[["g",7]],"content":""}"#,
            r#"{"pubkey":"ab","created_at":1,"kind":1,"tags":[[null]],"content":""}"#,
            r#"{"pubkey":"ab","created_at":1,"kind":1,"tags":[],"content":[]}"#,
            // kind outside the u16 range NIP-01 defines
            r#"{"pubkey":"ab","created_at":1,"kind":70000,"tags":[],"content":""}"#,
            r#"{"pubkey":"ab","created_at":1,"kind":-1,"tags":[],"content":""}"#,
        ];

        for json in bad {
            assert!(from_json(json).is_err(), "accepted malformed input: {json}");
        }
    }

    #[test]
    fn from_json_accepts_the_extremes_of_created_at() {
        for created_at in [i64::MIN, -1, 0, i64::MAX] {
            let json = format!(
                r#"{{"pubkey":"ab","created_at":{created_at},"kind":1,"tags":[],"content":""}}"#
            );
            assert_eq!(from_json(&json).unwrap().created_at, created_at);
        }
    }

    #[test]
    fn from_json_accepts_deeply_nested_and_oversized_tags() {
        // Relay input is attacker-controlled; long tags must parse, not explode.
        let tag: String = (0..64)
            .map(|i| format!("\"v{i}\""))
            .collect::<Vec<_>>()
            .join(",");
        let json =
            format!(r#"{{"pubkey":"ab","created_at":1,"kind":1,"tags":[[{tag}]],"content":""}}"#);
        let parsed = from_json(&json).unwrap();
        assert_eq!(parsed.tags.len(), 1);
        assert_eq!(parsed.tags[0].len(), 64);
    }

    #[test]
    fn from_value_matches_from_json() {
        let json = to_json(&GOLDEN[2].signed()).unwrap();
        let value: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(from_value(&value).unwrap(), from_json(&json).unwrap());
    }

    #[test]
    fn a_relay_delivered_event_survives_the_full_path() {
        // The path a real inbound event takes: JSON text from a socket, parsed,
        // verified, and the ID independently recomputed.
        for golden in GOLDEN {
            let wire = to_json(&golden.signed()).unwrap();
            let parsed = from_json(&wire).unwrap();
            assert!(verify_event(&parsed));
            assert_eq!(event_id_hex(&parsed).unwrap(), parsed.id);
            assert_eq!(to_json(&parsed).unwrap(), wire);
        }
    }
}

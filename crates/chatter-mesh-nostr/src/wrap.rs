//! NIP-17/59 gift wrapping: rumor (14) → seal (13) → gift wrap (1059).
//!
//! Ported from `bitchat/Nostr/NostrProtocol.swift`
//! (`createPrivateMessage`, `decryptPrivateMessage`, and the private
//! `createSeal` / `createGiftWrap` / `unwrapGiftWrap` / `openSeal` /
//! `randomizedTimestamp` helpers).
//!
//! # The three layers, and what each one hides
//!
//! ```text
//! rumor  kind 14   never signed     the message; pubkey names the author
//! seal   kind 13   signed by SENDER content = encrypt(rumor)     hides content from relays
//! wrap   kind 1059 signed by a      content = encrypt(seal)      hides the sender from relays
//!                  fresh throwaway  tagged ["p", recipient]
//! ```
//!
//! The rumor is **never signed**. A signed rumor is a self-contained, transferable
//! proof that the sender wrote those words; leaving it unsigned means the
//! recipient can authenticate the message but cannot prove authorship to anyone
//! else. The authentication instead comes from the seal, which the sender's real
//! key signs.
//!
//! The wrap is signed by a key generated fresh for that single wrap and then
//! thrown away. Reusing it across wraps would let a relay group every message one
//! user sends, which is the exact correlation the layer exists to prevent.
//!
//! # Reading the sender
//!
//! [`unwrap_gift`] reports the sender from the **seal**, never from the wrap and
//! never from the rumor:
//!
//! - the wrap's pubkey is a throwaway and says nothing about who sent it;
//! - the rumor's pubkey is unsigned plaintext inside the seal, so anyone who can
//!   write a seal can put any name in it.
//!
//! Only the seal's pubkey is bound to a signature. The seal's signature is
//! verified, and the rumor's claimed author is required to match the seal's
//! signer — otherwise a sender could seal a rumor attributed to somebody else and
//! the recipient would display it as theirs.
//!
//! Encryption here is bitchat's variant, not the published NIP-44 v2 — see the
//! crate docs. These DMs interoperate with bitchat and with nothing else.

use rand_core::CryptoRng;

use crate::crypto::{decrypt, encrypt};
use crate::error::NostrError;
use crate::event::{from_json, sign_event, to_json, verify_event};
use crate::identity::NostrIdentity;
use crate::types::{EventKind, NostrEvent};

/// Timestamps on the seal and the wrap are jittered by up to this many seconds
/// either side of the real time, matching bitchat's `randomizedTimestamp()`.
///
/// Relays see only the jittered value; the true send time travels inside the
/// encrypted rumor. Without it, a relay could correlate a wrap with a reply on
/// another relay purely by arrival time.
pub const TIMESTAMP_JITTER_SECS: i64 = 900;

/// How many times to draw random bytes looking for a valid throwaway secret.
///
/// A random 32-byte value falls outside the secp256k1 scalar range with
/// probability below 2⁻¹²⁷, so this loop realistically never turns twice.
const MAX_KEY_ATTEMPTS: usize = 16;

/// Wrap `rumor_content` for `recipient_pubkey_hex` as a NIP-17 gift wrap.
///
/// Returns a signed kind-1059 event, ready to publish. The `rng` must be
/// cryptographically secure: it produces both the throwaway wrap key and the
/// encryption nonces, and a predictable value for either breaks the whole
/// construction.
pub fn gift_wrap(
    rumor_content: &str,
    recipient_pubkey_hex: &str,
    sender: &NostrIdentity,
    created_at: i64,
    rng: &mut impl CryptoRng,
) -> Result<NostrEvent, NostrError> {
    let rumor = make_rumor(sender.public_key_hex(), rumor_content, created_at);
    let seal = make_seal(&rumor, recipient_pubkey_hex, sender, created_at, &mut *rng)?;
    make_wrap(&seal, recipient_pubkey_hex, created_at, rng)
}

/// Unwrap a gift wrap addressed to us.
///
/// Returns `(rumor content, authenticated sender pubkey hex, rumor created_at)`.
/// The sender is the seal's signer, and the seal's signature has been verified
/// before this returns.
///
/// The rumor's timestamp is the third element and matters: the seal and wrap
/// timestamps are deliberately jittered by ±15 minutes so relays cannot correlate
/// send times, which means they are the wrong thing to display. Only the rumor
/// carries the real send time. Dropping it would leave the UI showing a time that
/// is wrong by up to a quarter of an hour, in the one direction the jitter was
/// designed to make wrong.
pub fn unwrap_gift(
    wrap: &NostrEvent,
    recipient_secret: &[u8; 32],
) -> Result<(String, String, i64), NostrError> {
    if wrap.kind != EventKind::GiftWrap.as_u16() {
        return Err(NostrError::InvalidEvent("not a gift wrap"));
    }

    // Layer 1: decrypt the wrap to the seal. The wrap's pubkey is the throwaway
    // key, used here only as the ECDH counterpart — it identifies nobody.
    let seal_json = decrypt(&wrap.content, &wrap.pubkey, recipient_secret)?;
    let seal = from_json(&seal_json)?;

    if seal.kind != EventKind::Seal.as_u16() {
        return Err(NostrError::InvalidEvent("wrapped event is not a seal"));
    }

    // Authenticate before opening. Without this a DM is forgeable by anyone who
    // knows the recipient's npub: they could mint a seal claiming any sender.
    if !verify_event(&seal) {
        return Err(NostrError::InvalidSignature);
    }

    // Layer 2: decrypt the seal to the rumor.
    let rumor_json = decrypt(&seal.content, &seal.pubkey, recipient_secret)?;
    let rumor = from_json(&rumor_json)?;

    if rumor.kind != EventKind::DirectMessage.as_u16() {
        return Err(NostrError::InvalidEvent("sealed event is not a DM rumor"));
    }

    // The rumor's author is unsigned plaintext. It is only meaningful if it
    // agrees with the key that actually signed the seal.
    if rumor.pubkey != seal.pubkey {
        return Err(NostrError::InvalidEvent(
            "rumor author does not match the seal signer",
        ));
    }

    Ok((rumor.content, seal.pubkey, rumor.created_at))
}

/// The unsigned kind-14 rumor. Never signed, never published on its own.
fn make_rumor(sender_pubkey_hex: &str, content: &str, created_at: i64) -> NostrEvent {
    NostrEvent::unsigned(
        sender_pubkey_hex.to_string(),
        created_at,
        EventKind::DirectMessage,
        Vec::new(),
        content.to_string(),
    )
}

/// The kind-13 seal: the encrypted rumor, signed by the sender's real key.
fn make_seal<R: CryptoRng>(
    rumor: &NostrEvent,
    recipient_pubkey_hex: &str,
    sender: &NostrIdentity,
    created_at: i64,
    rng: &mut R,
) -> Result<NostrEvent, NostrError> {
    let rumor_json = to_json(rumor)?;
    let ciphertext = encrypt(
        &rumor_json,
        recipient_pubkey_hex,
        sender.secret(),
        &mut *rng,
    )?;

    let seal = NostrEvent::unsigned(
        sender.public_key_hex().to_string(),
        randomized_timestamp(created_at, rng),
        EventKind::Seal,
        Vec::new(),
        ciphertext,
    );
    sign_event(seal, sender.secret())
}

/// The kind-1059 gift wrap: the encrypted seal, signed by a fresh throwaway key.
fn make_wrap<R: CryptoRng>(
    seal: &NostrEvent,
    recipient_pubkey_hex: &str,
    created_at: i64,
    rng: &mut R,
) -> Result<NostrEvent, NostrError> {
    let seal_json = to_json(seal)?;
    let throwaway = throwaway_identity(&mut *rng)?;
    let ciphertext = encrypt(
        &seal_json,
        recipient_pubkey_hex,
        throwaway.secret(),
        &mut *rng,
    )?;

    let wrap = NostrEvent::unsigned(
        throwaway.public_key_hex().to_string(),
        randomized_timestamp(created_at, rng),
        EventKind::GiftWrap,
        vec![vec!["p".to_string(), recipient_pubkey_hex.to_string()]],
        ciphertext,
    );
    sign_event(wrap, throwaway.secret())
}

/// A single-use key for one wrap. Discarded the moment the wrap is signed.
fn throwaway_identity<R: CryptoRng>(rng: &mut R) -> Result<NostrIdentity, NostrError> {
    for _ in 0..MAX_KEY_ATTEMPTS {
        let mut secret = [0u8; 32];
        rng.fill_bytes(&mut secret);
        if let Ok(identity) = NostrIdentity::from_secret(secret) {
            return Ok(identity);
        }
    }
    Err(NostrError::InvalidPrivateKey)
}

/// `created_at` jittered by ±[`TIMESTAMP_JITTER_SECS`].
fn randomized_timestamp<R: CryptoRng>(created_at: i64, rng: &mut R) -> i64 {
    let span = (TIMESTAMP_JITTER_SECS as u64) * 2 + 1;
    let offset = (rng.next_u64() % span) as i64 - TIMESTAMP_JITTER_SECS;
    created_at.saturating_add(offset)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::derive_for_geohash;
    use rand_core::Rng;

    const NOW: i64 = 1_700_000_000;

    /// Deterministic CSPRNG stand-in so wrap tests are reproducible.
    ///
    /// Only ever used in tests — production callers pass a real CSPRNG.
    struct TestRng(u64);

    impl rand_core::TryRng for TestRng {
        type Error = core::convert::Infallible;
        fn try_next_u32(&mut self) -> Result<u32, Self::Error> {
            Ok({ (self.next_u64() >> 32) as u32 })
        }
        fn try_next_u64(&mut self) -> Result<u64, Self::Error> {
            Ok({
                // SplitMix64.
                self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
                let mut z = self.0;
                z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
                z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
                z ^ (z >> 31)
            })
        }
        fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), Self::Error> {
            for chunk in dest.chunks_mut(8) {
                let bytes = self.next_u64().to_le_bytes();
                let n = chunk.len();
                chunk.copy_from_slice(&bytes[..n]);
            }
            Ok(())
        }
    }

    impl rand_core::TryCryptoRng for TestRng {}

    fn rng(seed: u64) -> TestRng {
        TestRng(seed)
    }

    fn alice() -> NostrIdentity {
        derive_for_geohash(&[1u8; 32], "alice").expect("derives")
    }

    fn bob() -> NostrIdentity {
        derive_for_geohash(&[2u8; 32], "bob").expect("derives")
    }

    fn mallory() -> NostrIdentity {
        derive_for_geohash(&[3u8; 32], "mallory").expect("derives")
    }

    #[test]
    fn gift_wrap_round_trips() {
        let (alice, bob) = (alice(), bob());
        let wrap = gift_wrap(
            "meet me at the usual place",
            bob.public_key_hex(),
            &alice,
            NOW,
            &mut rng(1),
        )
        .expect("wraps");

        let (content, sender, rumor_at) = unwrap_gift(&wrap, bob.secret()).expect("unwraps");
        assert_eq!(content, "meet me at the usual place");
        assert_eq!(sender, alice.public_key_hex());
        // The rumor keeps the real send time; only the seal and wrap are jittered.
        assert_eq!(rumor_at, NOW);
        assert_ne!(wrap.created_at, rumor_at);
    }

    #[test]
    fn gift_wrap_round_trips_unicode_and_empty_content() {
        let (alice, bob) = (alice(), bob());
        let long = "a".repeat(4096);
        for message in ["", "héllo 🌍 / \\ \" \n\t", long.as_str()] {
            let wrap =
                gift_wrap(message, bob.public_key_hex(), &alice, NOW, &mut rng(2)).expect("wraps");
            let (content, _, _) = unwrap_gift(&wrap, bob.secret()).expect("unwraps");
            assert_eq!(content, message);
        }
    }

    #[test]
    fn the_wrap_is_kind_1059_signed_and_tagged_to_the_recipient() {
        let (alice, bob) = (alice(), bob());
        let wrap = gift_wrap("hi", bob.public_key_hex(), &alice, NOW, &mut rng(3)).expect("wraps");

        assert_eq!(wrap.kind, 1059);
        assert_eq!(
            wrap.tags,
            vec![vec!["p".to_string(), bob.public_key_hex().to_string()]]
        );
        assert!(wrap.sig.is_some());
        assert!(verify_event(&wrap));
    }

    #[test]
    fn the_wrap_pubkey_is_a_throwaway_belonging_to_neither_party() {
        let (alice, bob) = (alice(), bob());
        let wrap = gift_wrap("hi", bob.public_key_hex(), &alice, NOW, &mut rng(4)).expect("wraps");

        assert_ne!(wrap.pubkey, alice.public_key_hex());
        assert_ne!(wrap.pubkey, bob.public_key_hex());
    }

    #[test]
    fn every_wrap_uses_a_fresh_throwaway_key() {
        let (alice, bob) = (alice(), bob());
        let first = gift_wrap("hi", bob.public_key_hex(), &alice, NOW, &mut rng(5)).expect("wraps");
        let second =
            gift_wrap("hi", bob.public_key_hex(), &alice, NOW, &mut rng(6)).expect("wraps");
        assert_ne!(
            first.pubkey, second.pubkey,
            "reusing a wrap key would let a relay link a sender's messages"
        );
    }

    #[test]
    fn the_wrap_leaks_neither_the_sender_nor_the_plaintext() {
        let (alice, bob) = (alice(), bob());
        let wrap = gift_wrap(
            "the secret plaintext",
            bob.public_key_hex(),
            &alice,
            NOW,
            &mut rng(7),
        )
        .expect("wraps");

        let serialized = serde_json::to_string(&wrap).expect("serializes");
        assert!(!serialized.contains("the secret plaintext"));
        assert!(!serialized.contains(alice.public_key_hex()));
    }

    #[test]
    fn timestamps_are_jittered_but_stay_within_the_window() {
        let (alice, bob) = (alice(), bob());
        let mut saw_a_different_timestamp = false;

        for seed in 0..32u64 {
            let wrap =
                gift_wrap("hi", bob.public_key_hex(), &alice, NOW, &mut rng(seed)).expect("wraps");
            assert!(
                (wrap.created_at - NOW).abs() <= TIMESTAMP_JITTER_SECS,
                "jitter escaped the window: {}",
                wrap.created_at - NOW
            );
            if wrap.created_at != NOW {
                saw_a_different_timestamp = true;
            }
        }

        assert!(
            saw_a_different_timestamp,
            "timestamps were never jittered at all"
        );
    }

    #[test]
    fn the_rumor_keeps_the_true_timestamp_while_the_wrap_does_not() {
        // The whole point of the jitter: the real send time survives inside the
        // encrypted rumor, where relays cannot read it.
        let (alice, bob) = (alice(), bob());
        let rumor = make_rumor(alice.public_key_hex(), "hi", NOW);
        assert_eq!(rumor.created_at, NOW);
        assert!(rumor.sig.is_none(), "the rumor must never be signed");

        let seal =
            make_seal(&rumor, bob.public_key_hex(), &alice, NOW, &mut rng(8)).expect("seals");
        assert!(seal.sig.is_some(), "the seal must be signed by the sender");
        assert_eq!(seal.pubkey, alice.public_key_hex());
    }

    #[test]
    fn the_recipient_is_the_only_one_who_can_unwrap() {
        let (alice, bob, mallory) = (alice(), bob(), mallory());
        let wrap = gift_wrap("hi", bob.public_key_hex(), &alice, NOW, &mut rng(9)).expect("wraps");

        assert!(unwrap_gift(&wrap, bob.secret()).is_ok());
        assert!(unwrap_gift(&wrap, mallory.secret()).is_err());
        assert!(unwrap_gift(&wrap, alice.secret()).is_err());
    }

    /// The impersonation vector. Mallory seals a rumor that claims Alice wrote
    /// it, and signs the seal with her own key. The rumor's author is unsigned,
    /// so the mismatch is the only thing standing between Bob and a forged DM
    /// attributed to Alice.
    #[test]
    fn a_seal_whose_signer_disagrees_with_the_rumor_author_is_rejected() {
        let (alice, bob, mallory) = (alice(), bob(), mallory());

        let forged_rumor = make_rumor(alice.public_key_hex(), "transfer the funds", NOW);
        let seal = make_seal(
            &forged_rumor,
            bob.public_key_hex(),
            &mallory,
            NOW,
            &mut rng(10),
        )
        .expect("seals");
        let wrap = make_wrap(&seal, bob.public_key_hex(), NOW, &mut rng(11)).expect("wraps");

        let err = unwrap_gift(&wrap, bob.secret()).expect_err("must be rejected");
        assert_eq!(
            err,
            NostrError::InvalidEvent("rumor author does not match the seal signer")
        );
    }

    #[test]
    fn an_honest_seal_from_mallory_reports_mallory_not_alice() {
        // The flip side: if Mallory does not lie about the rumor author, the DM
        // is valid — and attributed to her.
        let (bob, mallory) = (bob(), mallory());
        let wrap =
            gift_wrap("hi", bob.public_key_hex(), &mallory, NOW, &mut rng(12)).expect("wraps");
        let (_, sender, _) = unwrap_gift(&wrap, bob.secret()).expect("unwraps");
        assert_eq!(sender, mallory.public_key_hex());
        assert_ne!(sender, alice().public_key_hex());
    }

    #[test]
    fn a_seal_with_a_broken_signature_is_rejected() {
        let (alice, bob) = (alice(), bob());
        let rumor = make_rumor(alice.public_key_hex(), "hi", NOW);
        let mut seal =
            make_seal(&rumor, bob.public_key_hex(), &alice, NOW, &mut rng(13)).expect("seals");
        seal.sig = Some("0".repeat(128));

        let wrap = make_wrap(&seal, bob.public_key_hex(), NOW, &mut rng(14)).expect("wraps");
        assert!(unwrap_gift(&wrap, bob.secret()).is_err());
    }

    #[test]
    fn an_unsigned_seal_is_rejected() {
        let (alice, bob) = (alice(), bob());
        let rumor = make_rumor(alice.public_key_hex(), "hi", NOW);
        let mut seal =
            make_seal(&rumor, bob.public_key_hex(), &alice, NOW, &mut rng(15)).expect("seals");
        seal.sig = None;

        let wrap = make_wrap(&seal, bob.public_key_hex(), NOW, &mut rng(16)).expect("wraps");
        assert!(unwrap_gift(&wrap, bob.secret()).is_err());
    }

    #[test]
    fn a_tampered_seal_id_is_rejected() {
        // The id is the hash the signature commits to, so editing the seal after
        // signing must not survive verification.
        let (alice, bob) = (alice(), bob());
        let rumor = make_rumor(alice.public_key_hex(), "hi", NOW);
        let mut seal =
            make_seal(&rumor, bob.public_key_hex(), &alice, NOW, &mut rng(17)).expect("seals");
        seal.created_at += 1;

        let wrap = make_wrap(&seal, bob.public_key_hex(), NOW, &mut rng(18)).expect("wraps");
        assert!(unwrap_gift(&wrap, bob.secret()).is_err());
    }

    #[test]
    fn a_non_wrap_event_is_rejected_outright() {
        let (alice, bob) = (alice(), bob());
        let mut wrap =
            gift_wrap("hi", bob.public_key_hex(), &alice, NOW, &mut rng(19)).expect("wraps");
        wrap.kind = 1;
        assert_eq!(
            unwrap_gift(&wrap, bob.secret()).expect_err("rejected"),
            NostrError::InvalidEvent("not a gift wrap")
        );
    }

    #[test]
    fn a_wrap_with_junk_content_is_rejected() {
        let (alice, bob) = (alice(), bob());
        let mut wrap =
            gift_wrap("hi", bob.public_key_hex(), &alice, NOW, &mut rng(20)).expect("wraps");
        wrap.content = "v2:not-real-ciphertext".to_string();
        assert!(unwrap_gift(&wrap, bob.secret()).is_err());
    }

    #[test]
    fn gift_wrap_rejects_a_malformed_recipient_key() {
        let alice = alice();
        assert!(gift_wrap("hi", "not hex", &alice, NOW, &mut rng(21)).is_err());
        assert!(gift_wrap("hi", "", &alice, NOW, &mut rng(22)).is_err());
    }

    /// An unsigned rumor has neither `id` nor `sig`, so the inner-event parser
    /// has to tolerate both being absent — otherwise nothing ever unwraps.
    #[test]
    fn the_inner_event_parser_accepts_an_unsigned_rumor() {
        let json = r#"{"pubkey":"ab","created_at":7,"kind":14,"tags":[],"content":"hi"}"#;
        let event = from_json(json).expect("parses");
        assert_eq!(event.id, "");
        assert_eq!(event.sig, None);
        assert_eq!(event.kind, 14);
        assert_eq!(event.content, "hi");
    }

    #[test]
    fn a_round_tripped_event_survives_json() {
        let (alice, bob) = (alice(), bob());
        let rumor = make_rumor(alice.public_key_hex(), "hi / there \"quoted\"", NOW);
        let seal =
            make_seal(&rumor, bob.public_key_hex(), &alice, NOW, &mut rng(23)).expect("seals");
        let parsed = from_json(&to_json(&seal).expect("serializes")).expect("parses");
        assert_eq!(parsed, seal);
    }

    #[test]
    fn randomized_timestamp_covers_both_directions() {
        let mut saw_earlier = false;
        let mut saw_later = false;
        let mut r = rng(24);
        for _ in 0..256 {
            let ts = randomized_timestamp(NOW, &mut r);
            assert!((ts - NOW).abs() <= TIMESTAMP_JITTER_SECS);
            if ts < NOW {
                saw_earlier = true;
            }
            if ts > NOW {
                saw_later = true;
            }
        }
        assert!(saw_earlier && saw_later);
    }

    #[test]
    fn throwaway_keys_differ_between_draws() {
        let mut r = rng(25);
        let a = throwaway_identity(&mut r).expect("key");
        let b = throwaway_identity(&mut r).expect("key");
        assert_ne!(a.public_key_hex(), b.public_key_hex());
    }
}

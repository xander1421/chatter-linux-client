//! NIP-13 proof of work for outgoing geohash messages.
//!
//! Ported from `Nostr/NostrPoW.swift`.
//!
//! A kind-20000 geohash message carries a `["nonce", "<value>", "<target>"]`
//! tag whose value makes the event ID begin with at least `target` zero bits.
//! Two things depend on it, and neither is optional in practice:
//!
//! * bitchat applies a strict per-sender rate-limit bucket to public events and
//!   *skips* it for events whose validated difficulty reaches
//!   [`RATE_LIMIT_BYPASS_BITS`] (`MessageRateLimiter.allow`). Unmined events are
//!   accepted but throttled harder.
//! * Relays that enforce NIP-13 reject an unmined event outright, with
//!   `"blocked: pow required"`.
//!
//! # Bounded by attempts, not by time
//!
//! bitchat bounds mining with a two-second wall clock and steps the committed
//! target down when it expires. This crate has no clock — every builder takes
//! `created_at` from its caller, which is what keeps it testable — so the bound
//! here is [`MAX_ATTEMPTS`] instead. At [`TARGET_BITS`] the expected cost is
//! 2^8 = 256 hashes, so the cap is ~4000x the mean and missing it is not a
//! thing that happens; the caller ships unmined if it somehow does, exactly as
//! bitchat does when mining fails.

use crate::error::NostrError;
use crate::event::event_id;
use crate::types::NostrEvent;

/// Difficulty mined onto outgoing geohash messages, from `NostrPoW.targetBits`.
///
/// 8 bits is ~256 hash attempts — microseconds. It is not meant to be
/// expensive; it is meant to be *present*, because that is what the receiver
/// scores.
pub const TARGET_BITS: u32 = 8;

/// Validated difficulty at or above which bitchat skips its per-sender rate
/// limit, from `NostrPoW.rateLimitBypassBits`. Equal to [`TARGET_BITS`] by
/// construction: we mine exactly enough to clear the bar we are mining for.
pub const RATE_LIMIT_BYPASS_BITS: u32 = 8;

/// Hash attempts before mining gives up. See the module docs for why this is a
/// count and not a duration.
const MAX_ATTEMPTS: u64 = 1_000_000;

/// Width of the hex nonce value, matching bitchat's `%016llx`.
const NONCE_HEX_WIDTH: usize = 16;

/// Leading zero bits of a byte string — the NIP-13 difficulty of an event ID.
#[must_use]
pub fn leading_zero_bits(bytes: &[u8]) -> u32 {
    let mut total = 0;
    for byte in bytes {
        if *byte == 0 {
            total += 8;
        } else {
            total += byte.leading_zeros();
            break;
        }
    }
    total
}

/// The difficulty an inbound event may actually claim.
///
/// The *committed* target in the nonce tag is the ceiling: the ID must really
/// meet it (or the claim is void and scores zero), and work beyond the
/// commitment earns nothing extra. Without that second half a spammer could
/// commit to 1 bit, get lucky, and be scored as though they had mined 20.
///
/// Mirrors `NostrPoW.validatedDifficulty`, including reading the **last**
/// `nonce` tag when there are several.
#[must_use]
pub fn validated_difficulty(id: &[u8; 32], tags: &[Vec<String>]) -> u32 {
    let Some(tag) = tags
        .iter()
        .rev()
        .find(|tag| tag.first().is_some_and(|name| name == "nonce"))
    else {
        return 0;
    };
    if tag.len() < 3 {
        return 0;
    }
    let Ok(committed) = tag[2].parse::<u32>() else {
        return 0;
    };
    if committed == 0 || committed > 256 {
        return 0;
    }
    if leading_zero_bits(id) >= committed {
        committed
    } else {
        0
    }
}

/// Mine a `["nonce", value, target]` tag for `event`.
///
/// `event` must already carry its final `pubkey`, `created_at`, `kind`,
/// `content` and base tags: the nonce commits to the whole serialized event, so
/// changing any of them afterwards invalidates the work. In particular the
/// caller must fix `created_at` *before* mining and reuse that exact value when
/// signing — which is why the builders take it as a parameter.
///
/// `None` means the attempt cap was reached; the caller sends unmined rather
/// than not sending.
pub fn mine_nonce_tag(event: &NostrEvent, target_bits: u32) -> Option<Vec<String>> {
    let target = target_bits.min(256);
    let mut candidate = event.clone();
    // The tag being searched over, appended once and then rewritten in place.
    candidate
        .tags
        .push(vec!["nonce".to_string(), String::new(), target.to_string()]);
    let slot = candidate.tags.len() - 1;

    for nonce in 0..MAX_ATTEMPTS {
        let value = format!("{nonce:0width$x}", width = NONCE_HEX_WIDTH);
        candidate.tags[slot][1] = value;
        // Re-serialize per attempt. bitchat mutates a byte buffer in place to
        // avoid this; at 256 expected attempts the difference is unmeasurable
        // and the simple version cannot get the nonce byte range wrong.
        let Ok(id) = event_id(&candidate) else {
            return None;
        };
        if leading_zero_bits(&id) >= target {
            return Some(candidate.tags.swap_remove(slot));
        }
    }
    None
}

/// Append a mined nonce tag to `event`; `false` means it was left unmined.
///
/// Never an error: an unmined geohash message is throttled by bitchat and may
/// be refused by a PoW-enforcing relay, but refusing to *send* it would be
/// worse. The boolean is there so a caller that has a logger can say so — this
/// crate is sans-IO and has none.
pub fn mine_in_place(event: &mut NostrEvent, target_bits: u32) -> bool {
    match mine_nonce_tag(event, target_bits) {
        Some(tag) => {
            event.tags.push(tag);
            true
        }
        None => false,
    }
}

/// Difficulty of an event as it stands, recomputing its ID from its fields.
pub fn difficulty_of(event: &NostrEvent) -> Result<u32, NostrError> {
    let id = event_id(event)?;
    Ok(validated_difficulty(&id, &event.tags))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::NostrIdentity;
    use crate::types::{EventKind, NostrEvent};

    fn identity() -> NostrIdentity {
        NostrIdentity::from_secret([0x42; 32]).expect("identity")
    }

    fn unmined() -> NostrEvent {
        NostrEvent::unsigned(
            identity().public_key_hex().to_string(),
            1_700_000_000,
            EventKind::EphemeralEvent,
            vec![vec!["g".to_string(), "u4pruy".to_string()]],
            "hello cell".to_string(),
        )
    }

    #[test]
    fn leading_zero_bits_counts_across_byte_boundaries() {
        assert_eq!(leading_zero_bits(&[0xFF]), 0);
        assert_eq!(leading_zero_bits(&[0x7F]), 1);
        assert_eq!(leading_zero_bits(&[0x01]), 7);
        assert_eq!(leading_zero_bits(&[0x00, 0xFF]), 8);
        assert_eq!(leading_zero_bits(&[0x00, 0x0F]), 12);
        assert_eq!(leading_zero_bits(&[0x00, 0x00]), 16);
        assert_eq!(leading_zero_bits(&[]), 0);
    }

    #[test]
    fn mining_produces_a_tag_that_actually_meets_its_commitment() {
        let mut event = unmined();
        mine_in_place(&mut event, TARGET_BITS);

        let tag = event.tags.last().expect("a tag was appended");
        assert_eq!(tag[0], "nonce");
        assert_eq!(tag[1].len(), NONCE_HEX_WIDTH, "fixed-width hex, like bitchat");
        assert_eq!(tag[2], TARGET_BITS.to_string());

        let id = event_id(&event).expect("id");
        assert!(
            leading_zero_bits(&id) >= TARGET_BITS,
            "the mined ID must really meet the committed difficulty"
        );
        assert_eq!(difficulty_of(&event).expect("score"), TARGET_BITS);
        assert!(difficulty_of(&event).expect("score") >= RATE_LIMIT_BYPASS_BITS);
    }

    #[test]
    fn an_unmined_event_scores_zero() {
        assert_eq!(difficulty_of(&unmined()).expect("score"), 0);
    }

    #[test]
    fn a_commitment_the_id_does_not_meet_scores_zero() {
        // The anti-spam half: claiming 32 bits does not earn 32 bits.
        let mut event = unmined();
        mine_in_place(&mut event, TARGET_BITS);
        let slot = event.tags.len() - 1;
        event.tags[slot][2] = "250".to_string();

        assert_eq!(
            difficulty_of(&event).expect("score"),
            0,
            "an unmet commitment is void, not credited at the achieved difficulty"
        );
    }

    #[test]
    fn work_beyond_the_commitment_earns_no_extra_credit() {
        // An all-zero ID has 256 leading zero bits, but commits to 4 — so it
        // scores 4. Otherwise a spammer commits low, banks the occasional lucky
        // hash, and is scored as though they had done the work.
        //
        // Tested on `validated_difficulty` directly rather than by mining and
        // then editing the tag: the commitment is *inside* the ID preimage, so
        // rewriting it changes the ID and destroys the very work being scored.
        let lucky = [0u8; 32];
        assert_eq!(leading_zero_bits(&lucky), 256);
        assert_eq!(
            validated_difficulty(&lucky, &[vec!["nonce".into(), "aa".into(), "4".into()]]),
            4
        );
    }

    #[test]
    fn malformed_nonce_tags_score_zero_rather_than_panicking() {
        let id = [0u8; 32];
        assert_eq!(validated_difficulty(&id, &[]), 0);
        assert_eq!(validated_difficulty(&id, &[vec!["nonce".into()]]), 0);
        assert_eq!(
            validated_difficulty(&id, &[vec!["nonce".into(), "abc".into()]]),
            0,
            "a two-element nonce tag has no commitment"
        );
        assert_eq!(
            validated_difficulty(&id, &[vec!["nonce".into(), "abc".into(), "nope".into()]]),
            0,
            "a non-numeric commitment"
        );
        assert_eq!(
            validated_difficulty(&id, &[vec!["nonce".into(), "abc".into(), "0".into()]]),
            0,
            "zero is not a commitment"
        );
        assert_eq!(
            validated_difficulty(&id, &[vec!["nonce".into(), "abc".into(), "257".into()]]),
            0,
            "wider than the digest"
        );
    }

    #[test]
    fn the_last_nonce_tag_is_the_one_that_counts() {
        // Matches `tags.last(where:)`. A sender that prepends a fake
        // high-difficulty tag must not have it read in preference to the real
        // one at the end.
        let id = [0u8; 32];
        let tags = vec![
            vec!["nonce".into(), "aa".into(), "200".into()],
            vec!["nonce".into(), "bb".into(), "8".into()],
        ];
        assert_eq!(validated_difficulty(&id, &tags), 8);
    }

    #[test]
    fn mining_does_not_disturb_the_base_tags() {
        let mut event = unmined();
        let before = event.tags.clone();
        mine_in_place(&mut event, TARGET_BITS);

        assert_eq!(&event.tags[..before.len()], &before[..]);
        assert_eq!(event.tags.len(), before.len() + 1);
    }
}

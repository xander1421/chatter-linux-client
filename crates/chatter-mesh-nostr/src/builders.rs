//! Constructors for every Nostr event kind bitchat publishes.
//!
//! Ported from `bitchat/Nostr/NostrProtocol.swift` (the `create*Event` family).
//!
//! Each builder assembles the tags, then signs. Signing computes the event ID
//! over the exact tag list, so **tag order is part of the wire format** — a
//! reordering changes the ID and every deduplication key derived from it. The
//! orders below match bitchat and must not be "tidied".
//!
//! `created_at` is a parameter rather than a call to the clock. Event IDs are
//! deterministic given it, which is what makes these builders testable, and it
//! lets a caller pin a timestamp it has already committed to elsewhere (the
//! proof-of-work path in bitchat does exactly that).

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;

use crate::error::NostrError;
use crate::event::sign_event;
use crate::geohash;
use crate::identity::NostrIdentity;
use crate::pow;
use crate::types::{EventKind, GeohashLevel, NostrEvent};

/// Build and sign an event under `identity`.
fn build(
    identity: &NostrIdentity,
    created_at: i64,
    kind: EventKind,
    tags: Vec<Vec<String>>,
    content: String,
) -> Result<NostrEvent, NostrError> {
    let event = NostrEvent::unsigned(
        identity.public_key_hex().to_string(),
        created_at,
        kind,
        tags,
        content,
    );
    sign_event(event, identity.secret())
}

/// A geohash chat message (kind 20000).
///
/// Tags, in order: `["g", geohash]`, then `["n", nickname]` only when the
/// nickname is non-empty after trimming, then `["t", "teleport"]` only when the
/// sender is teleported into the cell rather than physically in it. A blank
/// nickname is omitted rather than sent empty, so that clearing a nickname
/// actually anonymises the message instead of publishing an empty `n` tag.
///
/// Kind 20000 is ephemeral: relays relay it and forget it. It is **plaintext**
/// and readable by anyone subscribed to the cell — that is the point of a public
/// channel, but it means the geohash the caller passes is published verbatim.
pub fn geohash_message(
    geohash: &str,
    content: &str,
    nickname: Option<&str>,
    teleported: bool,
    identity: &NostrIdentity,
    created_at: i64,
) -> Result<NostrEvent, NostrError> {
    let cell = geohash::normalize(geohash)?;

    let mut tags = vec![vec!["g".to_string(), cell]];
    if let Some(nickname) = nickname.map(str::trim).filter(|n| !n.is_empty()) {
        tags.push(vec!["n".to_string(), nickname.to_string()]);
    }
    if teleported {
        tags.push(vec!["t".to_string(), "teleport".to_string()]);
    }

    // Mined before signing, and after `created_at` is fixed: the nonce commits
    // to the whole serialized event, so anything changed afterwards voids the
    // work. bitchat's unmined variant of this event has no production callers —
    // `createMinedEphemeralGeohashEvent` is what ships — and an unmined event is
    // rate-limited harder by every bitchat peer and rejected outright by any
    // relay that enforces NIP-13.
    let mut event = NostrEvent::unsigned(
        identity.public_key_hex().to_string(),
        created_at,
        EventKind::EphemeralEvent,
        tags,
        content.to_string(),
    );
    let _mined = pow::mine_in_place(&mut event, pow::TARGET_BITS);
    sign_event(event, identity.secret())
}

/// A geohash presence heartbeat (kind 20001).
///
/// # Privacy invariant
///
/// Rejects any geohash finer than [`GeohashLevel::MAX_PRESENCE_PRECISION`] with
/// [`NostrError::PresencePrecisionTooFine`]. A heartbeat is an unencrypted,
/// repeated "I am here" addressed to every relay watcher; at neighbourhood,
/// block, or building precision that is a live location feed. The check lives
/// here, in the only place a heartbeat can be constructed, so that no call site
/// — present or future — can opt out of it.
///
/// The event carries the `g` tag and nothing else: empty content, and no `n`
/// nickname tag. A nickname on a heartbeat would tie the derived per-cell pubkey
/// back to a person and undo the derivation in [`crate::identity`].
pub fn geohash_presence(
    geohash: &str,
    identity: &NostrIdentity,
    created_at: i64,
) -> Result<NostrEvent, NostrError> {
    let cell = geohash::normalize(geohash)?;

    let precision = cell.chars().count();
    if precision > GeohashLevel::MAX_PRESENCE_PRECISION {
        return Err(NostrError::PresencePrecisionTooFine(precision));
    }

    build(
        identity,
        created_at,
        EventKind::GeohashPresence,
        vec![vec!["g".to_string(), cell]],
        String::new(),
    )
}

/// A courier drop (kind 1401): an opaque sealed envelope parked on relays for a
/// recipient who is offline.
///
/// `recipient_tag` is the rotating 16-byte address the recipient subscribes for,
/// published hex-encoded under `x`. It rotates precisely so that a relay cannot
/// use it as a long-lived identifier for the recipient.
///
/// `identity` should be a throwaway key. The envelope authenticates its sender
/// internally, so a stable publisher key adds nothing and would let a relay link
/// every drop one user has ever sent.
/// `expires_at` is a unix timestamp in seconds and is **not** optional.
///
/// Kind 1401 is a *regular* kind, so relays store it indefinitely. The NIP-40
/// `expiration` tag is the only thing that lets an honouring relay garbage-collect
/// a drop once its envelope has expired; without it every courier drop we ever
/// publish accumulates on public relays forever. bitchat always emits it, and
/// since tags are inside the event ID, omitting it also makes our drops a
/// different event from theirs for identical content.
pub fn courier_drop(
    recipient_tag: &[u8; 16],
    sealed: &[u8],
    expires_at: i64,
    identity: &NostrIdentity,
    created_at: i64,
) -> Result<NostrEvent, NostrError> {
    if sealed.is_empty() {
        return Err(NostrError::InvalidEvent("courier drop has no envelope"));
    }
    if expires_at <= created_at {
        return Err(NostrError::InvalidEvent(
            "courier drop expires at or before it was created",
        ));
    }

    build(
        identity,
        created_at,
        EventKind::CourierDrop,
        vec![
            vec!["x".to_string(), hex::encode(recipient_tag)],
            vec!["expiration".to_string(), expires_at.to_string()],
        ],
        BASE64.encode(sealed),
    )
}

/// A persistent location note (kind 1) tagged to a geohash.
///
/// Unlike [`geohash_message`], kind 1 is a regular event: relays store it and
/// serve it to anyone who asks later. A note is a durable public record of
/// having been somewhere, so the caller — not this builder — owns the decision
/// to publish one at fine precision.
pub fn location_note(
    geohash: &str,
    content: &str,
    identity: &NostrIdentity,
    created_at: i64,
) -> Result<NostrEvent, NostrError> {
    let cell = geohash::normalize(geohash)?;

    build(
        identity,
        created_at,
        EventKind::TextNote,
        vec![vec!["g".to_string(), cell]],
        content.to_string(),
    )
}

/// A NIP-09 deletion request (kind 5) for one or more of our own events.
///
/// Relays that honour NIP-09 drop the referenced events; the request must be
/// signed by the key that signed the originals, so `identity` has to be the same
/// derived identity that published them. Deletion is a request, not a guarantee:
/// a relay may ignore it, and anything already ephemeral (kinds 20000/20001) was
/// never stored to begin with.
pub fn deletion(
    event_ids: &[String],
    identity: &NostrIdentity,
    created_at: i64,
) -> Result<NostrEvent, NostrError> {
    if event_ids.is_empty() {
        return Err(NostrError::InvalidEvent(
            "deletion request references no events",
        ));
    }

    let mut tags = Vec::with_capacity(event_ids.len());
    for id in event_ids {
        if id.len() != 64 || !id.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(NostrError::InvalidEvent(
                "deletion target is not a 32-byte hex event id",
            ));
        }
        tags.push(vec!["e".to_string(), id.to_ascii_lowercase()]);
    }

    build(
        identity,
        created_at,
        EventKind::Deletion,
        tags,
        String::new(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::verify_event;
    use crate::identity::derive_for_geohash;

    const SEED: [u8; 32] = [3u8; 32];
    const NOW: i64 = 1_700_000_000;

    fn identity() -> NostrIdentity {
        derive_for_geohash(&SEED, "u4pru").expect("derives")
    }

    #[test]
    fn geohash_message_has_kind_20000_and_is_signed_by_the_identity() {
        let id = identity();
        let event = geohash_message("u4pru", "hello", None, false, &id, NOW).expect("builds");

        assert_eq!(event.kind, EventKind::EphemeralEvent.as_u16());
        assert_eq!(event.kind, 20000);
        assert_eq!(event.pubkey, id.public_key_hex());
        assert_eq!(event.content, "hello");
        assert_eq!(event.created_at, NOW);
        assert_eq!(event.id.len(), 64);
        assert!(event.sig.is_some());
        assert!(verify_event(&event));
    }

    /// The tags a geohash message carries before the mined nonce is appended.
    ///
    /// Every geohash message is mined (see `pow`), so the nonce tag is always
    /// last and its value is not predictable — assertions about tag *order* have
    /// to be made about the prefix.
    fn base_tags(event: &NostrEvent) -> &[Vec<String>] {
        let last = event.tags.last().expect("at least the g tag");
        assert_eq!(last[0], "nonce", "the mined nonce tag is appended last");
        assert_eq!(last[2], pow::TARGET_BITS.to_string());
        &event.tags[..event.tags.len() - 1]
    }

    #[test]
    fn geohash_message_tag_order_is_g_then_n_then_t() {
        let event =
            geohash_message("u4pru", "hi", Some("alice"), true, &identity(), NOW).expect("builds");
        assert_eq!(
            base_tags(&event),
            vec![
                vec!["g".to_string(), "u4pru".to_string()],
                vec!["n".to_string(), "alice".to_string()],
                vec!["t".to_string(), "teleport".to_string()],
            ]
        );
    }

    #[test]
    fn geohash_message_carries_proof_of_work_that_verifies() {
        // Without it every bitchat peer applies its strict per-sender rate
        // limit to us, and a NIP-13-enforcing relay refuses the event outright.
        let event =
            geohash_message("u4pru", "hi", Some("alice"), false, &identity(), NOW).expect("builds");
        assert_eq!(
            pow::difficulty_of(&event).expect("score"),
            pow::TARGET_BITS,
            "the signed event's own ID must meet the difficulty it commits to"
        );
        assert!(pow::difficulty_of(&event).expect("score") >= pow::RATE_LIMIT_BYPASS_BITS);
        assert!(
            verify_event(&event),
            "mining must not disturb the signature"
        );
    }

    #[test]
    fn geohash_message_omits_a_blank_nickname() {
        for nickname in [None, Some(""), Some("   "), Some("\t\n")] {
            let event =
                geohash_message("u4pru", "hi", nickname, false, &identity(), NOW).expect("builds");
            assert_eq!(
                base_tags(&event),
                vec![vec!["g".to_string(), "u4pru".to_string()]],
                "nickname {nickname:?} should produce no n tag"
            );
        }
    }

    #[test]
    fn geohash_message_trims_a_padded_nickname() {
        let event = geohash_message("u4pru", "hi", Some("  alice  "), false, &identity(), NOW)
            .expect("builds");
        assert_eq!(event.tag_value("n"), Some("alice"));
    }

    #[test]
    fn geohash_message_omits_the_teleport_tag_when_not_teleported() {
        let event =
            geohash_message("u4pru", "hi", Some("alice"), false, &identity(), NOW).expect("builds");
        assert_eq!(base_tags(&event).len(), 2);
        assert_eq!(event.tag_value("t"), None);
    }

    #[test]
    fn geohash_message_normalises_the_cell_and_rejects_junk() {
        let event = geohash_message("U4PRU", "hi", None, false, &identity(), NOW).expect("builds");
        assert_eq!(event.tag_value("g"), Some("u4pru"));

        assert!(geohash_message("", "hi", None, false, &identity(), NOW).is_err());
        assert!(geohash_message("u4pra", "hi", None, false, &identity(), NOW).is_err());
    }

    #[test]
    fn geohash_message_is_allowed_at_any_precision() {
        // Chat, unlike presence, is a deliberate act: fine cells are the point of
        // a block-level channel.
        for cell in ["u4", "u4pru", "u4pruy", "u4pruydq", "u4pruydqqvj"] {
            assert!(
                geohash_message(cell, "hi", None, false, &identity(), NOW).is_ok(),
                "{cell}"
            );
        }
    }

    // MARK: presence

    #[test]
    fn presence_has_kind_20001_empty_content_and_only_a_g_tag() {
        let id = identity();
        let event = geohash_presence("u4pru", &id, NOW).expect("builds");

        assert_eq!(event.kind, EventKind::GeohashPresence.as_u16());
        assert_eq!(event.kind, 20001);
        assert_eq!(event.content, "");
        assert_eq!(event.tags, vec![vec!["g".to_string(), "u4pru".to_string()]]);
        assert_eq!(event.tag_value("n"), None);
        assert_eq!(event.pubkey, id.public_key_hex());
        assert!(verify_event(&event));
    }

    /// The privacy invariant. A precision-7 presence event must not merely be
    /// discouraged — it must be impossible to construct.
    #[test]
    fn presence_at_block_precision_cannot_be_constructed() {
        let err = geohash_presence("u4pruyd", &identity(), NOW).expect_err("must be rejected");
        assert_eq!(err, NostrError::PresencePrecisionTooFine(7));
    }

    #[test]
    fn presence_is_rejected_at_every_precision_finer_than_five() {
        for (cell, precision) in [
            ("u4pruy", 6),
            ("u4pruyd", 7),
            ("u4pruydq", 8),
            ("u4pruydqq", 9),
            ("u4pruydqqvj", 11),
            ("u4pruydqqvjx", 12),
        ] {
            let err = geohash_presence(cell, &identity(), NOW)
                .expect_err("presence must be refused at fine precision");
            assert_eq!(
                err,
                NostrError::PresencePrecisionTooFine(precision),
                "cell {cell}"
            );
        }
    }

    #[test]
    fn presence_is_accepted_at_city_precision_and_coarser() {
        for cell in ["u", "u4", "u4p", "u4pr", "u4pru"] {
            let event = geohash_presence(cell, &identity(), NOW)
                .unwrap_or_else(|e| panic!("presence at {cell} should be allowed: {e}"));
            assert_eq!(event.tag_value("g"), Some(cell));
        }
    }

    #[test]
    fn presence_counts_precision_after_normalising_the_cell() {
        // Uppercase input must not slip past the precision check.
        assert_eq!(
            geohash_presence("U4PRUYD", &identity(), NOW).expect_err("rejected"),
            NostrError::PresencePrecisionTooFine(7)
        );
        assert!(geohash_presence("U4PRU", &identity(), NOW).is_ok());
    }

    #[test]
    fn presence_rejects_an_invalid_geohash_before_the_precision_check() {
        // Not a precision error — a malformed cell is not "too fine".
        assert!(matches!(
            geohash_presence("u4pra", &identity(), NOW),
            Err(NostrError::InvalidGeohash(_))
        ));
        assert!(matches!(
            geohash_presence("", &identity(), NOW),
            Err(NostrError::InvalidGeohash(_))
        ));
    }

    // MARK: courier drop

    #[test]
    fn courier_drop_tags_the_recipient_and_base64s_the_envelope() {
        let tag = [0xabu8; 16];
        let sealed = b"sealed envelope bytes";
        let event = courier_drop(&tag, sealed, NOW + 3600, &identity(), NOW).expect("builds");

        assert_eq!(event.kind, EventKind::CourierDrop.as_u16());
        assert_eq!(event.kind, 1401);
        // Tag order is inside the event ID, so `x` before `expiration` is part of
        // the wire format, not a stylistic choice.
        assert_eq!(
            event.tags,
            vec![
                vec!["x".to_string(), "ab".repeat(16)],
                vec!["expiration".to_string(), (NOW + 3600).to_string()],
            ]
        );
        assert_eq!(BASE64.decode(&event.content).expect("valid base64"), sealed);
        assert!(verify_event(&event));
    }

    #[test]
    fn courier_drop_rejects_an_empty_envelope() {
        assert!(courier_drop(&[0u8; 16], &[], NOW + 3600, &identity(), NOW).is_err());
        // An expiry at or before creation is refused: a drop that is born expired
        // is either a clock bug or an attempt to make relays store it forever.
        assert!(courier_drop(&[0u8; 16], b"sealed", NOW, &identity(), NOW).is_err());
    }

    // MARK: location note

    #[test]
    fn location_note_is_a_kind_1_text_note_with_a_g_tag() {
        let event =
            location_note("u4pruydq", "left a note here", &identity(), NOW).expect("builds");
        assert_eq!(event.kind, EventKind::TextNote.as_u16());
        assert_eq!(event.kind, 1);
        assert_eq!(
            event.tags,
            vec![vec!["g".to_string(), "u4pruydq".to_string()]]
        );
        assert_eq!(event.content, "left a note here");
        assert!(verify_event(&event));
    }

    #[test]
    fn location_note_rejects_an_invalid_geohash() {
        assert!(location_note("nope!", "hi", &identity(), NOW).is_err());
    }

    // MARK: deletion

    #[test]
    fn deletion_emits_one_e_tag_per_event_id() {
        let ids = vec!["aa".repeat(32), "bb".repeat(32)];
        let event = deletion(&ids, &identity(), NOW).expect("builds");

        assert_eq!(event.kind, EventKind::Deletion.as_u16());
        assert_eq!(event.kind, 5);
        assert_eq!(event.content, "");
        assert_eq!(
            event.tags,
            vec![
                vec!["e".to_string(), "aa".repeat(32)],
                vec!["e".to_string(), "bb".repeat(32)],
            ]
        );
        assert!(verify_event(&event));
    }

    #[test]
    fn deletion_lowercases_event_ids() {
        let ids = vec!["AB".repeat(32)];
        let event = deletion(&ids, &identity(), NOW).expect("builds");
        let expected = "ab".repeat(32);
        assert_eq!(event.tag_value("e"), Some(expected.as_str()));
    }

    #[test]
    fn deletion_rejects_empty_and_malformed_targets() {
        assert!(deletion(&[], &identity(), NOW).is_err());
        assert!(deletion(&["short".to_string()], &identity(), NOW).is_err());
        assert!(deletion(&["zz".repeat(32)], &identity(), NOW).is_err());
        assert!(
            deletion(&["aa".repeat(32), "nope".to_string()], &identity(), NOW).is_err(),
            "one bad id must reject the whole request"
        );
    }

    // MARK: cross-cutting

    #[test]
    fn identical_inputs_give_identical_event_ids() {
        // Signatures carry random aux and so differ, but the ID is a hash of the
        // serialized event and must not.
        let a =
            geohash_message("u4pru", "hi", Some("alice"), true, &identity(), NOW).expect("builds");
        let b =
            geohash_message("u4pru", "hi", Some("alice"), true, &identity(), NOW).expect("builds");
        assert_eq!(a.id, b.id);
    }

    #[test]
    fn tag_order_changes_the_event_id() {
        // Guards the ordering contract: if `n` and `t` were ever swapped, this
        // would be the test that noticed.
        let with_teleport =
            geohash_message("u4pru", "hi", Some("alice"), true, &identity(), NOW).expect("builds");
        let without =
            geohash_message("u4pru", "hi", Some("alice"), false, &identity(), NOW).expect("builds");
        assert_ne!(with_teleport.id, without.id);
    }
}

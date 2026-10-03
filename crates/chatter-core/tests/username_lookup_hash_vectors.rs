//! The username discovery hash is libsignal's, and only libsignal's.
//!
//! `hash_username_lookup` is what registration stores as
//! `username_lookup_hash` and what `LookupUsername` sends. The server never
//! computes it (it never sees the username), so there is exactly one
//! implementation and these vectors pin it to libsignal's published one.

use chatter_core::crypto::auth_crypto::hash_username_lookup;

/// libsignal `java/client/src/test/java/org/signal/libsignal/usernames/
/// UsernamesTest.java` `testValidUsernameHashing`: `he110.42`.
const HE110_42: &str = "f63f0521eb3adfe1d936f4b626b89558483507fbdb838fc554af059111cf322e";

#[test]
fn matches_libsignal_published_vector() {
    let got = hash_username_lookup("he110.42").expect("valid Signal username");
    assert_eq!(hex::encode(got), HE110_42);
}

#[test]
fn nickname_case_does_not_change_the_hash() {
    // libsignal lowercases the nickname before hashing (username.rs
    // `from_parts_without_soft_limit`), so a differently-cased entry of the
    // same username must find the same account.
    for u in ["HE110.42", "He110.42", "hE110.42"] {
        let got = hash_username_lookup(u).expect("valid Signal username");
        assert_eq!(hex::encode(got), HE110_42, "{u}");
    }
}

#[test]
fn discriminator_is_part_of_the_hash() {
    let a = hash_username_lookup("he110.42").unwrap();
    let b = hash_username_lookup("he110.43").unwrap();
    assert_ne!(a, b);
}

#[test]
fn names_outside_signal_grammar_have_no_hash() {
    // No discriminator, single-digit or zero-padded discriminator, leading
    // digit, non-ASCII, empty part: every one is refused rather than hashed
    // some other way.
    for u in ["alice", "alice.4", "alice.042", "1alice.42", "用户.42", "alice.", ".42"] {
        assert!(hash_username_lookup(u).is_err(), "{u} must be rejected");
    }
}

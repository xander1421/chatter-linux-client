//! Cross-language determinism check for Signal-style username discovery.
//!
//! Reads `testdata/username_lookup_hash.json` at the repo root and verifies
//! that `hash_username_lookup` produces byte-identical output to the Go
//! server implementation in `internal/services/user/service/profile.go`.
//! The same fixture is consumed by `TestUsernameLookupHashVectors` on the
//! Go side — if either implementation drifts, registrations on one
//! platform become undiscoverable from the other.

use chatter_core::crypto::auth_crypto::hash_username_lookup;
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Deserialize)]
struct Vector {
    username: String,
    expected_hex: String,
}

#[derive(Deserialize)]
struct Fixture {
    salt: String,
    info: String,
    vectors: Vec<Vector>,
}

fn fixture_path() -> PathBuf {
    // Walk up from the crate dir until we find a `testdata` sibling of a
    // workspace / repo root (the Go server lives alongside it).
    let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    loop {
        let candidate = dir.join("testdata").join("username_lookup_hash.json");
        if candidate.exists() {
            return candidate;
        }
        if !dir.pop() {
            panic!("testdata/username_lookup_hash.json not found walking up from CARGO_MANIFEST_DIR");
        }
    }
}

#[test]
fn fixture_matches_rust_implementation() {
    let path = fixture_path();
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read {}: {}", path.display(), e));
    let fx: Fixture = serde_json::from_str(&raw).expect("parse fixture JSON");

    assert_eq!(
        fx.salt, "chatter/username-discovery/v1",
        "fixture salt drifted — coordinated migration required"
    );
    assert_eq!(fx.info, "lookup", "fixture info drifted");
    assert!(!fx.vectors.is_empty(), "fixture has no vectors");

    for v in &fx.vectors {
        let want = hex::decode(&v.expected_hex).expect("bad hex in fixture");
        let got = hash_username_lookup(&v.username);
        assert_eq!(
            got.as_slice(),
            want.as_slice(),
            "username {:?}: got {}, want {}",
            v.username,
            hex::encode(got),
            v.expected_hex
        );
    }
}

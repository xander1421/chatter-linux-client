//! Live functional test for Signal-style username discovery.
//!
//! Exercises the REAL client registration path (`ChatterClient::register` →
//! `do_register` → `register`), which computes `hash_username_lookup(full)` and
//! sends it in `RegisterRequest.username_lookup_hash`, then verifies the server
//! stored it by resolving the handle back via `LookupUsername`.
//!
//! Requires `INTEGRATION_TESTS=1` + a server on `localhost:50051`
//! (`skip_if_no_server!()`). Isolated tempdir per client.

use crate::{common, skip_if_no_server};

use std::sync::Arc;

use chatter_core::client::ChatterClient;

const SERVER_URL: &str = "https://localhost:50051";
const PIN: &str = "Kf8mq2xpZ"; // non-blocklisted, clears validate_pin
const PASSWORD: &str = "TestPass123!";

fn build_client(dir: &tempfile::TempDir) -> Arc<ChatterClient> {
    ChatterClient::builder()
        .server_url(SERVER_URL)
        .data_dir(dir.path().to_str().unwrap())
        .build()
        .expect("Failed to build ChatterClient")
}

fn unique_nick(prefix: &str) -> String {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let mut b = [0u8; 4];
    rand::RngCore::fill_bytes(&mut rand::rng(), &mut b);
    format!("{}{:x}{}", prefix, u32::from_le_bytes(b), ts % 100000)
}

/// Register a fresh user, then resolve their own discriminated handle.
/// If discovery works end-to-end, `lookup_by_username(full)` returns the
/// just-registered user id.
#[tokio::test]
async fn username_lookup_resolves_registered_user() {
    skip_if_no_server!();

    let dir = tempfile::tempdir().unwrap();
    let client = build_client(&dir);
    client.connect().await.expect("connect failed");

    let nick = unique_nick("lookup");
    let result = client
        .register(&nick, PASSWORD, "Lookup Test", "e2e-test", None, Some(PIN))
        .await
        .expect("registration failed");
    let my_id = result.user_id;

    // The discriminated `nickname.discriminator` the client actually registered.
    let full = client
        .full_username()
        .expect("full_username should be set after registration");
    eprintln!("registered full handle = {full}  user_id = {my_id}");

    // Round-trip: resolve the handle we just registered.
    let resolved = client
        .lookup_by_username(&full)
        .await
        .expect("lookup_by_username RPC failed");
    eprintln!("lookup({full}) -> {resolved:?}");

    assert_eq!(
        resolved,
        Some(my_id),
        "username discovery broken: registered handle {full} did not resolve to its own user id \
         (server username_lookup_hash likely not persisted)"
    );
}

/// A handle that was never registered must resolve to nothing.
#[tokio::test]
async fn username_lookup_unknown_returns_none() {
    skip_if_no_server!();

    let dir = tempfile::tempdir().unwrap();
    let client = build_client(&dir);
    client.connect().await.expect("connect failed");

    // Register so the client has a session (lookup requires current user).
    let nick = unique_nick("lookupneg");
    client
        .register(&nick, PASSWORD, "Lookup Neg", "e2e-test", None, Some(PIN))
        .await
        .expect("registration failed");

    // A syntactically-valid but unregistered handle (non-leading-zero discriminator).
    let missing = format!("{}.4242", unique_nick("ghost"));
    let resolved = client
        .lookup_by_username(&missing)
        .await
        .expect("lookup_by_username RPC failed");
    assert_eq!(resolved, None, "unregistered handle unexpectedly resolved");
}

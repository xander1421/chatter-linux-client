# LiveKit API 0.5.5 private-PKI signaling patch

Upstream: https://crates.io/crates/livekit-api/0.5.5 (Apache-2.0).
Extracted the published cached `.crate` archive, preserving source attribution,
manifest, tests, README and VCS metadata. Archive SHA-256, matching the accepted
workspace lock checksum:

`1006f7c009369fe5f16f2eb6e93d6d0dcb2b427306a2f61b26ff5d2e440ec282`

Workspace `[patch.crates-io] livekit-api` selects this reproducible copy. The
workspace lock removes only the registry source/checksum for this package and
adds the already-selected tokio-rustls dependency. No SDK version changes.

`SignalOptions.tls_root_ca_pem` supplies an optional per-client private CA.
An explicit CA replaces default roots in WebSocket TLS and diagnostic HTTPS,
including signaling reconnect and HTTP CONNECT proxy TLS. It retains standard
certificate/hostname verification, HTTPS-only diagnostic requests and forbids
redirects. Malformed/empty explicit anchors and unsupported TLS backends fail
closed. Private-PKI callers skip cloud region discovery, whose separate default
HTTP client must never receive their token. Callers leaving the option unset
retain the ordinary SDK default behavior.

The matching `vendor/livekit/src/room/mod.rs` option forwards this setting at
room construction. Chatter's room adapter supplies its existing baked TLS CA
for WSS and rejects production plaintext signaling before changing room state.
Explicit development builds retain local WS testing.

Modified upstream files: Cargo.toml (enable existing tokio-rustls dependency
for all Rustls feature choices and add rcgen as a test-only fixture dependency),
src/signal_client/mod.rs (configuration/initial/reconnect/validation plumbing),
src/signal_client/signal_stream.rs (per-client connector for direct/proxy TLS).
Added pinned_tls.rs and pinned_tls_tests.rs. Tests generate disposable CA/leaf
keys in memory and exercise real loopback TLS signaling, reconnect, diagnostic
HTTP, wrong-CA and wrong-host rejection; no native WebRTC or external server.

For isolated tests outside the application workspace, copy this directory into
an artifact directory, append an empty `[workspace]` to that copy's manifest,
seed its Cargo.lock with the accepted application lock and run:

```
cargo test --manifest-path <copy>/Cargo.toml --lib --no-default-features \
  --features signal-client-tokio,rustls-tls-webpki-roots pinned_tls -j2 --offline
```

Use private XDG/DBUS paths, CHATTER_NO_KEYRING=1, and empty HTTP(S)/ALL_PROXY
variables for these loopback fixture tests. The archive's original Cargo.lock
is retained as upstream provenance; the application's lock controls shipping.

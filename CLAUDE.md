# CLAUDE.md — Chatter Client

Rust + Slint client. This is the only Chatter client; it ships on Linux,
Windows and Android. `ios/` + `crates/chatter-ios` and `macos/` hold the Apple
target scaffolding.

## Build & Run

```bash
cargo run -p chatter-ui
```

Agents never launch it bare (a bare run pops a keyring prompt on the desktop):
`CHATTER_NO_KEYRING=1 XDG_DATA_HOME=$(mktemp -d) timeout 120 cargo run -p chatter-ui`.

`cargo check` type-checks (including Slint codegen, which runs in
`chatter-ui-slint/build.rs`) but does not link; link errors surface only in
`cargo build` / `cargo run`. The generated `chatter-ui-slint` crate is the
expensive compile (measurements in the workspace `Cargo.toml` comment above
`[profile.dev.package.chatter-ui-slint]`; see `docs/BUILD_SPEED.md`).

Android: `android/build-android.sh [install]`. Release packaging (Android, Linux):
`packaging/README.md`.

## Workspace

Thirteen crates (`Cargo.toml` `[workspace] members`):

- `chatter-core` — headless SDK: crypto, network, storage, services, inbox
- `chatter-ui` — Slint app + glue (desktop binary and Android library)
- `chatter-ui-slint` — generated Slint components (compiles `ui/main.slint`)
- `chatter-proto` — generated protobuf / gRPC client code (`make proto-gen` in the server repo)
- `chatter-attachment-crypto` — attachment AES-256-CBC + HMAC-SHA256, its own crate so it builds at dev `opt-level = 2`
- `chatter-ios` — iOS staticlib entry point
- `chatter-mesh` — sans-IO bitchat wire format
- `chatter-mesh-noise` — Noise XX session layer
- `chatter-mesh-link` — peer registry, ingress guard, announce policy
- `chatter-mesh-ble`, `chatter-mesh-android` — BLE transports (BlueZ / JNI)
- `chatter-mesh-nostr` — Nostr relay transport
- `chatter-mesh-store` — mesh outbox / courier / archive

`crates/chatter-core/fuzz` is excluded from the workspace (nightly
`cargo fuzz`; see its README).

The mesh crates are wired into `chatter-core` (`services/mesh/`) with UI in
`ui/mesh/`, **behind the `mesh` cargo feature, which is off by default**. The
chatter-mesh* crates are optional dependencies of `chatter-core`, and
`services::mesh`, `storage::mesh_store` and `storage::mesh_identity_store` are
`#[cfg(feature = "mesh")]`, so a default build links none of them. Builds that
need it opt in: `android/build-android.sh` (on unless `MESH=0`), `dev-run.sh`,
`make test-rust`, `make check-features-mesh` (server-repo `Makefile`). Mesh rows
in `crates/chatter-core/tests/production_wiring.rs` are checked only with
`--features mesh`.

## Architecture Rules

- **Business logic lives in `chatter-core`, never in `chatter-ui`** — All filtering, data shaping, and conditional logic belongs in `chatter-core` services. The UI crate (`chatter-ui`) is thin presentation. Need different data → add a service method, don't filter in UI.
- Slint files in `ui/`, Rust glue in `crates/chatter-ui/src/bin_ui/`.

## Before trusting a security claim in this crate

A control that reads as implemented may have no production caller. Check for
callers before citing one.

`crates/chatter-core/tests/production_wiring.rs` is the executable list: it
strips tests and comments from the crate sources, counts real call sites, and
fails when an entry changes in either direction — a wired control losing its
last caller, or a dead one gaining its first. Trust it over prose.

Wired controls it pins include:

- **Mesh announce trust policy** — `services/mesh/ble/announces.rs` calls
  `evaluate_announce` (`chatter-mesh-link/src/announce.rs`), including the
  sender-ID-equals-`SHA256(noise key)[..8]` derivation check. Announce
  authentication is `verify_packet` (`chatter-mesh-noise/src/sign.rs`);
  `ANNOUNCE_CONTEXT` (`chatter-mesh-noise/src/lib.rs`) keeps the domain strings
  prefix-free of one another.
- **KT append-only check** — `services/signal/kt_anchor.rs` pins the highest
  tree head this device has accepted and calls
  `crypto/keytransparency.rs::verify_consistency_proof` before advancing it;
  `services/signal/session/kt.rs` runs it via `check_tree_head_consistency` /
  `hold_head_locked`.
- **Peer identity change** — `ui/chat/identity_change_bar.slint` →
  `ui/chat/chat_layout.slint` → `bin_ui/features/chat/identity_change.rs`
  (`wire_accept`) → `ChatterClient::accept_peer_identity_change`
  (`client/trust.rs`) → `services/signal/identity.rs`.

Open security items, with citations, live in `docs/OPEN-ITEMS.md` in the
server repo.

## Coding Guidelines

Follow the `andrej-karpathy-skills:karpathy-guidelines` skill: think before coding, simplicity first, surgical changes, goal-driven execution.

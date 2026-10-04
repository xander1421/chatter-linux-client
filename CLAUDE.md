# CLAUDE.md — Chatter Linux Client

Rust + Slint client. Linux/Windows/Android targets. This is the only Chatter
client; `android-client/` was deleted and `ios-client/` is empty.

## Build & Run

```bash
cargo run -p chatter-ui
```

Use `cargo run` for verification, not `cargo build` / `cargo check` — Slint codegen + linker errors only surface at run.

## Workspace

Eleven crates (`Cargo.toml` `[workspace]`):

- `chatter-core` — headless SDK: crypto, network, storage, services, inbox
- `chatter-ui` — Slint app + glue (desktop binary and Android library)
- `chatter-ui-slint` — generated Slint components
- `chatter-ios` — iOS staticlib entry point
- `chatter-mesh` — sans-IO bitchat wire format
- `chatter-mesh-noise` — Noise XX session layer
- `chatter-mesh-link` — peer registry, ingress guard, announce policy
- `chatter-mesh-ble`, `chatter-mesh-android` — BLE transports
- `chatter-mesh-nostr` — Nostr relay transport
- `chatter-mesh-store` — mesh outbox / courier / archive

The mesh crates are wired into `chatter-core` (`services/mesh/`) with UI in
`ui/mesh/`, **behind the `mesh` cargo feature, which is off by default**. The
chatter-mesh* crates are optional dependencies of `chatter-core`, and
`services::mesh`, `storage::mesh_store` and `storage::mesh_identity_store` are
`#[cfg(feature = "mesh")]`, so a default build links none of them
(`cargo tree -p chatter-ui -e normal | grep chatter-mesh` is empty). Builds that
need it opt in: `android/build-android.sh` (on unless `MESH=0`), `dev-run.sh`,
`make test-rust`, `make check-features-mesh`. Mesh rows in
`tests/production_wiring.rs` are checked only with `--features mesh`.

## Architecture Rules

- **Business logic lives in `chatter-core`, never in `chatter-ui`** — All filtering, data shaping, and conditional logic belongs in `chatter-core` services. The UI crate (`chatter-ui`) is thin presentation. Need different data → add a service method, don't filter in UI.
- Slint files in `ui/`, Rust glue in `crates/chatter-ui/src/bin_ui/`.

## Before trusting a security claim in this crate

Some controls that read as implemented are not reachable from production code.
Check for callers before citing any of them.

`crates/chatter-core/tests/production_wiring.rs` now holds this list as an
executable table and fails when an entry changes in either direction — a wired
control losing its last caller, or a dead one gaining its first. Prefer it over
this prose, which has already gone stale once.

- **CORRECTED (was wrong here):** the announce trust policy
  (`chatter-mesh-link/src/announce.rs`: `preflight_announce`,
  `evaluate_announce_trust`, `evaluate_announce`) **is** wired now —
  `services/mesh/ble.rs:823` calls `evaluate_announce`, which drives the other
  two. Mesh peers are authenticated on the live path, including the
  sender-ID-equals-`SHA256(noise key)[..8]` derivation check.
- Announce authentication on that path is `verify_packet`
  (`chatter-mesh-noise/src/sign.rs`). The unused announce-specific preimage
  (`sign_announce` / `verify_announce` / `canonical_announce_bytes`) and the
  announce-back planner (`plan_announce_response`, `announce_back_dedup_id`)
  were removed. `ANNOUNCE_CONTEXT` stays so the domain strings remain
  prefix-free of one another.
- **CORRECTED:** `crypto/keytransparency.rs::verify_consistency_proof` is wired
  as of 2026-08-03. `services/signal/kt_anchor.rs` pins the highest tree head
  this device has accepted and requires a consistency proof before advancing;
  `session.rs` calls it on every KT-verified send, so forks and rollbacks are
  now rejected rather than invisible.
- **CORRECTED (was wrong here, in the dangerous direction):**
  `services/signal/identity.rs::accept_peer_identity_change` **is** wired now,
  end to end: `ui/chat/identity_change_bar.slint:35` → `chat_layout.slint:213`
  → `app.slint:1464` → `features/chat/identity_change.rs:75,84` →
  `client/mod.rs:931` → `identity.rs:227`. A peer reinstalling no longer wedges
  the conversation permanently. `production_wiring.rs` pins both directions.

The full list, with citations, is `docs/OPEN-ITEMS.md` in the server repo.

## Coding Guidelines

Follow the `andrej-karpathy-skills:karpathy-guidelines` skill: think before coding, simplicity first, surgical changes, goal-driven execution.

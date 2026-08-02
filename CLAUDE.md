# CLAUDE.md — Chatter Linux Client

Rust + Slint client. Linux/Windows/Android targets. This is the only Chatter
client; `android-client/` was deleted and `ios-client/` is empty.

## Build & Run

```bash
cargo run -p chatter-ui
```

Use `cargo run` for verification, not `cargo build` / `cargo check` — Slint codegen + linker errors only surface at run.

## Workspace

Nine crates (`Cargo.toml` `[workspace]`):

- `chatter-core` — headless SDK: crypto, network, storage, services, inbox
- `chatter-ui` — Slint app + glue (desktop binary and Android library)
- `chatter-mesh` — sans-IO bitchat wire format
- `chatter-mesh-noise` — Noise XX session layer
- `chatter-mesh-link` — peer registry, ingress guard, announce policy
- `chatter-mesh-ble`, `chatter-mesh-android` — BLE transports
- `chatter-mesh-nostr` — Nostr relay transport
- `chatter-mesh-store` — mesh outbox / courier / archive

The mesh crates **are** wired into `chatter-core` (`services/mesh/`) with UI in
`ui/mesh/`. Older docs saying "chatter-mesh is not wired into anything" are
stale.

## Architecture Rules

- **Business logic lives in `chatter-core`, never in `chatter-ui`** — All filtering, data shaping, and conditional logic belongs in `chatter-core` services. The UI crate (`chatter-ui`) is thin presentation. Need different data → add a service method, don't filter in UI.
- Slint files in `ui/`, Rust glue in `crates/chatter-ui/src/bin_ui/`.

## Before trusting a security claim in this crate

Two controls that read as implemented are not reachable from production code.
Check for callers before citing either:

- `chatter-mesh-link/src/announce.rs` — the announce trust policy
  (`preflight_announce`, `evaluate_announce_trust`, `evaluate_announce`) has
  zero callers outside its own `#[cfg(test)]` module. Mesh peers are
  unauthenticated on the live path.
- `crypto/keytransparency.rs::verify_consistency_proof` — only called from
  `tests/validated.rs`. Inclusion proofs *are* verified in production
  (`services/signal/session.rs:76`, `:664`); consistency proofs are not.

The full list, with citations, is `docs/OPEN-ITEMS.md` in the server repo.

## Coding Guidelines

Follow the `andrej-karpathy-skills:karpathy-guidelines` skill: think before coding, simplicity first, surgical changes, goal-driven execution.

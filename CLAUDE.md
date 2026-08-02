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

Some controls that read as implemented are not reachable from production code.
Check for callers before citing any of them.

`crates/chatter-core/tests/production_wiring.rs` now holds this list as an
executable table and fails when an entry changes in either direction — a wired
control losing its last caller, or a dead one gaining its first. Prefer it over
this prose, which has already gone stale once.

- **CORRECTED (was wrong here):** the announce trust policy
  (`chatter-mesh-link/src/announce.rs`: `preflight_announce`,
  `evaluate_announce_trust`, `evaluate_announce`) **is** wired now —
  `services/mesh/ble.rs:782` calls `evaluate_announce`, which drives the other
  two. Mesh peers are authenticated on the live path, including the
  sender-ID-equals-`SHA256(noise key)[..8]` derivation check.
- Still dead in that crate: `plan_announce_response` and
  `announce_back_dedup_id` (the announce-back / initial-sync / afterglow half of
  the ported handler), and the whole
  `chatter-mesh-noise/src/sign.rs` `sign_announce` / `verify_announce` /
  `canonical_announce_bytes` trio — production authenticates announces with the
  packet-level `verify_packet` instead, so anyone citing the announce-specific
  preimage is reading dead code.
- `crypto/keytransparency.rs::verify_consistency_proof` — only called from
  `tests/validated.rs`. Inclusion proofs *are* verified in production
  (`services/signal/session.rs:76`, `:712`); consistency proofs are not.
  V25 in `VALIDATED.md` reads as though the client monitors the log; it does
  not — the test calls the verifier by hand.
- `services/signal/identity.rs::accept_peer_identity_change` and
  `pending_identity_change` — zero production callers, and this one wedges.
  Production writes `pending_identity_changes` rows (`note_identity_change`,
  from the four send paths in `session.rs`) and reads them (`session.rs:172`,
  which refuses the send with `UntrustedIdentity`), but `accept_…` holds the
  only `DELETE` and nothing outside `identity.rs` reaches it. A peer
  reinstalling therefore breaks that conversation permanently, with no shipped
  path — UI or otherwise — that can clear the row.

The full list, with citations, is `docs/OPEN-ITEMS.md` in the server repo.

## Coding Guidelines

Follow the `andrej-karpathy-skills:karpathy-guidelines` skill: think before coding, simplicity first, surgical changes, goal-driven execution.

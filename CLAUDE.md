# CLAUDE.md — Chatter Linux Client

Rust + Slint client. Linux/Windows/Android targets.

## Build & Run

```bash
cargo run -p chatter-ui
```

Use `cargo run` for verification, not `cargo build` / `cargo check` — Slint codegen + linker errors only surface at run.

## Architecture Rules

- **Business logic lives in `chatter-core`, never in `chatter-ui`** — All filtering, data shaping, and conditional logic belongs in `chatter-core` services. The UI crate (`chatter-ui`) is thin presentation. Need different data → add a service method, don't filter in UI.
- Slint files in `ui/`, Rust glue in `crates/chatter-ui/src/bin_ui/`.

## Coding Guidelines

Follow the `andrej-karpathy-skills:karpathy-guidelines` skill: think before coding, simplicity first, surgical changes, goal-driven execution.

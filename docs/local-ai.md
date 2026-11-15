# AI assistant (Jarvis)

Where AI may run, which engine runs it, and where it lives in `chatter-core`.
The module docs cited below are the detailed reference.

## Cargo features

| Feature | Default | Effect |
|---|---|---|
| `jarvis` | on (`chatter-core` and `chatter-ui` defaults) | Jarvis client façade, the virtual AI channel, slash commands |
| `local-ai` | off | Compiles `crate::local_ai` (Ollama / OpenAI-compatible HTTP providers, catalog, hardware probe, indexer) and the in-process mistralrs engine |
| `local-ai-cuda` | off | `local-ai` plus mistralrs CUDA / flash-attn |

`crate::local_ai` is `#[cfg(feature = "local-ai")]` (`src/lib.rs`). A default
build has only the cloud silo (`crate::cloud_ai`).

## Policy: two silos

`crate::ai_policy` (`src/ai_policy/policy.rs`) decides which silo a
conversation may use; neither silo depends on it.

- **Local** silo — on-device or self-hosted: `crate::local_ai`.
- **Cloud** silo — off-device: `crate::cloud_ai`.

`ai_access` rules:

1. Mesh-delivered traffic: no AI (`AiAccess::Denied`).
2. `ConversationType::Channel`: local or cloud (`LocalOrCloud`).
3. Everything else — every DM flavour, anonymous transports, Saved Messages,
   and any future conversation type: local only (`LocalOnly`). Fail-closed by
   the `_` arm.

`ai_policy::channel_ai::resolve` combines that with two settings:

- per-conversation switch `conv.ai.enabled.<id>` (AI is opt-in per
  conversation);
- user-level `ai.silo_preference`: `local-only` (default) or `prefer-cloud`.

Cloud is chosen only when the user prefers it, policy permits it, and the cloud
silo is configured; otherwise it falls back to local if policy allows, else off.
The engine enforces the policy again (`SharedJarvisEngine`,
`src/services/jarvis/engine.rs`).

## Engines

`JarvisEngine` (`src/services/jarvis/engine.rs`):

- `Disabled` — no configured provider; never falls back to a local process.
- `Cloud` — built only from the cloud silo's `CloudSettings`.
- `InProcess` (mistralrs actor, `src/services/ai/local_actor.rs`) and `Http`
  (an `AnyBackend` from `src/local_ai/providers.rs`) — `local-ai` only.

Cloud modes (`CloudMode`, `src/cloud_ai/mod.rs`):

- `direct-anthropic` (default) — the user's own API key
  (`src/cloud_ai/anthropic.rs`; egress pinned to `api.anthropic.com`).
- `server-broker` — routed through the Chatter server on the organization key,
  which sees the prompt to meter it (`src/cloud_ai/broker.rs`).

Cloud configuration lives in `RemoteAIService`
(`src/services/remote_ai_service.rs`), settings keys `cloud_ai.mode`,
`cloud_ai.provider`, `cloud_ai.model`, `cloud_ai.api_key`.

## The AI channel

A client-only virtual channel named `Jarvis (AI)` whose ID is the nil UUID
(`jarvis_channel_id`, `src/client/jarvis.rs`); the same ID is the
`JarvisContext::general()` memory key. `sidebar_channels`
(`src/client/sidebar.rs`) inserts it at the top when
`jarvis_channel_enabled()`, i.e. when the silo preference is `prefer-cloud`.

The AI channel and the context-free prompt always run, following the silo
preference (still clamped by policy). Any other conversation must have
`conv.ai.enabled.<id>` set (`check_ai_allowed_in`, `src/client/jarvis.rs`).

Slash commands (`JarvisCommand`, `src/services/jarvis/commands.rs`): `/ask`,
`/summarize`, `/knowledge`, `/remember`.

Remote context (`ContextBuilder::build_remote`,
`src/services/jarvis/context_builder.rs`) is only the owner's own remote
conversation history: the most recent 60 turns, trimmed from the oldest to
about 32,000 bytes. Stored facts, entities, summaries and local history never
enter a remote prompt.

## Storage

Jarvis tables are in the app's SQLite (`src/storage/migrations/`):
`v031_jarvis_memory` (conversations, facts, summaries), `v032_jarvis_graph`
(entities, relationships, entity mentions), `v049_vector_embeddings`,
`v055_temporal_facts` (`valid_from` / `valid_until` / `superseded_by` on
`jarvis_facts`).

## Tests

- `crates/chatter-core/tests/integration_jarvis.rs`
- Unit tests beside `ai_policy`, `cloud_ai`, `services/jarvis` and
  `client/composer/tests.rs` (`begin_refuses_a_jarvis_request_before_consuming`).

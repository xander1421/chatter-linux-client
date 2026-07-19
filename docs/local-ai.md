# Local AI — End-to-End Guide

How the local AI stack works, how to use it, and how to extend it.
Everything described here runs on the user's machine; no chat content,
prompt, or embedding ever leaves the device.

## The flow at a glance

```
Settings → AI Assistant
  │ 1. detect: probe Ollama :11434, LM Studio :1234, llama.cpp :8080,
  │           vLLM :8000 (+ compiled-in mistralrs)          providers.rs
  │ 2. hardware: arch / cores / RAM (/proc/meminfo) /
  │           GPU VRAM (nvidia-smi, rocm-smi best-effort)   hardware.rs
  │ 3. catalog: curated models tagged Fits GPU / CPU / Too
  │           big for this machine                          catalog.rs
  │ 4. download: Ollama /api/pull, streamed NDJSON progress ollama.rs
  │ 5. pick provider + chat model → persisted, Jarvis
  │           engine hot-swapped                            engine.rs
  ▼
Sidebar → "Jarvis (AI)" channel (virtual, nil-UUID id, local only)
  │ send → JarvisService.ask
  │   ├─ context: recent history + user facts + entity graph
  │   │          + semantic recall over indexed messages    context_builder.rs
  │   ├─ completion via active provider (enum-dispatched)   engine.rs
  │   └─ both sides persisted in jarvis_conversations       SQLite v031
  ▼
Memory grows: entity extraction, co-mention graph, fact decay,
conversation compression (background maintenance)           services/jarvis/
```

## Using it

1. **Run a provider.** Easiest: install [Ollama](https://ollama.com) and
   start it (`ollama serve`, usually auto-started). LM Studio, llama.cpp
   server (`llama-server`), and vLLM are detected too on their default
   ports. No account or token is needed anywhere.
2. **Settings → AI Assistant.** The section probes providers on open.
   Pick a provider, then either select an installed model or download one
   from the catalog (filtered by your hardware — a model tagged
   "Too big" won't download).
3. **Chat.** The **Jarvis (AI)** channel sits at the top of the channel
   list (also `Ctrl+J` or the sidebar Jarvis button). Plain messages go
   straight to the model; slash commands work too:
   - `/ask <q>` — explicit question (also works in any real channel)
   - `/summarize` — summarize the conversation
   - `/knowledge <entity>` — what Jarvis knows about an entity
   - `/remember <fact>` — store a fact
4. **@Jarvis in real channels** is intercepted before send — the message
   never reaches the channel or the server; the answer renders locally.

## Architecture

### Two engines, one dispatch point

- **HTTP providers** — `local_ai/ollama.rs` (native API: chat, embed,
  list, pull, delete) and `local_ai/openai_compat.rs` (one generic
  client for LM Studio / llama.cpp / vLLM: chat, embed, list only).
  Both implement `LocalAIBackend` (`local_ai/backend.rs`); the trait
  uses native `async fn`, so cross-provider dispatch is the
  `AnyBackend` enum (`local_ai/providers.rs`), not `dyn`.
- **In-process** — mistralrs actor (`services/ai/local_actor.rs`),
  compiled only with the `local-ai` cargo feature (CUDA). Without the
  feature it returns stub strings and the settings UI hides it.

`JarvisEngine` (`services/jarvis/engine.rs`) picks one from
`LocalAISettings.active_provider`; `SharedJarvisEngine` is a lock so a
settings change swaps the engine live — no restart.

### The AI channel

Virtual channel, injected by `ChatterClient::sidebar_channels`
(`client/mod.rs`) when local AI is enabled. Its id is the **nil UUID**,
which is also the `JarvisContext::general()` memory key — channel and
memory share one identity by construction. The UI branches on that id:
selection loads history from `jarvis_conversations`
(`features/jarvis/channel_view.rs`), send routes to `jarvis_ask`
(`features/chat/send_message.rs`). It is excluded from auto-select,
read-marks, pins, and every network path.

### Memory

Two complementary stores, both in the app's existing SQLite:

| Store | Tables | Retrieval |
|---|---|---|
| Jarvis memory | `jarvis_conversations`, `jarvis_facts`, `jarvis_summaries` (v031) | recent history, SQL fact search, decay + compression |
| Knowledge graph | `jarvis_entities`, `jarvis_relationships`, `jarvis_entity_mentions` (v032) | entity lookup from capitalized query words |
| Vector index | `vector_embeddings` (v049, f32 BLOBs) | cosine top-K via `SemanticSearchService` |

`ContextBuilder::build_with_semantic` merges all three into the system
prompt. Embeddings come from the active provider (`nomic-embed-text`
by default on Ollama); with no embed-capable provider the semantic
section silently drops out and retrieval stays keyword-based. The
brute-force cosine scan is fine to ~50K vectors; if it ever gets slow,
the upgrade path is sqlite-vec.

Incoming decrypted channel messages are auto-indexed for semantic
recall by `MessageIndexer` (opt-out: `local_ai.auto_index`). AI-channel
conversations are deliberately not vector-indexed — recent history and
facts already cover them.

### Settings & persistence

`SettingsRepository` keys: `local_ai.enabled`, `local_ai.active_provider`,
`local_ai.chat_model`, `local_ai.embedding_model`, `local_ai.ollama_url`.
The Ollama URL is user-overridable; other providers use default ports.

## Extending

- **Add a provider**: if it speaks the OpenAI API, add a `ProviderKind`
  variant + default port in `local_ai/providers.rs` — probe, backend
  construction, and UI list pick it up automatically.
- **Add a catalog model**: one `CatalogModel` row in
  `local_ai/catalog.rs` (tag, params, approx size). Fit tags come free.
- **Gated HuggingFace models** (`ponytail:` deferred): mistralrs loads
  public HF ids tokenless; add an HF token setting only when someone
  actually needs a gated model.

## Validation

- Unit: `cargo test -p chatter-core --lib local_ai jarvis`
- Live e2e against a running Ollama (downloads ~400MB once):
  `cargo test -p chatter-core --lib live_ -- --ignored --nocapture`
  — proves detection, streaming pull progress, and a real chat
  completion through the exact code paths the app uses.
- UI: `cargo run -p chatter-ui` — Jarvis (AI) channel in the sidebar,
  Settings → AI Assistant for detection/catalog/downloads.

## Chatter Mind (2026 memory architecture)

Shipped on top of the base stack:

- **Write path**: `FactExtractor` runs a background LLM pass per exchange —
  grammar-constrained JSON on Ollama (`format` schema + `think:false`), so
  thinking models (qwen3, deepseek-r1) can't ramble past it. Facts flow
  through the consolidating `upsert_fact`: ADD / REINFORCE / SUPERSEDE.
- **Temporal facts** (v055): contradictions never overwrite — the old fact
  keeps `valid_until` + `superseded_by`; `get_fact_history` answers "when
  was X true". Prompts render "(since date)".
- **Hybrid retrieval**: BM25 (existing `messages_fts`) + vector cosine,
  merged by reciprocal rank fusion (k=60). Works keyword-only without an
  embedding model.
- **App-context bridge**: live channel + friend names in the system prompt
  ground "what did alex say" / "summarize general".
- **Thinking-model compatibility**: `<think>` blocks stripped from replies;
  structured tasks (extraction, summaries) disable thinking; budgets sized
  for reasoning spend.

### VALIDATE-AI

`crates/chatter-core/tests/validated_ai.rs` — the empirical suite. Real
Ollama, real DB, real service, zero mocks:

    cargo test -p chatter-core --test validated_ai -- --ignored --nocapture

V-AI-1 detection · V-AI-3 memory round-trip · V-AI-4 multi-turn coherence ·
V-AI-5 LLM write path + consolidation · V-AI-7 app-context grounding.
(V-AI-2 self-heal and V-AI-6 temporal supersede live beside their units in
`engine.rs` / `integration_jarvis.rs`.) Serialized on one Ollama instance.

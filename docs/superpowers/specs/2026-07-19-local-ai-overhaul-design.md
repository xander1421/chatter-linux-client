# Local AI Overhaul — Design

Date: 2026-07-19
Branch: refactor/signal-dm-no-channel (work continues here or a child branch)
Status: Approved (user + ponytail review)

## Goal

Make the local AI stack usable end-to-end: detect what the user already runs,
suggest models that fit their hardware, download them, chat with the local
agent in a dedicated local "AI channel", and give the agent real memory —
validated in both chatter-core and chatter-ui.

## Decisions (user-approved)

1. **Channel mode:** Local AI channel only. A purely client-side virtual
   channel; no server involvement. The Jarvis side panel is **deleted** —
   the AI channel is the single surface. `@Jarvis` mentions in real channels
   stay intercepted pre-send; the answer lands in the AI channel.
2. **Backends: keep both.**
   - HTTP providers, probed at runtime: Ollama (`:11434`, native API),
     LM Studio (`:1234`), llama.cpp server (`:8080`), vLLM (`:8000`) — the
     last three via one generic `OpenAiCompatBackend` implementing the
     existing `LocalAIBackend` trait.
   - mistralrs in-process engine stays, still gated behind the `local-ai`
     cargo feature (CUDA).
3. **Downloads:** Curated static catalog. Ollama models via `/api/pull` with
   `stream:true` NDJSON progress → UI progress bar. mistralrs models via
   public HuggingFace model id (tokenless). LM Studio / llama.cpp / vLLM:
   list-only. **No HF token field** (deferred; `ponytail:` marker in code).
4. **Memory:** Hybrid in existing SQLite. Wire the orphaned
   `SemanticSearchService` (embeddings → `v049_vector_embeddings` BLOBs →
   cosine) into Jarvis `ContextBuilder`, alongside existing facts/entity
   graph. No new DB, no sqlite-vec (upgrade path if >50K vectors).
   Embedding model auto-defaults (`nomic-embed-text` on Ollama, first
   embed-capable model on OpenAI-compat); no UI picker. mistralrs-only
   setup falls back to keyword retrieval (no embed support).
   AI-channel conversations are NOT vector-indexed (recent-history +
   facts cover them; revisit if recall weak).

## Ponytail cuts (deferred, not designed)

- HF token setting for gated models.
- Vector-indexing AI-channel conversations.
- Embedding model UI picker.
- AMD `rocm-smi` probe is best-effort parse, silent skip when absent.

## Architecture

### Feature regating

- HTTP local-AI stack (`local_ai/*`, `HttpLocalAIService`,
  `SemanticSearchService`, `MessageIndexer`) loses its `local-ai` cfg gates —
  always compiled (deps are just reqwest/serde).
- `local-ai` feature gates **only** mistralrs (`services/ai/local_actor.rs`
  real path).
- `jarvis` becomes a **default feature** in chatter-core and chatter-ui.

### New core modules (`crates/chatter-core/src/local_ai/`)

- `openai_compat.rs` — `OpenAiCompatBackend` implements `LocalAIBackend`:
  `chat` → `/v1/chat/completions`, `list_models` → `/v1/models`,
  `embed` → `/v1/embeddings`; `pull_model`/`delete_model` return
  `BackendError::Unsupported`.
- `providers.rs` — `ProviderKind { Ollama, LmStudio, LlamaCpp, Vllm,
  MistralRs }`, `DetectedProvider { kind, url, healthy, models }`,
  `detect_providers()` probes the four ports concurrently (short timeout,
  ~1s) + reports mistralrs availability from the compiled feature.
  Re-probe on settings open.
- `hardware.rs` — `HardwareProfile { arch, cpu_cores, ram_gb, gpus:
  Vec<GpuInfo { vendor, name, vram_gb }> }`. Sources:
  `std::env::consts::ARCH`, `std::thread::available_parallelism`,
  `/proc/meminfo`, `nvidia-smi --query-gpu=name,memory.total
  --format=csv,noheader` parse, best-effort `rocm-smi` parse.
  `ModelFit { Gpu, CpuSlow, TooBig }` heuristic: quantized size ≤ 90% VRAM →
  Gpu; ≤ 90% free-ish RAM → CpuSlow; else TooBig.
- `catalog.rs` — static `CATALOG: &[CatalogModel]` — llama3.2 (1b/3b),
  llama3.1:8b, qwen2.5 (0.5b/1.5b/3b/7b/14b), mistral:7b, phi4,
  gemma3 (1b/4b/12b), deepseek-r1 (7b/14b distills), nomic-embed-text,
  all-minilm. Fields: ollama tag, HF id (where sensible), param count,
  approx download+runtime size per default quant, kind (chat/embed).
  `fit(profile) -> ModelFit` per entry.

### Ollama pull progress

`OllamaBackend::pull_model` switches to `stream:true`; parses NDJSON lines
(`status`, `total`, `completed`) and reports via `mpsc`/callback →
`DownloadProgress { model, completed, total, status }` events consumed by UI.

### Provider selection & Jarvis rewiring

- `LocalAISettings` gains `active_provider: ProviderKind` (+ existing urls,
  chat model). Persisted via existing `SettingsRepository` keys.
- A `dyn LocalAIBackend` resolver returns the active backend
  (Ollama / OpenAI-compat variants). Jarvis `service.rs` swaps its direct
  `LocalAIActorHandle` (mistralrs) dependency for a small
  `JarvisEngine` enum: `Http(Arc<dyn LocalAIBackend>)` or
  `InProcess(LocalAIActorHandle)`; picked from active provider.
- `HttpLocalAIService` becomes the single owner of backend construction;
  embedding generator follows the active provider.

### AI channel (client-side virtual channel)

- chatter-core channel service injects a virtual channel (id constant, e.g.
  `jarvis:local`, display "Jarvis (AI)") into the channel list when local AI
  enabled. Business logic in core per architecture rule.
- Message view for that channel routes send → `client/jarvis.rs::jarvis_ask`
  (existing façade), renders from `jarvis_conversations` history (existing
  migrations v031) — history survives restart.
- Send path: no network, no encryption, never leaves device.
- Delete: `ui/jarvis/` panel components, `features/jarvis/panel.rs`,
  panel state props/callbacks in `main.slint`/`app.slint`,
  `jarvis_global.slint` visibility flag. `mention_handler.rs` retargets
  output to AI channel.

### Memory wiring

`services/jarvis/context_builder.rs` gains optional
`Arc<SemanticSearchService>`; when embeddings available, `build_context`
adds top-K (K=5, min_score 0.3 — existing constants) semantic hits from
message history into the "Relevant Knowledge" prompt section. Keyword
fact search + entity graph retrieval stay.

### Settings UI (`ui/settings/settings_screen.slint` AI section rebuild)

- Hardware summary line (arch, cores, RAM, GPU + VRAM).
- Detected providers list: name, health dot, model count; radio = active.
- Active provider's installed models → chat-model dropdown.
- Catalog list with fit tag (GPU / CPU slow / too big) + download button +
  progress bar (Ollama), "open HF id" for mistralrs.
- Existing mistralrs load/unload controls survive under the provider entry.

### Docs deliverable

`docs/local-ai.md` — e2e agent flow guidelines: detection → catalog →
download → AI channel chat → memory (what's stored where) → extension
points (add provider, add catalog entry).

## Error handling

- Probe failures = provider absent (no error UI, just not listed).
- Pull failures surface in the catalog row (retry button).
- Backend chat errors surface as system-style message in the AI channel.
- No provider detected + no mistralrs → AI channel hidden, settings section
  shows "no local AI providers found" + hint.

## Testing / validation

- Core: unit tests for catalog fit heuristic, provider probe parsing
  (mock HTTP), OpenAI-compat request/response shapes, NDJSON pull-progress
  parser, context builder with fake semantic hits. `GOEXPERIMENT` n/a (Rust).
  `cargo test -p chatter-core`.
- UI: `cargo run -p chatter-ui` (repo rule: run, not build/check) with
  `SLINT_EMIT_DEBUG_INFO=1`, TMPDIR off tmpfs; wiring proofs for new
  callbacks (send-in-AI-channel, download, provider select).
- Loop until both green.

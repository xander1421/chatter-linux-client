# Chatter Linux client codebase review

The client has a useful core façade and separate Slint crate, but UI and core responsibilities are only partly separated. The main problems are an accessible implementation API, desktop interactions embedded in core, presentation models embedded in core, and application workflows still implemented in UI callbacks. Incremental extraction is appropriate; a rewrite would discard working seams and regression coverage.

The first fix should be the Jarvis conversation-context mismatch, followed by asynchronous result ownership and enforceable boundaries. Large-file cleanup should follow these changes rather than drive them.

## Scope and baseline

Reviewed on 2026-10-18 against client commit `158eb7858cf93304f984082ee3b29e44044d6b5e` and parent repository commit `69e123b627ad909e4d1c469b2cc394c20f7154e1`. Three focused workers reviewed core boundaries, UI workflows, and tests/CI; their findings were checked against source.

The workspace contains eleven crates. Under `src/`, excluding the core `proto/` directory, core contains 588 Rust files and 275,043 lines; UI contains 169 Rust files and 51,482 lines. These counts include comments and embedded unit tests. There are 124 Slint files under `ui/`. This is a substantial cross-platform client, not a small Linux-only frontend.

This is an architecture and maintainability audit, not a complete security audit or runtime certification. No production code was changed. Pre-existing parent `go.sum` changes and three untracked UI test files were left untouched.

## What is already working structurally

- Core does not depend on Slint. Generated components live in `chatter-ui-slint`, with callback wiring in `chatter-ui`.
- `ChatterClient` provides an application façade, and service accessors default to crate-private visibility outside the testing feature: [core lib.rs](../../crates/chatter-core/src/lib.rs#L13).
- Notification injection already provides a model for moving platform implementations outward: [client builder](../../crates/chatter-core/src/client/mod.rs#L169).
- Typed conversation handles, composer APIs, domain events, and existing UI request ownership helpers provide migration seams. Preserve them.

## Findings and fixes

### 1 Jarvis slash commands lose the conversation kind

**Priority: P1 — correctness.** The slash-command path captures only `active_channel_id`, then constructs `JarvisContext::channel` whenever that ID exists: [send_message.rs lines 460 and 1086](../../crates/chatter-ui/src/bin_ui/features/chat/send_message.rs#L460). Core's `ActiveConversation::channel_id` supplies IDs for server DMs and anonymous conversations too: [chat_state.rs line 62](../../crates/chatter-core/src/state/chat_state.rs#L62). Consequently, those conversations are classified as channels by this path. The fallback constructs a nil DM identifier when no ID exists.

Core's mention handler already distinguishes channel, server DM, anonymous DM, and anonymous group: [client/jarvis.rs line 273](../../crates/chatter-core/src/client/jarvis.rs#L273). The two paths disagree about the input to conversation history and AI policy. This is established by source; incorrect responses or data disclosure were not reproduced.

**Fix:** Share one core context resolver and route slash commands through a core command accepting the typed conversation and session owner. Reject an absent conversation explicitly. Test every conversation kind and ensure distinct conversations retain distinct context keys. Include `/summarize`, which depends on correct history scope.

### 2 Jarvis completion lacks explicit ownership and blocks an async worker

**Priority: P1 for ownership review; P2 for synchronization.** The handler queues a thinking placeholder, waits using blocking `std::sync::mpsc::Receiver::recv`, then awaits execution and queues a response without an explicit session/conversation revision check: [send_message.rs lines 1093 through 1134](../../crates/chatter-ui/src/bin_ui/features/chat/send_message.rs#L1093).

The blocking receive occupies a Tokio worker until the UI closure runs. The completion may run after navigation or logout; the handler itself does not establish that its publication still belongs to the current view. Existing history helpers show how ownership can be represented: [history_owner.rs](../../crates/chatter-ui/src/bin_ui/features/history_owner.rs#L1). A stale-result incident was not reproduced.

**Fix:** Capture the session and conversation before dispatch. Core must fence account-bound work; the presenter must fence view publication and clear thinking state for the matching request. Use an awaited Tokio oneshot or create the placeholder before spawning work. Verify delayed completion after conversation switch, logout, and window teardown.

### 3 The core façade is bypassable

**Priority: P1 — architectural enforcement.** Modules labeled internal remain `pub`, including services, storage, network, and protobuf definitions: [core lib.rs line 54](../../crates/chatter-core/src/lib.rs#L54). `#[doc(hidden)]` changes documentation visibility, not access control. Production UI exposes a storage-defined `PeerBinding` type and reads Tor status directly from the network module: [mesh/core_api.rs line 193](../../crates/chatter-ui/src/bin_ui/features/mesh/core_api.rs#L193), [UI lib.rs line 865](../../crates/chatter-ui/src/lib.rs#L865).

These examples are implementation coupling, not evidence that UI performs raw SQL or RPC operations. Some service types are legitimately part of today's public signatures; migrating their import paths alone would not establish a clean API.

**Fix:** Inventory production imports and public signatures. Publish supported DTOs, errors, snapshots, and capabilities through a deliberate API surface. Migrate callers, then make implementation modules crate-private. Give integration tests explicit fixture access without widening production access. Add a dependency/import gate with a temporary, shrinking allowlist for existing exceptions; compile a production consumer without `testing` to verify visibility.

### 4 Core cannot shed desktop interaction dependencies

**Priority: P2 — portability and build isolation.** Linux core includes clipboard, GTK file dialogs, notifications, tray, and camera dependencies even when default features are disabled: [core Cargo.toml lines 246 through 272](../../crates/chatter-core/Cargo.toml#L246). `desktop-system` gates keyring stores, while the desktop modules are predominantly gated by operating system: [system/mod.rs](../../crates/chatter-core/src/system/mod.rs#L12).

**Fix:** Move file dialogs, clipboard, tray, external opening, and native notification implementations into a desktop platform adapter or the existing platform layer. Inject capabilities into application services where needed. Preserve core notification policy, credential-storage contracts, and media/session rules. As an intermediate step, make native interactions truly optional under a dedicated feature. Keep media/device extraction separate so call behavior is preserved.

Acceptance requires a minimal core dependency graph without GTK dialogs, clipboard, or tray crates, plus a runnable desktop client with the adapters installed. Absence of Slint alone is insufficient.

### 5 Core owns display formatting while UI owns application workflows

**Priority: P2 — responsibility placement.** Core row models contain formatted timestamps, initials, display strings, and decoded RGBA avatars: [sidebar_data.rs lines 1 through 40 and 91](../../crates/chatter-core/src/ui/sidebar_data.rs#L1). Core also owns English relative dates and local-time rendering: [chat_data.rs line 24](../../crates/chatter-core/src/ui/chat_data.rs#L24).

In the opposite direction, UI implements batch deletion sequencing, session checks, and partial completion: [chat/delete.rs line 209](../../crates/chatter-ui/src/bin_ui/features/chat/delete.rs#L209). Core owns authorization and persistence underneath, but the multi-step application operation remains frontend-specific.

**Fix:** Distinguish domain policy, application use cases, and presentation. Keep permissions, privacy routing, unread semantics, persistence, sorting rules, and operation ownership in core. Put locale formatting, avatar decoding for display, Slint conversion, placeholder lifecycle, and view publication in presenters. Move batch deletion into a core operation returning typed progress and partial-failure results.

The current rule in [CLAUDE.md line 44](../../CLAUDE.md#L44), which places all filtering, shaping, and conditional logic in core, is too broad for a useful separation: rendering necessarily contains conditionals. Clarify it around business decisions and invariants. Presentation extraction can initially remain a module inside `chatter-ui`; another crate is optional.

### 6 UI can mutate shared application state directly

**Priority: P2 — invariant ownership.** `ChatterClient` exposes full mutable state stores: [client/mod.rs line 2521](../../crates/chatter-core/src/client/mod.rs#L2521). UI directly logs out auth state after account deletion: [settings.rs line 812](../../crates/chatter-ui/src/bin_ui/features/modals/settings.rs#L812). Conversation selection also remains accessible through state setters.

**Fix:** Give UI read-only snapshots/subscriptions and explicit commands for account/session transitions and conversation selection. Make each command responsible for its related drafts, attachments, subscriptions, and session revision changes. Preserve UI-local state in the presenter. This is an architectural risk; these call sites alone do not establish a broken transition.

### 7 CI does not execute the full offline regression coverage

**Priority: P1 — confidence in future refactors.** Parent CI checks workspace compilation but executes only the UI library and `ui_wiring` suites in its client test steps. Core behavior tests and other UI regression targets are outside those commands. Clippy is non-blocking: [ci.yml lines 246 through 278](../../../.github/workflows/ci.yml#L246). The local [Makefile test-rust target](../../../Makefile#L428) defines broader workspace and feature-specific runs.

**Fix:** Bring a bounded offline core suite and selected behavioral UI regressions into required CI, then expand toward the existing local matrix. Keep live-server suites separate. Run the static wiring and development-credential checks plus their fixtures in CI. Baseline Clippy debt and make newly introduced warnings blocking before making the whole lint job required. Existing textual wiring tests remain useful backstops, but they cannot prove behavior or architecture boundaries.

### 8 Large modules and global presentation state raise maintenance costs

**Priority: P3 — maintainability.** Core `client/mod.rs` is 6,976 lines and `services/call_worker.rs` is 6,221 lines. UI channel settings is 2,340 lines. File size includes tests and is a review signal, not proof of poor design. History request generations are process-global atomics: [history_owner.rs line 9](../../crates/chatter-ui/src/bin_ui/features/history_owner.rs#L9).

**Fix:** Split by coherent operations after responsibility placement is clear. Keep the façade small by delegating to application services. Collect view state and cancellation ownership into window-scoped presenters as touched. Verify teardown and test isolation. Avoid arbitrary line-count limits or splitting files without reducing coupling.

## Recommended target structure

| Layer | Responsibilities | Allowed dependencies |
| --- | --- | --- |
| Domain | Conversation identities, permission and privacy rules, value types | No UI toolkit or native interaction adapters |
| Core application | Commands, session ownership, persistence orchestration, typed events and snapshots | Domain and infrastructure contracts |
| Infrastructure and platform | Network/storage implementations, native dialogs, notifications, clipboard, media devices | Contracts and platform libraries |
| Presentation | View models, formatting, image preparation, view request ownership | Supported application API |
| Slint UI | Layout, interaction, binding and rendering | Presentation and generated components |

These are responsibility boundaries; they do not require five new crates. Keep the existing workspace and extract only where dependency isolation benefits justify it. Inject platform implementations at startup.

## Fix order and completion criteria

1. **Correct Jarvis context and ownership.** One core context resolver, no nil placeholder identity, asynchronous placeholder synchronization, and tests for each conversation type plus delayed completion after logout/navigation.
2. **Establish refactor gates.** Required offline behavior tests, static checks, a production consumer compiled without `testing`, and an import/dependency allowlist that rejects new boundary violations.
3. **Extract desktop interactions.** Minimal core no longer pulls dialog/clipboard/tray implementations. Verify desktop startup and relevant interactions after adapter installation.
4. **Consolidate use cases and restrict state mutation.** Batch deletion and session transitions execute through core commands; presenters consume results and read-only snapshots.
5. **Extract presentation and tighten visibility.** Move formatting/decoded images outward, migrate public DTOs, make implementation modules private, and remove temporary boundary exceptions.
6. **Reduce hotspots as touched.** Split coherent responsibilities and scope view state to its owner. Retain behavior and avoid unrelated renames during extraction.

Each step should be independently reviewable and preserve current behavior except for explicitly covered bug fixes. Add behavioral tests for invariants rather than tests that merely mirror helper structure.

## Verification performed

- `python3 -m unittest discover -s scripts/tests -p 'test_ui_wiring_lint.py'` from the client: **17 tests passed**.
- `python3 scripts/ui-wiring-lint.py`: **passed**; 303 callbacks, zero reported missing handlers, stubs, dead callbacks, unused imports, or touch gaps. Its 120 local-only handlers require interpretation, not automatic migration to core.
- `python3 scripts/dev-gate-lint.py`: **passed**.
- `git diff --check` in parent and client: **passed** before adding this report; repeated for delivery.

No Rust compilation, full formatting scan, runtime UI exercise, live-server suite, or dependency vulnerability scan was performed. Passing these static checks does not establish build or runtime health. Hindsight was unavailable; findings above derive from source and repository configuration.

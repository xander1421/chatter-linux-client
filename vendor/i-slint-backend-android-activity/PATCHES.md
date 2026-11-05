# Chatter patches to i-slint-backend-android-activity 1.18.1

Based on the exact public 1.18.1 crate, upgraded by three-way merging the
1.17.1 fork. Keep upstream licensing files.

- `accessibility.rs`, `androidwindowadapter.rs`, `javahelper.rs`, and
  `java/SlintAndroidJavaHelper.java`: a first Android virtual-view bridge for
  Slint's accessible tree. The native input host provides nodes on demand
  when an accessibility service is enabled; JNI snapshots run on the Slint
  event thread, and Android click actions invoke the item's default action.
  `docs/ANDROID-ACCESSIBILITY.md` describes scope and remaining work.
  The host node returned by `SlintNodeProvider` must explicitly report the
  laid-out input view's parent and screen bounds, visibility, and enabled state.
  `AccessibilityNodeInfo.obtain(host)` only supplies the node's source; leaving
  those fields at their defaults made Android report an invisible 0x0 host and
  skip every virtual Slint child even while an accessibility service was on.
  The input view remains full-size, touch-transparent, and non-focusable on the
  GameActivity path; its IME and insets behavior is unchanged.
  Android snapshots select the last visible `accessible-id: "modal-overlay"`
  group in accessible tree order and serialize only that subtree. The ID is
  metadata, not a spoken label. Menu roots and `ModalContainer` set it while
  logically open; root tab screens opt out so their bottom navigation remains
  reachable. The Java provider's host node remains the tree root.
  Click dispatch also checks current modal membership, so a stale cached
  virtual-view ID cannot activate content behind a newly opened overlay.
  `accessibility.rs` finds each item's stable id in a map keyed by item-tree
  address and item index (`vtable` dependency, same version as i-slint-core),
  checked against the stored weak in case a freed tree's address is reused.
  Scanning the ever-growing id list made each new node cost the whole list;
  after a few emoji-grid rebuilds the S9 snapshot overran `query`'s 500 ms
  and Android received an empty tree.
  `query` waits up to 500 ms for a snapshot and otherwise returns the last
  finished tree (a late snapshot replaces it for the next query); only one
  snapshot runs at a time. Snapshots over 200 ms are logged (tag SlintA11y).
  Without this a large screen (an emoji tab, ~700 nodes, O0 generated code
  on the S9) still timed out to "[]".
  Snapshots skip items clipped out of view (and their subtrees); only the
  root is always reported. An emoji tab's offscreen cells made each snapshot
  ~1.2 s on the S9, stalling the UI thread on every TalkBack query.
  A Text node whose label equals its parent's label (a button's own visible
  text) is dropped, so TalkBack stops on the button once, as on a native
  Button.
  Each node reports `focusable`: a labelled node inside a clickable,
  labelled ancestor is not (the Java provider reads the flag), so a chat row
  is one TalkBack stop rather than one per text inside it.
  Android's ACTION_LONG_CLICK runs Slint's Expand action
  (`accessibilityLongClick`), so items with `accessible-action-expand`
  (chat rows → their menu) are reachable with TalkBack's double tap and
  hold.
  Text inputs carry their `value` (as the node text) and their label or
  placeholder (as the hint), like a native EditText.
  The provider caches a snapshot for 100 ms from when it is READY, so the
  per-node calls of one query share it even when a snapshot takes longer.
  A node with zero width or height is reported not visible to the user:
  banners that collapse by animating height to 0 stay `is_visible()` in Slint
  and were otherwise read out by TalkBack while hidden.
  Each Flickable that sits between an accessible node and its accessible
  parent and can move is reported once as a scrollable node (class
  ScrollView) offering ACTION_SCROLL_FORWARD/BACKWARD; the rows under it are
  its children. `accessibilityScroll` moves its viewport 80% of a page on
  the vertical axis if that scrolls, else the horizontal one, and the
  provider sends TYPE_VIEW_SCROLLED. Slint exposes no Flickable to
  accessibility, so with off-screen rows dropped TalkBack had nothing to
  scroll and anything past the first screen of a list was unreachable.

- `androidwindowadapter.rs` (game-activity path): IME state handling — see the
  `[patch.crates-io]` comment in the workspace `Cargo.toml`.
- `text_input_offsets.rs` / `androidwindowadapter.rs`: convert GameTextInput's
  Java UTF-16 selection/composition offsets to UTF-8 before inbound echo guards
  and core dispatch, and convert every outbound state back to UTF-16. The
  locked android-activity 0.6.1 decodes string encoding but forwards span
  integers unchanged. Keeping cached state in UTF-8 avoids Cyrillic/emoji
  caret drift and mismatched self echoes without changing IME refresh timing.
  Host regression: `python3 tests/check_game_text_offsets.py` (rustc only).
- `androidwindowadapter.rs` (game-activity): a live field's Disable detaches
  its IME client and deferred updates without publishing an empty full-text
  replacement. A SAF picker / pause is focus loss, not draft deletion; a queued
  empty reset could otherwise replace that field after resume. Detached Updates
  and inbound reports are ignored, and the next Enable starts a fresh session
  with its owner's actual text/selection. Destroyed-field cleanup stays separate.
  Host production-boundary regression: `python3 tests/check_picker_focus_lifecycle.py`
  (rustc only); does not claim OEM IME/device coverage.
- `androidwindowadapter.rs`: preserve each Android KeyEvent's meta-state while
  dispatching into core, whose modifier state comes from individual key
  transitions (setting `KeyEvent.modifiers` alone is overwritten). This covers
  IMEs such as Gboard sending Ctrl+A/C/X/V without separate Ctrl transitions;
  aggregate and left/right-only bits are accepted. Dedicated Android Copy,
  Cut and Paste keys use the same core shortcuts. One-shot modifiers are scoped
  to that key; explicit held modifiers are restored and cleared on LostFocus.
  Core retains responsibility for selection, preedit, read-only checks and
  Unicode clipboard insertion; the GameTextInput full-state/UTF boundary is
  unchanged. No clipboard snapshot or background persistence is introduced.
- `java/SlintAndroidJavaHelper.java`: native clipboard reads accept Android's
  `coerceToText` result and safely handle absent, empty and null text clips.
  Host production/native regression:
  `python3 tests/check_native_key_commands.py` (rustc + javac only). It extracts
  the actual native mapping/dispatch, core modifier transitions and shortcut
  classifier, and Java clipboard accessor into recording API seams. Synthetic
  clipboard fixtures only; this does not certify OEM IME, JNI or live-device
  clipboard/selection behavior.
- **Remaining lifecycle limit:** GameTextInput `TextEvent(TextInputState)` in
  locked android-activity 0.6.1 carries text/spans but no originating client or
  session generation. The existing detached-input guard covers Disable while
  unowned; after re-Enable it cannot identify an old report. A backend-local
  generation cannot label that inbound report, so delayed post-refocus reports
  are still unproven and no cross-field/password isolation claim is made.
- `lib.rs` `set_frame_presented_callback` / `androidwindowadapter.rs`
  `do_render`: an observational callback runs only after successful rendering
  and surface submission, without registering a raw-GL Skia rendering notifier.
  Chatter uses it for first-frame launch-cover removal and recurring conversation
  frame timing/tasks. The launch-cover JNI call remains one-shot; missing surfaces,
  skipped frames and render failures leave it pending, and surface recreation
  retains the observer. Re-registering replaces the observer (for example when
  moving from a startup error window into the app). The callback observes
  submission, not display scanout; Kotlin's launch-cover timeout remains intact.
  This avoids the notifier's extra pre-render flush and its unconditional
  disabling of partial rendering, without changing real raw-GL notifier behavior.
- `lib.rs` `run_event_loop` / `vsync.rs`: backport of Slint 1.18.1's
  dedicated native Choreographer wakeup driver (upstream PR #12702, commit
  `1cd1059c6b8a54fb66e33bc3166420b7e247eb6c`). It replaces normal frame-floor
  scheduling; see the native review and device validation entry below.
- `lib.rs` `with_refresh_rate_vote` / `androidwindowadapter.rs` `last_motion`:
  the app votes the panel's top rate only while the UI moves. The loop calls
  the vote with `true` when an animation runs or a touch moves, and `false`
  after 400 ms of neither (it wakes itself for that), on transitions only.
  chatter-ui wires it to `MainActivity.setUiMoving`, which sets/clears
  `Surface.setFrameRate`; native callbacks follow the actual display rate.

On upgrade, re-apply all of these and recheck a fling's frame times on a 120 Hz device
(`adb shell dumpsys SurfaceFlinger --latency '<SurfaceView layer>'`; the frames
are GL on a SurfaceView, so `dumpsys gfxinfo` does not see them).

- Native correction after `60d79d0d`: clipboard read failures log only the fixed
  `Clipboard read failed` marker, never the exception/cause/provider details.
  Right Alt snapshots (including aggregate ALT_ON + ALT_RIGHT_ON) emit AltGr
  without inventing left Alt; core's existing Android public `alt` flag excludes
  AltGr, preserving physical AltRight semantics. Explicit left + right retains both.
  `check_native_key_commands.py` asserts exact SelectAll/Copy/Cut/Paste variants,
  one press operation and no release operation at a recording shortcut seam,
  held modifiers on both sides, extracted LostFocus cleanup and core reset, and
  throwing synthetic provider stderr privacy. This seam does not execute TextInput
  editing or full Window focus routing.
  `python3 tests/check_native_key_commands.py --baseline` extracts real pre-fix
  `60d79d0d^` mapper/plain dispatch and Java accessor; both compile with compatible
  API mocks and must fail named Ctrl+V/dedicatedPaste and null-direct/nonempty-
  coercion assertions. `--clipboard-ref 60d79d0d` is a privacy negative control
  expected to fail the throwing-provider assertion. `--evidence-dir PATH` retains
  generated sources, executables/classes and each command's output.

- RC81 incoming SAF-return geometry recovery: `InsetsRecovery.java` owns the
  focused helper lifecycle and animation identities. Focus return, animation end,
  layout, and native focus/resize requests pull current root-window insets and
  full WindowMetrics bounds immediately and after layout. Retired animations and
  old helpers cannot replay into the current owner. JNI publications carry a
  process-wide monotonic sequence; `inset_dispatch.rs` drops superseded queued
  frames before updating Slint. API 30+ publishes canonical full window size;
  the older global-layout approximation keeps its previous size policy. Real
  visible IME and system-bar insets are preserved. No composer padding, draft,
  attachment, or input-focus policy changed.
  Run the production lifecycle policy without Android using `javac -d OUT
  java/InsetsRecovery.java tests/InsetsRecoveryTest.java` and `java -cp OUT
  InsetsRecoveryTest`. The focused `chatter-ui` integration test
  `ui_inset_recovery` uses the production queue fence and keyboard conversion
  against the actual phone layout, including staged SAF draft navigation and
  independent stale keyboard/property and reduced viewport recovery.

- RC88 accepted-send consumption vs. the IME's copy of the field
  (`androidwindowadapter.rs` `TextInputBridge` / `consume_text_input` /
  `handle_text_input_state` / `handle_text_input_ack`, `lib.rs`
  `with_text_input_bridge`, `consume_text_input`, `deliver_text_input_state`,
  `deliver_text_input_ack`). Core publishes a programmatic text change only at
  the next render (`i-slint-core` `items/text.rs` `render`), android-activity
  0.6.1 samples one latest-state text snapshot per input pass after that pass's
  motions, GameTextInput's `InputConnection.setState` never fires its listener
  (only `endBatchEdit` and `onKey` do, games-activity 4.4.0 bytecode) and
  android-activity's own `setState` travels the native work pipe while anything
  the app posts is a Handler message, so the two could not be ordered. A send
  tap therefore cleared the field and then received the IME's snapshot of the
  sent text (composing), which refilled it; the render saw nothing to publish
  and the IME kept composing the sent text (Galaxy S9 / Gboard, caption +
  attachment and a plain send alike). Now the app binds a bridge to its
  producer gate (`com.google.androidgamesdk.ChatterTextInputGate`, a
  GameTextInput `InputConnection` subclass that `GatedSurfaceView` installs
  while the surface view is constructed, before `GameActivity.onCreate`
  registers `mSurfaceView.mInputConnection` with native code, so native
  show/hide/restart and the `softKeyboardActive` flag that
  `onCreateInputConnection` consults are the gate's). Every outbound state goes
  through `TextInputBridge::push` with the owner (a monotonic number per
  `ime_client`) and generation it was computed under; an accepted send
  (`consume_text_input`, called by the app right after it clears the field; a
  no-op unless the IME's client is that now-empty multi-line field — the
  composer, never a single-line search box) bumps the generation, marks the
  reset pending and publishes an immutable pending reset through
  `TextInputBridge::consume`. Both publications put an immutable request into
  a slot under ONE Java monitor and return (the latest push replaces an
  unclaimed one); on the UI thread every mutating IME operation, the
  connection's creation (`onCreateInputConnection`) and a posted fallback take
  the same monitor, claim what is pending — the push first, which establishes
  the owner, then the reset (empty state applied, GameActivity's native copy
  refreshed through `stateChanged`, generation acknowledged to the backend
  with no text) — and only then run the operation, whose own snapshot
  publication (`endBatchEdit`/`onKey`) happens inside the same hold. A request
  is therefore claimed before an operation or published after it completed;
  nothing is replayed. The native thread's wait is bounded by one IME
  operation and never by the UI event loop; the monitor cannot deadlock
  because the native thread reaches the bridge only from Slint callbacks and
  by then holds no lock the UI thread needs (0.6.1 `lib.rs
  input_events_iter` drops the inner read guard before building the iterator,
  `game_activity/mod.rs InputIteratorInner::from` ends `with_locked_app`
  before any callback and the C `android_app_swap_input_buffers` unlocks
  before returning, `InputIteratorInner::next` holds no lock around the
  callbacks, `GameTextInput::getState` copies under `currentStateMutex_` and
  releases it before dispatch), while the UI thread under the monitor only
  enqueues to the Slint event loop (`invoke_from_event_loop` pushes and
  wakes), briefly takes `currentStateMutex_` and the `android_app` mutex in
  `onTextInputEventNative` (no callbacks under them) and makes the IMM
  `restartInput` binder call. For the first state of a field to be the one
  the IME attaches to, Enable now publishes the state BEFORE
  `show_soft_input` (which travels the work pipe; a push is a Handler message
  plus the synchronous slot). Owners and generations are monotonic on both
  sides: a push for an older field is ignored, a reset for a newer field
  waits for that field's first push, a push older than the pending reset of
  its field claims the reset and is dropped, a push newer than it supersedes
  and acknowledges it. Every snapshot the IME produces is tagged with the
  owner and generation applied when it was made; the backend drops it only by
  that provenance, never by content, and `finish_composing` replays nothing
  while the reset is unacknowledged. Activity replacement inside one process
  does not occur (`android_main` ends with `_exit(0)`, chatter-ui `lib.rs`),
  so no cross-process session token is carried; a gate that is not the
  installed one claims and publishes nothing regardless. With a bridge bound
  android-activity's `TextEvent` is a wake-up only; without one the old path
  is unchanged. Limit: an IME operation that completed before the accepted
  send's publication is part of the consumed text even if its snapshot
  reaches the backend after the consumption (dropped by generation); one that
  starts after it runs on the new generation as the IME formed it; no timer,
  focus change or content rule second-guesses either.
  RC89 follow-up: a restore after the consumption (the draft an edit held
  aside, a failed send's text) is a new app state of the field and takes the
  next generation, pushed at once by `restore_text_input` rather than at the
  next render; every gated IME operation publishes a snapshot (GameTextInput
  4.4.0 `endBatchEdit` runs `stateUpdated` unconditionally), so a snapshot
  of the Editable the reset emptied, tagged with the consumed generation,
  landed after the restore as a current state and replaced the restored
  text with nothing. The gate applies the restore push in place of a still
  unacknowledged reset (newer generation) and drops the reset's own
  snapshots by provenance; `TextInputBridge::push` now reports whether the
  gate was reached so the generation is not advanced past a push it never
  saw.
  RC89 entry: the same without a consumption. Opening an edit replaces the
  draft in a composer that kept its IME client through the message menu;
  the render's push of the message text carried the generation the IME
  still reported under, so a snapshot of the old draft produced before
  that push applied and landing after the replacement put the draft back
  under the edit banner (S9/FUTO: Edit then typing at once extended the
  old draft). Edit entry calls `restore_text_input` right after the text
  is set, like the restores: new generation, pushed now. The restore puts
  the caret at the end of the new text on both sides itself (a keystroke
  between a push of the old clamped caret and the composer's next-frame
  move to the end would land mid-text under the new generation), and takes
  an empty text too, so an app clear without a consumption (Cancel with an
  empty stash) is fenced the same way.
  RC89 publication: a consume or restore whose JNI call never reached the
  gate used to roll its generation back while the echo guards already
  described the new state: the render's Update was then an echo (never
  pushed), and a snapshot of the old text, current again, replaced the new
  text in Slint. Now the generation stays (the field changed whether or
  not the gate heard; old snapshots stay dropped), `publish_pending` is set
  at the one boundary (`set_game_text_input_state`), the next outbound
  Update bypasses every dedupe guard, the next frame (`do_render`) or
  input wake (`process_inputs`) re-publishes the field's current text and
  caret regardless (Slint raises an Update only while the field differs
  from its own last published text, so one failed retry would otherwise
  be the last), and a push that reaches the gate clears the flag (and
  `reset_pending`, since it stands in for the reset the gate never got).
  A change of IME client retires it. No timer, no redraw request, no
  content rule.

- C4 (2026-10-11), `javahelper.rs` / `text_input_offsets.rs`: NativeActivity
  preserves the real preedit-relative anchor/caret instead of selecting the
  whole composition. Paired with sharedparley's selection-only rendering.
  Reversed composing spans are treated as committed text, avoiding invalid
  slicing. Host coverage lives in `check_game_text_offsets.py`, whose extraction
  boundary and recording seam now match the bridge-aware production method.
  `check_picker_focus_lifecycle.py` now extracts the relocated inbound guard and
  models owner epochs; its focus-loss and field-switch regressions pass again.

- C3 (2026-10-11), `text_input_publication.rs` / `androidwindowadapter.rs`:
  failed-publication and reset flags now live in a JNI-independent module,
  included directly by the host UI wiring tests. The existing consume, push,
  acknowledgement, restore and field-retirement transitions are unchanged;
  failure/retry recovery can be injected on the host without an Android window.


- Earlier S9 wakeable frame-floor baseline (2026-11-03), superseded by the
  native vsync backport below, `lib.rs` `run_event_loop`:
  replace post-render `thread::sleep` with a monotonic next-render deadline in
  the Android poll timeout. Input/lifecycle wakes dispatch immediately while
  rendering alone waits; pending redraw is consumed only when rendering.
  Keep the existing render-start floor (4 ms until the refresh vote supplies
  half a vsync, doubled for skipped frames); elapsed render/swap time counts
  toward it. Animation polling honors the same deadline instead of forcing a
  zero timeout, and positive render waits round up to Android's millisecond
  poll resolution to avoid a sub-millisecond busy loop. The initial frame has
  no deadline, InitWindow clears a predecessor surface's deadline, and
  synchronous RedrawNeeded still renders immediately. Existing observer and
  refresh-rate voting behavior remain intact; this does not remove blocking
  inside rendering/swap or establish a buffer queue depth limit.
  Static-name, allocation-free libandroid ATrace RAII scopes cover poll
  (including its wait and callbacks), InputAvailable dispatch, ordinary
  render/submission, and synchronous redraw dispatch. They check
  ATrace_isEnabled and require Android API 23+ (Chatter minSdk is 26); no
  new dependency or required API 29 symbol is used. Optional motion-age
  counters resolve API 29 `ATrace_setCounter` dynamically; API 26–28 retain
  timing scopes. Native Down/Move/Up dispatch age uses source event timestamps
  and CLOCK_MONOTONIC, without changing the input or animation clocks or
  recording coordinates or message data. Frame observers are
  included in render scopes, so these durations are not GPU completion or
  physical scanout timestamps.

- Native vsync backport (2026-11-03): `vsync.rs` retains upstream licensing and
  the released 1.18.1 helper implementation, adding only fixed-name ATrace
  wait/wake scopes. Slint UI/rendering stays on android_main; the helper owns
  its own looper and only wakes that thread. No frame-permit gate is added.
  The loop retains Chatter's first-terminal-outcome handling, final animation
  invalidation, synchronous redraw, surface guards and successful-submission
  observer. Failed submission alone retains a 16 ms retry backoff. Normal
  pending redraws are immediate; native vsync drives animations with upstream's
  10 ms startup/unavailable fallback. `tests/native-vsync` exercises the real
  S9 native helper with a recording waker; full client timing remains a separate
  check. Review: `docs/audit/reports/slint-android-native-review-2026-11-03.md` in
  the superproject. No content/pixel cache or Swappy dependency is introduced.


## 1.18.1 upgrade (2026-11-04)

- Resolved 20 three-way conflict blocks across `lib.rs`,
  `androidwindowadapter.rs`, `javahelper.rs`, and the Java helper. The local
  accessibility/modal fence, clipboard coercion and fixed-message failure log,
  IME ownership/generations/consume/restore/publication retry, UTF-16 boundaries,
  selection menu, inset lifecycle and queued-inset fence remain in place.
- `AndroidPlatform` now retains upstream's weak `SlintContext` binding and
  upgrades that owning context for the event loop's timer queries and updates.
  Chatter's first-terminal-outcome rule still notifies the lifecycle listener
  for every poll event, including Pause (the app's image-cache purge and draft
  checkpoint live in that listener).
- Upstream made `WindowInner::process_key_input` and `process_touch_input`
  crate-private. All native, synthetic-modifier and composition input now uses
  `WindowEvent` dispatch; native key acceptance uses
  `Result<WindowEventDispatchResult, PlatformError>`. One-shot modifier
  bracketing/restoration, held-key cleanup on LostFocus, and IME action Return
  pairs remain. Inset resize dispatch uses the same new result API.
- Visual hit testing now returns `(byte_offset, affinity)`. The local long-press
  word-selection path preserves that affinity with
  `set_cursor_position_with_affinity`, as upstream does for dragged handles.
  Programmatic restore-to-end still uses the ordinary cursor setter.
- Native vsync and immediate pending-redraw wakeup are now upstream features.
  `vsync.rs` is identical to the pre-upgrade backport, with only Chatter's
  fixed-name ATrace wait/wake scopes relative to public 1.18.1. Local surface
  guards, final-animation invalidation, synchronous redraw, retry-only 16 ms
  backoff, millisecond ceiling, 400 ms refresh vote, and successful-submission
  observer remain. No frame permit, raw-GL notifier, or content cache was added.
- Kept upstream's native IME hint flags, incremental Editable/selection updates,
  clip-edge handle correction, API 34+ Back callback/fallback, and unconsumed
  system-bar/display-cutout safe-area handling. These coexist with the local
  InsetsRecovery lifecycle/sequence fence and native preedit selection fix.
- Renderer construction follows upstream's extra `set_window_handle(..., false)`
  argument and `unstable-wgpu-29` / `unstable-wgpu-30` features (28 was removed).
  Existing Chatter Android platform/bridge/observer entry points are unchanged.
- Host seams were adapted to routed key results, virtual-keyboard clearing on
  Disable, and the app connection's existing privacy/IME-hide state. Verified:
  offset tests (8), native key tests (5) plus Java clipboard checks, picker
  lifecycle tests (2), InsetsRecovery, SAF copy retention and selection restart.
  The complete Java helper compiles against Android API 36 with Java 8 source
  and target. Rust modules parse with rustfmt. Integration Rust/Android builds,
  real IME/accessibility/Back behavior and display timing remain coordinator
  checks; host seams do not certify OEM behavior.

1.18.1 integration: the native accessibility adapter uses Flickable content_*
geometry accessors (the old viewport aliases no longer exist in Rust), retaining
the same negative scroll-offset convention. Its direct vtable dependency matches
core at 0.5 to keep ItemTree pointer identities type-compatible.

## Rejected UI frame-gate cleanup (2026-11-04)

Repeated S9 controls did not establish a latency/cadence benefit from the UI
Choreographer permit experiment. Its callback/token machinery and default-off
per-frame gate checks are removed. The native VsyncDriver, idle behavior,
10ms unavailable-helper fallback,16ms failed-render retry, rounded timer tails,
first terminal outcome, synchronous RedrawNeeded, final animation invalidation,
refresh voting and callback redraw ownership remain. Native input event-time
and compact tracing diagnostics are retained. The actual package debug/API
predicate reused by the separate prefix diagnostic is neutrally named
`debug_trace_supported`; its eligibility semantics are unchanged.

TerminateWindow synchronously suspends the renderer while the native window
is still valid. Three real-renderer host tests and three S9 Home/return cycles
passed before this cleanup; final Android integration validates the combined
change. No graphics quality, resolution or content-cache policy is changed.

## Rotation relayout: one rotation must fully relayout and repaint (2026-11-05)

Landscape->portrait (or back) left the UI squished into one corner with the
rest black until a SECOND rotation; the first config change did not take. The
app handles `configChanges` (manifest) so the activity is not recreated: a
rotation arrives as `ConfigChanged` then `WindowResized`/`ContentRectChanged`,
with no `InitWindow`, so the EGL surface is never recreated. Traced on an S9
(logcat tag rotate_dbg), the real sequence landscape(2960x1440)->portrait was:

  ConfigChanged native_window=2960x1440   # STILL the old size here; density
                                          # unchanged, so the scale-gated branch
                                          # is a no-op and dispatches no Resized.
  WindowResized native_window=1440x2960   # the new size finally arrives
  resize() -> dispatch WindowEvent::Resized(1440x2960)
  do_render requested=1440x2960 skia_target=2960x1440 egl_surface=2960x1440  # BUG
  ... next frame: egl_surface=1440x2960 (correct)

Two root causes, both confirmed by the trace:

1. `WindowEvent::Resized` (i-slint-core api.rs) relayouts AND calls
   `renderer().resize()` (which on Android is `ANativeWindow_setBuffersGeometry`
   via glutin), but the ANativeWindow back buffer only takes the new geometry on
   the NEXT swap. The immediate frame draws the new-size scene into the still-old
   EGL buffer -> squished-into-a-corner, rest black. On a screen that keeps
   animating, continuous redraws self-heal within one frame; on a STATIC screen
   that stale frame is the last one drawn and persists until the next event (the
   second rotation). Fix: `i-slint-renderer-skia/opengl_surface.rs::render`
   reports `DrawOutcome::Skipped` (Android only) when the live EGL surface size
   (eglQuerySurface) does not yet equal the requested size, so the event loop's
   existing not-presented path re-arms one more redraw. The follow-up frame draws
   into the now-correct buffer. Steady state (sizes equal) is unaffected.

2. `AndroidWindowAdapter::resize` dispatched `Resized` but never set
   `pending_redraw`, so on a static screen no frame was scheduled after the
   rotation at all. Fix: `resize()` sets `pending_redraw = true`.

`ConfigChanged` is deliberately left gated on scale_factor: at ConfigChanged
time `native_window` still reports the OLD geometry (see trace), so dispatching
Resized there would carry the stale size. `WindowResized` carries the real new
size and drives `resize()`; that is the correct trigger. The native-pacing loop,
vsync gate and swap-interval handling are untouched.

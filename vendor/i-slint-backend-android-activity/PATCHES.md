# Chatter patches to i-slint-backend-android-activity 1.17.1

Copied from the Cargo registry at exactly 1.17.1. Keep upstream licensing files.

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
  `accessibility.rs` finds each item's stable id by searching on from the
  previous hit (tree order repeats between snapshots) instead of from the
  start of the ever-growing id list.
  The provider caches a snapshot for 100 ms from when it is READY, so the
  per-node calls of one query share it even when a snapshot takes longer.
  A node with zero width or height is reported not visible to the user:
  banners that collapse by animating height to 0 stay `is_visible()` in Slint
  and were otherwise read out by TalkBack while hidden.

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
- `lib.rs` `run_event_loop`: frame pacing. Upstream capped the poll timeout at a
  hardcoded 10 ms while animations run (its own FIXME), so a fling ticked at
  ~100 fps whatever the panel's rate. Now, while animating with a native window,
  the timeout is 0 and a redraw is forced every iteration; the vsync'd
  `eglSwapBuffers` (`opengl_surface.rs`, `SwapInterval::Wait(1)`) blocks and
  paces the loop at the display's refresh rate. Without a native window there
  is no swap to block on, so the timeout falls back to 16 ms. Under that pacing
  sits a frame floor: an iteration that renders takes at least half a vsync of
  the panel's top rate (4 ms until known), twice that if `do_render` presented
  nothing (it now returns whether the `DrawOutcome` was `Success`), so a swap
  that does not block can never tight-spin.
- `lib.rs` `with_refresh_rate_vote` / `androidwindowadapter.rs` `last_motion`:
  the app votes the panel's top rate only while the UI moves. The loop calls
  the vote with `true` when an animation runs or a touch moves, and `false`
  after 400 ms of neither (it wakes itself for that), on transitions only.
  chatter-ui wires it to `MainActivity.setUiMoving`, which sets/clears
  `Surface.setFrameRate` and returns the panel's top rate for the floor.

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

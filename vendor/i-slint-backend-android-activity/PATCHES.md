# Chatter patches to i-slint-backend-android-activity 1.17.1

Copied from the Cargo registry at exactly 1.17.1. Keep upstream licensing files.

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
  `handle_text_input_state`, `lib.rs` `with_text_input_bridge`,
  `consume_text_input`, `deliver_text_input_state`). Core publishes a
  programmatic text change only at the next render (`i-slint-core`
  `items/text.rs` `render`), android-activity 0.6.1 samples one latest-state
  text snapshot per input pass after that pass's motions, GameTextInput's
  `InputConnection.setState` never fires its listener (only `endBatchEdit` and
  `onKey` do, games-activity 4.4.0 bytecode) and android-activity's own
  `setState` travels the native work pipe while anything the app posts is a
  Handler message, so the two could not be ordered. A send tap therefore
  cleared the field and then received the IME's snapshot of the sent text
  (composing), which refilled it; the render saw nothing to publish and the IME
  kept composing the sent text (Galaxy S9 / Gboard, caption + attachment and a
  plain send alike). Now the app binds a bridge to its producer gate
  (`com.google.androidgamesdk.ChatterTextInputGate`, a GameTextInput
  `InputConnection` subclass installed over the surface view's own): every
  outbound state goes through `TextInputBridge::push` with the owner (a number
  per `ime_client`) and generation it was computed under; an accepted send
  (`consume_text_input`, called by the app right after it clears the field; a
  no-op unless the IME's client is that now-empty field) bumps the generation
  and publishes an immutable pending reset through `TextInputBridge::consume`
  without waiting; the gate empties the Editable before the IME's next
  operation runs (or from a posted fallback, once), drops pushes older than
  the reset, and tags every snapshot the IME produces with the owner and
  generation applied on the UI thread when it was made. Inbound states arrive
  through `deliver_text_input_state` in arrival order and are dropped only by
  that provenance (another owner, or a generation older than the consumption);
  content is never compared. With a bridge bound android-activity's
  `TextEvent` is a wake-up only; without one the old path is unchanged.
  Limit: an IME operation the IME computed against the consumed text (a
  composing-text update dispatched before it learned of the restart) applies
  as the IME formed it, to the new generation; no timer, focus change or
  content rule is used to second-guess it.

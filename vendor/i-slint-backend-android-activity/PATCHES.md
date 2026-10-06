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

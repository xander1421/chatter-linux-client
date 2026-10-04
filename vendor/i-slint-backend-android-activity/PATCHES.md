# Chatter patches to i-slint-backend-android-activity 1.17.1

Copied from the Cargo registry at exactly 1.17.1. Keep upstream licensing files.

- `androidwindowadapter.rs` (game-activity path): IME state handling — see the
  `[patch.crates-io]` comment in the workspace `Cargo.toml`.
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

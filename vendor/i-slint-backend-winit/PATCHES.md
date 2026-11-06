# Chatter patches to i-slint-backend-winit 1.18.1

Rebased onto the exact public 1.18.1 crate sources from the 1.17.1 fork.
Keep upstream licensing files.

- `winitwindowadapter.rs` `handle_focus_change`: an accessibility action that
  moves keyboard focus (AT-SPI click on a chat row opens the chat and focuses
  its composer) runs while the AccessKit adapter is borrowed, so the
  unconditional `borrow_mut()` panicked with "RefCell already borrowed" and the
  app exited (headless desktop driven over AT-SPI, 2026-10-29). The focus change
  is now reported directly when the adapter is free, else from a zero-delay
  timer once the action has returned.

- `accesskit.rs` modal pruning (2026-10-29): desktop screen readers could walk
  out of an open dialog or menu into the screen behind it (AT-SPI listed the
  whole chat under Contacts, Calls, Settings dialogs). As on Android, the
  frontmost visible `accessible-id: "modal-overlay"` item becomes the window
  root's only child; a change of overlay forces a full rebuild, and a focus
  node behind the overlay falls back to the root.

## 1.18.1 rebase

- Upstream now uses `try_borrow_mut()` for focus changes, avoiding the original
  panic, but skips notification on contention. Keep Chatter's zero-delay retry
  so an accessibility action still reports its resulting composer focus.
- Modal pruning now feeds upstream's `descendant_children` cache, alongside
  the new TextRun emission/state cache and sub-node IDs. Keep both caches:
  TextRuns remain upstream-managed children of their exposed input wrapper.
- Tree construction uses upstream's fallible `try_component()` lifecycle
  guard before recording the window/modal roots. Retain upstream's deferred
  accessibility actions, renderer-backed text emission, and deactivation cleanup.
- Neither local patch is completely superseded upstream. No app-facing API
  migration is introduced by these patches; upstream backend/context and renderer
  integration changes are retained unchanged.

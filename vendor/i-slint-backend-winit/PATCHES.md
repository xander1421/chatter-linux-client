# Chatter patches to i-slint-backend-winit 1.17.1

Copied from the Cargo registry at exactly 1.17.1. Keep upstream licensing files.

- `winitwindowadapter.rs` `handle_focus_change`: an accessibility action that
  moves keyboard focus (AT-SPI click on a chat row opens the chat and focuses
  its composer) runs while the AccessKit adapter is borrowed, so the
  unconditional `borrow_mut()` panicked with "RefCell already borrowed" and the
  app exited (headless desktop driven over AT-SPI, 2026-10-29). The focus change
  is now reported directly when the adapter is free, else from a zero-delay
  timer once the action has returned.

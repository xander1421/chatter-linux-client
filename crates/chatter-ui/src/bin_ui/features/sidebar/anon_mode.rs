//! Anon Mode toggle + PIN gate handlers.
//!
//! The button lives in the sidebar's bottom utility bar (see
//! `ui/sidebar/sidebar.slint`'s `anon-mode-clicked` callback) and routes
//! through the App's `toggle-anon-mode` callback. Logic:
//!
//! - If anon mode is currently OFF: open the PIN modal. On submit, call
//!   `ChatterClient::verify_pin_for_anon_mode(pin)`. On success: flip the
//!   in-memory flag, flip the Slint `AnonMode.enabled` global, refresh
//!   sidebar models. On failure: show inline error in the modal.
//!
//! - If anon mode is currently ON: flip both flags off + refresh. No PIN
//!   required to LEAVE anon mode (the property we protect is "harder to
//!   accidentally enter", not "harder to leave").

use std::sync::Arc;

use slint::ComponentHandle;

use chatter_core::client::ChatterClient;

use crate::{AnonMode, MainWindow};

/// Wire both the sidebar's `toggle-anon-mode` callback and the PIN modal's
/// `submit-anon-mode-pin` callback. Idempotent — call once per window.
pub(crate) fn wire(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    wire_toggle(window, ui_ctx);
    wire_submit_pin(window, ui_ctx);
}

fn wire_toggle(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak = window.as_weak();
    let ui_ctx = ui_ctx.clone();

    window.on_toggle_anon_mode(move || {
        let weak = weak.clone();
        let ui_ctx = ui_ctx.clone();

        let _ = slint::invoke_from_event_loop(move || {
            let Some(w) = weak.upgrade() else { return };
            if ui_ctx.anon_mode_enabled() {
                // Leaving anon mode — no PIN gate.
                ui_ctx.set_anon_mode_enabled(false);
                w.global::<AnonMode>().set_enabled(false);
                refresh_sidebar(&w, &ui_ctx);
                tracing::info!("Anon Mode disabled");
            } else {
                // Entering anon mode — prompt for PIN.
                w.set_anon_mode_pin("".into());
                w.set_anon_mode_pin_error("".into());
                w.set_anon_mode_verifying(false);
                crate::bin_ui::features::back::push(&w, "anon-mode-pin");
            }
        });
    });
}

fn wire_submit_pin(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak = window.as_weak();
    let ui_ctx = ui_ctx.clone();

    window.on_submit_anon_mode_pin(move |pin| {
        let weak = weak.clone();
        let ui_ctx = ui_ctx.clone();
        let pin = pin.to_string();

        let _ = slint::invoke_from_event_loop(move || {
            let Some(w) = weak.upgrade() else { return };
            w.set_anon_mode_verifying(true);
            w.set_anon_mode_pin_error("".into());

            // verify_pin is sync (local Argon2 fingerprint check) — run
            // inline. Wrapping in spawn_blocking would just add overhead
            // for a single HKDF / HMAC.
            let ok = ui_ctx.verify_pin_for_anon_mode(&pin);
            w.set_anon_mode_verifying(false);

            if ok {
                ui_ctx.set_anon_mode_enabled(true);
                w.global::<AnonMode>().set_enabled(true);
                crate::bin_ui::features::back::close(&w, "anon-mode-pin");
                w.set_anon_mode_pin("".into());
                refresh_sidebar(&w, &ui_ctx);
                tracing::info!("Anon Mode enabled");
            } else {
                w.set_anon_mode_pin_error(
                    "Incorrect PIN. Try again or sign in fresh if you've never set one.".into(),
                );
            }
        });
    });
}

/// Rebuild the chat list so the anon-mode filter takes effect: core's
/// `sidebar_rows` applies `anon_mode_enabled()` to every kind.
fn refresh_sidebar(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    crate::bin_ui::features::sidebar::refresh_chat_list(window, ui_ctx);
}

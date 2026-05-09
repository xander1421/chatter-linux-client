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
                w.set_active_modal("anon-mode-pin".into());
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
                w.set_active_modal("none".into());
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

/// Rebuild sidebar models so the anon-mode filter takes effect.
///
/// `refresh_dm_list` clears the DM model and `refresh_anon_dms` then
/// re-merges anon entries — both helpers read `anon_mode_enabled()`
/// internally and apply the filter.
fn refresh_sidebar(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    crate::bin_ui::features::auth::initial_data::refresh_dm_list(window, ui_ctx);
    crate::bin_ui::features::auth::initial_data::refresh_anon_dms(window, ui_ctx);

    // Channels: rebuild from local cache. When anon mode is on, we want
    // an empty list. When it flips back off, we want the cached server
    // channels back. The cheapest path that handles both: call the same
    // builder used at login.
    rebuild_channels(window, ui_ctx);

    crate::bin_ui::features::sidebar::refresh_selection(window, ui_ctx);
}

/// Mirror the channel-list build logic from `load_initial_data`, but
/// synchronous on the UI thread (the data already lives in local
/// storage). Anon Mode short-circuits to an empty list.
fn rebuild_channels(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    use chatter_core::domain::channel::ChannelType;

    let anon_mode = ui_ctx.anon_mode_enabled();
    let channel_list: Vec<crate::ChannelListItemData> = if anon_mode {
        Vec::new()
    } else {
        let channels = ui_ctx.get_channels_mru();
        channels
            .iter()
            .filter(|c| matches!(c.channel_type, ChannelType::Public | ChannelType::Private))
            .map(|c| crate::bin_ui::converters::channel_to_slint(c, ui_ctx))
            .collect()
    };
    window.set_channels(slint::ModelRc::new(slint::VecModel::from(channel_list)));
}

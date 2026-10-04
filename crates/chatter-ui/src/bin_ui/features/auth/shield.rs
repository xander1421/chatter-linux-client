//! Shield toggle on the sign-in, create-account and link-device screens.
//!
//! Thin glue over `ChatterClient::set_shield_before_login`, which owns the
//! decision and fails closed. Turning Shield on also starts the Tor connection
//! in the background, so the cold Arti bootstrap (up to about a minute)
//! overlaps with the user filling in the form. That dial goes only to the
//! onion service.

use std::sync::Arc;

use slint::ComponentHandle;

use chatter_core::client::ChatterClient;

use crate::{AnonMode, MainWindow};

/// Status line while a Shield sign-in / registration waits on its first Tor
/// connection.
pub(super) const CONNECTING_OVER_TOR: &str =
    "Connecting over Tor… the first connection can take up to a minute.";

pub(super) fn wire_shield_before_login(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    window
        .global::<AnonMode>()
        .set_shield_available(ui_ctx.shield_available());

    let weak = window.as_weak();
    let ui_ctx = ui_ctx.clone();
    window.global::<AnonMode>().on_toggle_before_login(move || {
        // Slint callbacks run on the UI thread.
        let Some(w) = weak.upgrade() else { return };
        let enable = !ui_ctx.anon_mode_enabled();
        match ui_ctx.set_shield_before_login(enable) {
            Ok(()) => {
                w.global::<AnonMode>().set_enabled(enable);
                w.set_login_error("".into());
                w.set_register_error("".into());
                w.set_link_device_error("".into());
                if enable {
                    let ui_ctx = ui_ctx.clone();
                    crate::rt_handle().spawn(async move {
                        if let Err(e) = ui_ctx.connect().await {
                            tracing::info!(error = %e, "Shield: early Tor connect failed; sign-in will retry over Tor");
                        }
                    });
                }
            }
            Err(e) => {
                let message = e.user_message();
                w.set_login_error(message.clone().into());
                w.set_register_error(message.clone().into());
                w.set_link_device_error(message.into());
            }
        }
    });
}

/// The error to show when the connect step fails. In Shield it must say that
/// nothing went direct — the pool has no direct path while Shield is on.
pub(super) fn connect_error(shield: bool, error: &impl std::fmt::Display) -> String {
    if shield {
        format!("Couldn't reach the server over Tor: {error}. Shield is still on; nothing was sent outside Tor.")
    } else {
        format!("Failed to connect to server: {error}")
    }
}

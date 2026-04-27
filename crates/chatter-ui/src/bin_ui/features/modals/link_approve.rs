//! Link-device approval modal wiring (primary device side).
//!
//! Pure UI plumbing. All parsing, password verification, and provisioning
//! cipher work lives in `chatter-core`'s `LinkDeviceService`. This module
//! binds three callbacks:
//!
//! 1. `link-approve-paste-changed(url)` — re-parse URL preview as user
//!    types/pastes, populate "About to link" panel.
//! 2. `link-approve-submit(url, password)` — fire facade
//!    `approve_link_from_url`, render result.
//! 3. `link-approve-open` — toolbar / settings entry point that opens the
//!    modal with cleared state.

use std::sync::Arc;

use slint::ComponentHandle;

use chatter_core::client::ChatterClient;
use chatter_core::services::link_device_service::LinkDeviceError;

use crate::MainWindow;

pub fn wire(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    wire_open(window);
    wire_paste(window);
    wire_close(window);
    wire_submit(window, ui_ctx);
}

fn wire_open(window: &MainWindow) {
    let weak = window.as_weak();
    window.on_link_approve_open(move || {
        if let Some(w) = weak.upgrade() {
            reset(&w);
            w.set_active_modal("link-device-approve".into());
        }
    });
}

fn wire_close(window: &MainWindow) {
    let weak = window.as_weak();
    window.on_link_approve_close(move || {
        if let Some(w) = weak.upgrade() {
            reset(&w);
        }
    });
}

fn wire_paste(window: &MainWindow) {
    let weak = window.as_weak();
    window.on_link_approve_paste_changed(move |text: slint::SharedString| {
        let Some(w) = weak.upgrade() else { return };
        let s = text.to_string();
        if s.is_empty() {
            w.set_link_approve_preview_ready(false);
            w.set_link_approve_preview_name("".into());
            w.set_link_approve_preview_server("".into());
            w.set_link_approve_error("".into());
            return;
        }
        match ChatterClient::preview_link_url(&s) {
            Ok(preview) => {
                w.set_link_approve_preview_ready(true);
                w.set_link_approve_preview_name(
                    preview.device_name.unwrap_or_default().into(),
                );
                w.set_link_approve_preview_server(
                    preview.server_url.unwrap_or_default().into(),
                );
                w.set_link_approve_error("".into());
            }
            Err(e) => {
                w.set_link_approve_preview_ready(false);
                // Only show error once user has typed something substantial.
                if s.len() > 16 {
                    w.set_link_approve_error(format!("{e}").into());
                } else {
                    w.set_link_approve_error("".into());
                }
            }
        }
    });
}

fn wire_submit(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak = window.as_weak();
    let ui_ctx = ui_ctx.clone();

    window.on_link_approve_submit(
        move |url: slint::SharedString, password: slint::SharedString| {
            let weak = weak.clone();
            let ui_ctx = ui_ctx.clone();
            let url = url.to_string();
            let password = password.to_string();

            if let Some(w) = weak.upgrade() {
                w.set_link_approve_busy(true);
                w.set_link_approve_error("".into());
                w.set_link_approve_status("Authorizing…".into());
            }

            crate::rt_handle().spawn(async move {
                let result = ui_ctx.approve_link_from_url(&url, &password).await;
                let weak_for_ui = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(w) = weak_for_ui.upgrade() else { return };
                    w.set_link_approve_busy(false);
                    match result {
                        Ok(approved) => {
                            let name = approved
                                .device_name
                                .unwrap_or_else(|| "the new device".to_string());
                            w.set_link_approve_status(
                                format!("Linked {name}.").into(),
                            );
                            w.set_link_approve_password("".into());
                            // Leave modal open briefly so user sees confirmation;
                            // they tap close to dismiss.
                        }
                        Err(LinkDeviceError::ReauthFailed) => {
                            w.set_link_approve_status("".into());
                            w.set_link_approve_error("Wrong password or PIN.".into());
                        }
                        Err(LinkDeviceError::ReauthUnsupportedForOidc) => {
                            // OIDC user with no PIN configured — they need to set a
                            // PIN before they can authorise new devices.
                            w.set_link_approve_status("".into());
                            w.set_link_approve_error(
                                "Set a recovery PIN in Settings before linking new devices.".into(),
                            );
                        }
                        Err(e) => {
                            w.set_link_approve_status("".into());
                            w.set_link_approve_error(format!("{e}").into());
                        }
                    }
                });
            });
        },
    );
}

fn reset(w: &MainWindow) {
    w.set_link_approve_url("".into());
    w.set_link_approve_password("".into());
    w.set_link_approve_preview_ready(false);
    w.set_link_approve_preview_name("".into());
    w.set_link_approve_preview_server("".into());
    w.set_link_approve_error("".into());
    w.set_link_approve_status("".into());
    w.set_link_approve_busy(false);
}

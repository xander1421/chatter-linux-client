//! Link-device approval modal wiring (primary device side).
//!
//! Pure UI plumbing. All parsing, password verification, and provisioning
//! cipher work lives in `chatter-core`'s `LinkDeviceService`. The flow is:
//!
//! 1. `link-approve-paste-changed(url)` — re-parse URL preview as user
//!    types / pastes. Calls the SDK's `prepare_link_from_url`, stashes
//!    the resulting `PendingDeviceLink` in a shared slot so the submit
//!    handler can consume it, and populates the modal with the device
//!    name + verification code. **The verification code MUST be
//!    displayed and confirmed by the user before submit fires.**
//! 2. `link-approve-submit(password)` — pulls the stashed
//!    `PendingDeviceLink` (move) and calls `approve_link(pending,
//!    SecretString)`. Cannot be invoked without a successful preview,
//!    cannot be replayed (the token is consumed).
//! 3. `link-approve-open` / `-close` — modal lifecycle.

use std::sync::{Arc, Mutex};

use slint::ComponentHandle;

use chatter_core::client::ChatterClient;
use chatter_core::prelude::SecretString;
use chatter_core::services::link_device_service::{
    LinkDeviceError, PendingDeviceLink,
};

use crate::MainWindow;

/// Shared slot for the in-flight pending link.
///
/// `PendingDeviceLink` is `!Clone` and consumed by `approve_link`, so the
/// slot holds an `Option<PendingDeviceLink>` that is filled by the paste
/// handler and `take()`n by the submit handler. The `Mutex` is for sharing
/// across the two callbacks; there is no real concurrency since both
/// callbacks are dispatched on the Slint event loop.
type PendingSlot = Arc<Mutex<Option<PendingDeviceLink>>>;

pub fn wire(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let pending: PendingSlot = Arc::new(Mutex::new(None));
    wire_open(window, pending.clone());
    wire_paste(window, ui_ctx, pending.clone());
    wire_close(window, pending.clone());
    wire_submit(window, ui_ctx, pending);
}

fn wire_open(window: &MainWindow, pending: PendingSlot) {
    let weak = window.as_weak();
    window.on_link_approve_open(move || {
        if let Some(w) = weak.upgrade() {
            reset(&w, &pending);
            w.set_active_modal("link-device-approve".into());
        }
    });
}

fn wire_close(window: &MainWindow, pending: PendingSlot) {
    let weak = window.as_weak();
    window.on_link_approve_close(move || {
        if let Some(w) = weak.upgrade() {
            reset(&w, &pending);
        }
    });
}

fn wire_paste(window: &MainWindow, ui_ctx: &Arc<ChatterClient>, pending: PendingSlot) {
    let weak = window.as_weak();
    let ui_ctx = ui_ctx.clone();
    window.on_link_approve_paste_changed(move |text: slint::SharedString| {
        let Some(w) = weak.upgrade() else { return };
        let s = text.to_string();
        if s.is_empty() {
            *pending.lock().unwrap() = None;
            w.set_link_approve_preview_ready(false);
            w.set_link_approve_preview_name("".into());
            w.set_link_approve_preview_server("".into());
            w.set_link_approve_verification_code("".into());
            w.set_link_approve_error("".into());
            return;
        }
        match ui_ctx.prepare_link_from_url(&s) {
            Ok(preview) => {
                w.set_link_approve_preview_ready(true);
                w.set_link_approve_preview_name(
                    preview.device_name().unwrap_or_default().into(),
                );
                w.set_link_approve_preview_server(
                    preview.server_url().unwrap_or_default().into(),
                );
                w.set_link_approve_verification_code(
                    preview.verification_code().into(),
                );
                w.set_link_approve_error("".into());
                *pending.lock().unwrap() = Some(preview);
            }
            Err(e) => {
                *pending.lock().unwrap() = None;
                w.set_link_approve_preview_ready(false);
                w.set_link_approve_verification_code("".into());
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

fn wire_submit(
    window: &MainWindow,
    ui_ctx: &Arc<ChatterClient>,
    pending: PendingSlot,
) {
    let weak = window.as_weak();
    let ui_ctx = ui_ctx.clone();

    window.on_link_approve_submit(move |password: slint::SharedString| {
        let weak = weak.clone();
        let ui_ctx = ui_ctx.clone();
        let password = password.to_string();
        let pending = pending.clone();

        // Consume the pending link. If it's already been taken (double-
        // submit, or paste was never successful), bail with a clear error.
        let token = match pending.lock().unwrap().take() {
            Some(t) => t,
            None => {
                if let Some(w) = weak.upgrade() {
                    w.set_link_approve_error(
                        "Paste a link URL first.".into(),
                    );
                }
                return;
            }
        };

        if let Some(w) = weak.upgrade() {
            w.set_link_approve_busy(true);
            w.set_link_approve_error("".into());
            w.set_link_approve_status("Authorizing…".into());
        }

        crate::rt_handle().spawn(async move {
            let secret = SecretString::new(password);
            let result = ui_ctx.approve_link(token, secret).await;
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
    });
}

fn reset(w: &MainWindow, pending: &PendingSlot) {
    *pending.lock().unwrap() = None;
    w.set_link_approve_url("".into());
    w.set_link_approve_password("".into());
    w.set_link_approve_preview_ready(false);
    w.set_link_approve_preview_name("".into());
    w.set_link_approve_preview_server("".into());
    w.set_link_approve_verification_code("".into());
    w.set_link_approve_error("".into());
    w.set_link_approve_status("".into());
    w.set_link_approve_busy(false);
}

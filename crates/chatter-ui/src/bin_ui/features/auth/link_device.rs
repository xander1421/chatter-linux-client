//! Link-device callback wiring (new-device side).
//!
//! Pure UI plumbing: forwards user actions to `ChatterClient::link_device_*`
//! façade methods and renders [`LinkProgress`] events into widget state. All
//! orchestration, session state, polling, and error formatting stays in
//! `chatter-core`'s [`chatter_core::services::link_device_service`].

use std::sync::Arc;

use slint::ComponentHandle;

use chatter_core::client::ChatterClient;
use chatter_core::services::link_device_service::LinkProgress;

use super::initial_data::load_initial_data;
use crate::MainWindow;

pub fn wire_link_device(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    wire_start(window, ui_ctx);
    wire_cancel(window, ui_ctx);
    wire_copy(window);
}

fn wire_start(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let ui_ctx = ui_ctx.clone();
    let weak = window.as_weak();

    window.on_link_device_start(move || {
        let ui_ctx = ui_ctx.clone();
        let weak = weak.clone();

        if let Some(w) = weak.upgrade() {
            w.set_link_device_active(true);
            w.set_link_device_error("".into());
            w.set_link_device_status("Generating QR code…".into());
        }

        crate::rt_handle().spawn(async move {
            let mut rx = match ui_ctx.link_device_run().await {
                Ok(rx) => rx,
                Err(e) => {
                    set_error(&weak, e.to_string());
                    return;
                }
            };

            let mut verification_code = String::new();
            while let Some(event) = rx.recv().await {
                match event {
                    LinkProgress::QrReady { url, qr_png, verification_code: code, .. } => {
                        verification_code = code;
                        let pixels = decode_png_to_rgba(&qr_png);
                        let url_for_ui = url;
                        let weak_for_ui = weak.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak_for_ui.upgrade() {
                                if let Some((px, ww, hh)) = pixels {
                                    w.set_link_device_qr(
                                        crate::bin_ui::slint_conversions::rgba_to_slint_image(
                                            &px, ww, hh,
                                        ),
                                    );
                                }
                                w.set_link_device_url(url_for_ui.into());
                            }
                        });
                    }
                    LinkProgress::WaitingForApproval => {
                        set_status(&weak, &format!(
                            "Verification code: {verification_code}\nCheck your other device shows the same code, then approve there."
                        ));
                    }
                    LinkProgress::Approved => {
                        set_status(&weak, "Approved! Setting up this device…");
                    }
                    LinkProgress::Linked(_completed) => {
                        finish_linked(&weak, &ui_ctx).await;
                        return;
                    }
                    LinkProgress::Failed(msg) => {
                        set_error(&weak, msg);
                        return;
                    }
                    LinkProgress::Cancelled => {
                        set_status(&weak, "This QR code expired or was cancelled. Generate a new one.");
                        let weak_for_view = weak.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak_for_view.upgrade() {
                                w.set_link_device_active(false);
                                w.set_link_device_url("".into());
                            }
                        });
                        return;
                    }
                }
            }
        });
    });
}

fn wire_cancel(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let ui_ctx = ui_ctx.clone();
    let weak = window.as_weak();

    window.on_link_device_cancel(move || {
        let ui_ctx = ui_ctx.clone();
        crate::rt_handle().spawn(async move {
            let _ = ui_ctx.link_device_cancel().await;
        });

        if let Some(w) = weak.upgrade() {
            w.set_current_view("login".into());
        }
    });
}

fn wire_copy(window: &MainWindow) {
    let weak = window.as_weak();

    window.on_link_device_copy(move || {
        let Some(w) = weak.upgrade() else { return };
        let url = w.get_link_device_url().to_string();
        if url.is_empty() {
            return;
        }

        #[cfg(target_os = "android")]
        {
            let message = match crate::platform::android::clipboard::set_text("Device link", &url) {
                Ok(()) => "Link copied to clipboard.",
                Err(_) => "Unable to copy link. Please try again.",
            };
            w.set_link_device_status(message.into());
        }
        #[cfg(target_os = "ios")]
        w.set_link_device_status("Clipboard copy is unavailable on this device.".into());

        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        let weak = weak.clone();
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        std::thread::spawn(move || {
            let wayland = std::process::Command::new("wl-copy").arg(&url).status();
            let result = match wayland {
                Ok(status) if status.success() => Ok(status),
                _ => std::process::Command::new("xclip")
                    .args(["-selection", "clipboard"])
                    .stdin(std::process::Stdio::piped())
                    .spawn()
                    .and_then(|mut child| {
                        use std::io::Write;
                        if let Some(stdin) = child.stdin.as_mut() {
                            stdin.write_all(url.as_bytes())?;
                        }
                        child.wait()
                    }),
            };
            let copied = result.is_ok_and(|status| status.success());
            let _ = weak.upgrade_in_event_loop(move |w| {
                // A completed old copy must not change a replacement link page.
                if w.get_current_view().as_str() != "link-device"
                    || w.get_link_device_url().as_str() != url
                {
                    return;
                }
                w.set_link_device_status(if copied {
                    "Link copied to clipboard.".into()
                } else {
                    "Unable to copy link. Please try again.".into()
                });
            });
        });
    });
}

// ============================================================================
// Helpers
// ============================================================================

fn set_status(weak: &slint::Weak<MainWindow>, msg: &str) {
    let weak = weak.clone();
    let msg = msg.to_string();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            w.set_link_device_status(msg.into());
        }
    });
}

fn set_error(weak: &slint::Weak<MainWindow>, msg: String) {
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            w.set_link_device_active(false);
            w.set_link_device_error(msg.into());
        }
    });
}

fn decode_png_to_rgba(bytes: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    let img = image::load_from_memory(bytes).ok()?;
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    Some((rgba.into_raw(), w, h))
}

/// Post-link bring-up. Mirrors the tail of password login: starts background
/// services on the SDK, subscribes to domain events, switches the view,
/// loads initial data and presence.
async fn finish_linked(weak: &slint::Weak<MainWindow>, ui_ctx: &Arc<ChatterClient>) {
    if let Err(e) = ui_ctx.start().await {
        set_error(weak, format!("post-link bring-up failed: {e}"));
        return;
    }

    crate::bin_ui::features::modals::spawn_anon_group_invite_handler(weak.clone(), ui_ctx);
    crate::bin_ui::features::modals::spawn_anon_dm_request_handler(weak.clone(), ui_ctx);
    crate::bin_ui::features::modals::load_channel_invites(weak.clone(), ui_ctx);

    {
        let mut domain_rx = ui_ctx.subscribe_events();
        let weak_for_domain = weak.clone();
        let ui_ctx_for_domain = ui_ctx.clone();
        crate::rt_handle().spawn(async move {
            loop {
                match domain_rx.recv().await {
                    Ok(event) => {
                        super::inbox::domain_handler::handle_domain_event(
                            event,
                            &weak_for_domain,
                            &ui_ctx_for_domain,
                        )
                        .await;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }

    let weak_for_view = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak_for_view.upgrade() {
            w.set_link_device_active(false);
            w.set_is_signed_in(true);
            w.set_current_view("main".into());
        }
    });

    load_initial_data(weak.clone(), ui_ctx).await;
    ui_ctx.initialize_presence().await;
}

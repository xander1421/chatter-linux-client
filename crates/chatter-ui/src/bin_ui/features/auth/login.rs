//! Login callback wiring.

use std::sync::Arc;

use slint::ComponentHandle;

use chatter_core::client::ChatterClient;

use super::initial_data::load_initial_data;
use crate::MainWindow;

pub fn wire_login(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    wire_password_login(window, ui_ctx);
}

fn wire_password_login(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let ui_ctx = ui_ctx.clone();
    let weak_window = window.as_weak();

    window.on_login(move |username: slint::SharedString, password: slint::SharedString, server_url: slint::SharedString, pin: slint::SharedString| {
        let ui_ctx = ui_ctx.clone();
        let weak = weak_window.clone();
        let username = username.to_string();
        let password = password.to_string();
        let server_url = server_url.to_string();
        let pin = pin.to_string();

        tokio::spawn(async move {
            // Set loading state
            let weak_for_loading = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_for_loading.upgrade() {
                    w.set_login_loading(true);
                    w.set_login_error("".into());
                }
            });

            // Update server URL and connect if different from current
            if ui_ctx.endpoint() != server_url {
                if let Err(e) = ui_ctx.set_server_url(&server_url) {
                    let error_msg = format!("Invalid Server URL: {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_login_loading(false);
                            w.set_login_error(error_msg.into());
                        }
                    });
                    return;
                }
            }

            // Ensure we are connected to the selected server
            if !ui_ctx.is_connected() {
                if let Err(e) = ui_ctx.connect().await {
                    let error_msg = format!("Failed to connect to server: {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_login_loading(false);
                            w.set_login_error(error_msg.into());
                        }
                    });
                    return;
                }
            }

            // Login via SDK (handles auth + state + inbox + presence)
            let pin_opt = if pin.is_empty() { None } else { Some(pin.as_str()) };
            match ui_ctx.login(&username, &password, "linux-client", pin_opt).await {
                Ok(_result) => {
                    // Spawn UI-only event handlers (need slint::Weak<MainWindow>)
                    crate::bin_ui::features::modals::spawn_anon_group_invite_handler(weak.clone(), &ui_ctx);
                    crate::bin_ui::features::modals::spawn_anon_dm_request_handler(weak.clone(), &ui_ctx);
                    crate::bin_ui::features::modals::load_channel_invites(weak.clone(), &ui_ctx);

                    // Subscribe to domain events for UI updates via the SDK facade.
                    {
                        let mut domain_rx = ui_ctx.subscribe_events();
                        let weak_for_domain = weak.clone();
                        let ui_ctx_for_domain = ui_ctx.clone();
                        tokio::spawn(async move {
                            loop {
                                match domain_rx.recv().await {
                                    Ok(event) => {
                                        super::inbox::domain_handler::handle_domain_event(event, &weak_for_domain, &ui_ctx_for_domain).await;
                                    }
                                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                        tracing::warn!(dropped_count = n, "Domain event receiver lagged");
                                    }
                                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                                }
                            }
                        });
                    }

                    // Switch to main view
                    let weak_for_view = weak.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak_for_view.upgrade() {
                            w.set_login_loading(false);
                            w.set_current_view("main".into());
                        }
                    });

                    // Load initial data + presence
                    load_initial_data(weak.clone(), &ui_ctx).await;
                    super::presence_integration::initialize_presence(&ui_ctx).await;
                }
                Err(e) => {
                    let error_msg = e.to_string();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_login_loading(false);
                            w.set_login_error(error_msg.into());
                        }
                    });
                }
            }
        });
    });
}


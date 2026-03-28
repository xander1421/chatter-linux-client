//! Login callback wiring.

use std::sync::Arc;

use slint::ComponentHandle;

use chatter_core::client::ChatterClient;

use super::initial_data::load_initial_data;
use crate::MainWindow;

pub fn wire_login(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    wire_password_login(window, ui_ctx);
    wire_sso_login(window, ui_ctx);
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
            if ui_ctx.auth().endpoint() != server_url {
                if let Err(e) = ui_ctx.auth().set_server_url(&server_url) {
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
            if !ui_ctx.auth().is_connected() {
                if let Err(e) = ui_ctx.auth().connect().await {
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
                    crate::bin_ui::features::auth::inbox::spawn_anon_group_event_handler(weak.clone(), &ui_ctx);
                    crate::bin_ui::features::modals::spawn_anon_dm_request_handler(weak.clone(), &ui_ctx);

                    // Subscribe to DomainEvent for UI updates
                    {
                        let mut domain_rx = ui_ctx.event_bus().subscribe_domain();
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

fn wire_sso_login(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let ui_ctx = ui_ctx.clone();
    let weak_window = window.as_weak();

    window.on_sso_login(move |server_url: slint::SharedString, issuer_url: slint::SharedString, client_id: slint::SharedString| {
        let ui_ctx = ui_ctx.clone();
        let weak = weak_window.clone();
        let server_url = server_url.to_string();
        let issuer_url = issuer_url.to_string();
        let client_id = client_id.to_string();

        tokio::spawn(async move {
            // Set loading state
            let weak_for_loading = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_for_loading.upgrade() {
                    w.set_login_loading(true);
                    w.set_login_error("".into());
                }
            });

            // Update server URL and connect
            if ui_ctx.auth().endpoint() != server_url {
                if let Err(e) = ui_ctx.auth().set_server_url(&server_url) {
                    set_login_error(&weak, format!("Invalid Server URL: {}", e));
                    return;
                }
            }

            if !ui_ctx.auth().is_connected() {
                if let Err(e) = ui_ctx.auth().connect().await {
                    set_login_error(&weak, format!("Failed to connect to server: {}", e));
                    return;
                }
            }

            // Discover OIDC provider
            if let Err(e) = ui_ctx.oidc().discover(&issuer_url, &client_id).await {
                set_login_error(&weak, format!("OIDC discovery failed: {}", e));
                return;
            }

            // Start authorization flow (get URL + callback port)
            let (auth_url, _port) = match ui_ctx.oidc().start_auth_flow().await {
                Ok(result) => result,
                Err(e) => {
                    set_login_error(&weak, format!("Failed to start SSO: {}", e));
                    return;
                }
            };

            // Open system browser for authentication
            if let Err(e) = open::that(auth_url.as_str()) {
                set_login_error(&weak, format!("Failed to open browser: {}", e));
                return;
            }

            // Wait for callback (blocks until browser redirects back)
            let tokens = match ui_ctx.oidc().wait_for_callback().await {
                Ok(tokens) => tokens,
                Err(e) => {
                    set_login_error(&weak, format!("SSO authentication failed: {}", e));
                    return;
                }
            };

            // Login with the OIDC access token
            match ui_ctx.login_with_oidc_token(&tokens.access_token, "linux-client").await {
                Ok(_result) => {
                    // Same post-login setup as password login
                    crate::bin_ui::features::modals::spawn_anon_group_invite_handler(weak.clone(), &ui_ctx);
                    crate::bin_ui::features::auth::inbox::spawn_anon_group_event_handler(weak.clone(), &ui_ctx);
                    crate::bin_ui::features::modals::spawn_anon_dm_request_handler(weak.clone(), &ui_ctx);

                    // Subscribe to domain events
                    {
                        let mut domain_rx = ui_ctx.event_bus().subscribe_domain();
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
                    set_login_error(&weak, e.to_string());
                }
            }
        });
    });
}

fn set_login_error(weak: &slint::Weak<MainWindow>, error: String) {
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            w.set_login_loading(false);
            w.set_login_error(error.into());
        }
    });
}

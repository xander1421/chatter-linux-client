//! Register callback wiring.

use std::sync::Arc;
use std::sync::Mutex;
use lazy_static::lazy_static;

use slint::{ComponentHandle, Image};

use chatter_core::app::UIContext;
#[cfg(not(target_os = "android"))]
use chatter_core::media::{FileFilter, FilePicker, FilePickerConfig};

lazy_static! {
    static ref SELECTED_AVATAR: Mutex<Option<Vec<u8>>> = Mutex::new(None);
}

const DEFAULT_AVATARS: &[&[u8]] = &[
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets/avatars/pepe.jpg")),
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets/avatars/doge.jpg")),
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets/avatars/wojak.jpg")),
];

use super::initial_data::load_initial_data;
use crate::MainWindow;

pub fn wire_register(window: &MainWindow, ui_ctx: &Arc<UIContext>) {
    let ui_ctx = ui_ctx.clone();
    let weak_window = window.as_weak();

    // Reset selection on startup
    if let Ok(mut guard) = SELECTED_AVATAR.lock() {
        *guard = None;
    }

    window.on_register_select_default_avatar({
        let weak = weak_window.clone();
        move |idx| {
            let idx = idx as usize;
            if idx < DEFAULT_AVATARS.len() {
                let bytes = DEFAULT_AVATARS[idx].to_vec();
                if let Ok(mut guard) = SELECTED_AVATAR.lock() {
                    *guard = Some(bytes.clone());
                }

                let weak = weak.clone();
                // Move image decoding to background thread
                std::thread::spawn(move || {
                    // Decode image off UI thread
                    let decoded = image::load_from_memory(&bytes).ok().map(|img| {
                        let rgba = img.to_rgba8();
                        let (w_img, h_img) = rgba.dimensions();
                        (rgba.into_raw(), w_img, h_img)
                    });

                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_register_has_avatar(true);
                            w.set_register_selected_default_idx(idx as i32);

                            if let Some((pixels, width, height)) = decoded {
                                let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                                    &pixels, width, height,
                                );
                                w.set_register_avatar_source(Image::from_rgba8(buffer));
                            }
                        }
                    });
                });
            }
        }
    });

    window.on_register_select_avatar({
        let weak = weak_window.clone();
        move || {
            #[cfg(not(target_os = "android"))]
            {
                let weak = weak.clone();
                tokio::spawn(async move {
                    let config = FilePickerConfig {
                        title: Some("Select Avatar".to_string()),
                        filters: vec![FileFilter {
                            name: "Images".to_string(),
                            extensions: vec!["jpg".to_string(), "jpeg".to_string(), "png".to_string(), "webp".to_string()],
                        }],
                        ..Default::default()
                    };

                    if let Some(path) = FilePicker::pick_file(config).await {
                        match std::fs::read(&path) {
                            Ok(bytes) => {
                                // Store bytes for registration
                                if let Ok(mut guard) = SELECTED_AVATAR.lock() {
                                    *guard = Some(bytes.clone());
                                }

                                // Decode image BEFORE invoke_from_event_loop (already in tokio::spawn)
                                let decoded = image::load_from_memory(&bytes).ok().map(|img| {
                                    let rgba = img.to_rgba8();
                                    let (w_img, h_img) = rgba.dimensions();
                                    (rgba.into_raw(), w_img, h_img)
                                });

                                // Update UI preview
                                let _ = slint::invoke_from_event_loop(move || {
                                    if let Some(w) = weak.upgrade() {
                                        // Reset default index
                                        w.set_register_selected_default_idx(-1);

                                        // Create slint::Image from pre-decoded pixels
                                        if let Some((pixels, width, height)) = decoded {
                                            let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                                                &pixels, width, height,
                                            );
                                            w.set_register_avatar_source(Image::from_rgba8(buffer));
                                            w.set_register_has_avatar(true);
                                        } else {
                                            tracing::error!("Failed to decode selected avatar for preview");
                                        }
                                    }
                                });
                            }
                            Err(e) => {
                                tracing::error!("Failed to read selected avatar file: {}", e);
                            }
                        }
                    }
                });
            }
            #[cfg(target_os = "android")]
            tracing::warn!("File picker not supported on Android");
        }
    });

    window.on_register(
        move |username: slint::SharedString, password: slint::SharedString, display_name: slint::SharedString, pin: slint::SharedString, server_url: slint::SharedString| {
            let ui_ctx = ui_ctx.clone();
            let weak = weak_window.clone();
            let username = username.to_string();
            let password = password.to_string();
            let display_name = display_name.to_string();
            let pin = pin.to_string();
            let server_url = server_url.to_string();

            tokio::spawn(async move {
                // Validate PIN is exactly 6 digits
                if pin.len() != 6 || !pin.chars().all(|c: char| c.is_ascii_digit()) {
                    let weak_for_error = weak.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak_for_error.upgrade() {
                            w.set_register_error("PIN must be exactly 6 digits".into());
                        }
                    });
                    return;
                }
                
                // Set loading state
                let weak_for_loading = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak_for_loading.upgrade() {
                        w.set_register_loading(true);
                        w.set_register_error("".into());
                    }
                });

                // Update server URL and connect if different from current
                if ui_ctx.auth_service.endpoint() != server_url {
                    if let Err(e) = ui_ctx.auth_service.set_server_url(&server_url) {
                        let error_msg = format!("Invalid Server URL: {}", e);
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                w.set_register_loading(false);
                                w.set_register_error(error_msg.into());
                            }
                        });
                        return;
                    }
                }

                // Ensure we are connected to the selected server
                if !ui_ctx.auth_service.is_connected() {
                    if let Err(e) = ui_ctx.auth_service.connect().await {
                        let error_msg = format!("Failed to connect to server: {}", e);
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                w.set_register_loading(false);
                                w.set_register_error(error_msg.into());
                            }
                        });
                        return;
                    }
                }

                // Use pre-built auth service from UIContext
                let auth_service = &ui_ctx.auth_service;
                // Get selected avatar or use random default
                let user_avatar = SELECTED_AVATAR.lock().ok().and_then(|g| g.clone());

                let avatar_bytes = if let Some(ref bytes) = user_avatar {
                    bytes.as_slice()
                } else {
                    // Load custom default avatars (cutouts in greyscale on black background)
                    let random_idx = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_millis() as usize % DEFAULT_AVATARS.len();
                    DEFAULT_AVATARS[random_idx]
                };


                // Attempt registration using simplified method
                match auth_service
                    .do_register(&username, &password, &display_name, "linux-client", Some(avatar_bytes), Some(&pin))
                    .await
                {
                    Ok(result) => {
                        // Clear selected avatar on success
                        if let Ok(mut guard) = SELECTED_AVATAR.lock() {
                            *guard = None;
                        }

                        // Update auth state
                        ui_ctx.auth_state.set_logged_in(
                            result.user_id,
                            result.device_id,
                            result.display_name.clone(),
                        );

                        // IMPORTANT: Spawn domain event handlers BEFORE starting inbox provider
                        // to avoid race condition where inbox emits events before handlers subscribe.

                        // Spawn DM bootstrap handler for processing incoming key exchanges
                        let _bootstrap_handle = ui_ctx.dm_bootstrap_service.spawn_handler(&ui_ctx.event_bus);
                        tracing::debug!("DM bootstrap handler spawned");

                        // Channel event stream is now started centrally in main.rs via AppContext
                        // It owns the gRPC stream and handles reconnection automatically
                        tracing::debug!("Channel events handled by service layer");

                        // Spawn anon group invite handler for processing incoming invites
                        let _anon_invite_handle = crate::bin_ui::features::modals::spawn_anon_group_invite_handler(
                            weak.clone(),
                            &ui_ctx,
                        );
                        tracing::debug!("Anon group invite handler spawned");

                        // Spawn anon group event handler for member joined/left/kicked events
                        let _anon_group_events_handle = crate::bin_ui::features::auth::inbox::spawn_anon_group_event_handler(
                            weak.clone(),
                            &ui_ctx,
                        );
                        tracing::debug!("Anon group event handler spawned");
                        
                        // Spawn pin persistence handler to save pin events to database
                        let _pin_persistence_handle = crate::bin_ui::features::auth::inbox::spawn_pin_persistence_handler(
                            ui_ctx.event_bus.clone(),
                            ui_ctx.message_service.clone(),
                        );
                        tracing::debug!("Pin persistence handler spawned");

                        // Spawn anon DM request handler for processing incoming message requests
                        let _anon_dm_handle = crate::bin_ui::features::modals::spawn_anon_dm_request_handler(
                            weak.clone(),
                            &ui_ctx,
                        );
                        tracing::debug!("Anon DM request handler spawned");

                        // Ensure post-boot services are initialized (may have been
                        // skipped if initial server connection failed on startup)
                        ui_ctx.ensure_call_service();

                        // Start inbox provider with InboxEventProcessor
                        // The processor handles protocol concerns and emits DomainEvent
                        {
                            let weak_for_inbox = weak.clone();
                            let ui_ctx_for_inbox = ui_ctx.clone();
                            tokio::spawn(async move {
                                // Start the inbox provider with the processor
                                if !ui_ctx_for_inbox.start_inbox_with_processor().await {
                                    tracing::warn!("Failed to start inbox with processor");
                                }

                                // Subscribe to DomainEvent for UI updates
                                let mut domain_rx = ui_ctx_for_inbox.event_bus.subscribe_domain();
                                let weak_for_domain = weak_for_inbox.clone();
                                let ui_ctx_for_domain = ui_ctx_for_inbox.clone();
                                tokio::spawn(async move {
                                    loop {
                                        match domain_rx.recv().await {
                                            Ok(event) => {
                                                super::inbox::domain_handler::handle_domain_event(event, &weak_for_domain, &ui_ctx_for_domain).await;
                                            }
                                            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                                tracing::warn!(
                                                    dropped_count = n,
                                                    "Domain event receiver lagged - {} events dropped",
                                                    n
                                                );
                                            }
                                            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                                tracing::info!("Domain event channel closed");
                                                break;
                                            }
                                        }
                                    }
                                });

                                // Also subscribe to raw InboxEvent for events not yet migrated
                                // TODO: Remove this once all events are handled via DomainEvent
                                if let Some(mut event_rx) = ui_ctx_for_inbox.start_inbox_provider().await {
                                    tokio::spawn(async move {
                                        loop {
                                            match event_rx.recv().await {
                                                Ok(event) => {
                                                    super::handle_inbox_event(event, &weak_for_inbox, &ui_ctx_for_inbox).await;
                                                }
                                                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                                    tracing::warn!(
                                                        dropped_count = n,
                                                        "Inbox event receiver lagged - {} events dropped",
                                                        n
                                                    );
                                                }
                                                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                                    tracing::info!("Inbox event channel closed");
                                                    break;
                                                }
                                            }
                                        }
                                    });
                                }
                            });
                        }

                        // Switch to main view and stop animation
                        let weak_for_view = weak.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak_for_view.upgrade() {
                                w.set_register_loading(false);
                                w.set_current_view("main".into());
                            }
                        });

                        // Load initial data
                        load_initial_data(weak, &ui_ctx).await;
                    }
                    Err(e) => {
                        // Registration failed - clean up any partial state
                        if let Err(cleanup_err) = auth_service.cleanup_failed_registration().await {
                            tracing::warn!("Failed to cleanup after registration failure: {}", cleanup_err);
                        }

                        let error_msg = e.to_string();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                w.set_register_loading(false);
                                w.set_register_error(error_msg.into());
                            }
                        });
                    }
                }
            });
        },
    );
}

//! Register callback wiring.

use std::sync::Arc;
use std::sync::Mutex;
use lazy_static::lazy_static;

use slint::{ComponentHandle, Image};

use chatter_linux::app::UIContext;
use chatter_linux::media::{FileFilter, FilePicker, FilePickerConfig};

lazy_static! {
    static ref SELECTED_AVATAR: Mutex<Option<Vec<u8>>> = Mutex::new(None);
}

const DEFAULT_AVATARS: &[&[u8]] = &[
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/avatars/pepe.jpg")),
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/avatars/doge.jpg")),
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/avatars/wojak.jpg")),
];

use super::channel_events::spawn_channel_events_handler;
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

                if let Some(w) = weak.upgrade() {
                    w.set_register_has_avatar(true);
                    w.set_register_selected_default_idx(idx as i32);

                    // Load bytes into preview
                    match image::load_from_memory(&bytes) {
                        Ok(img) => {
                            let rgba = img.to_rgba8();
                            let (w_img, h_img) = rgba.dimensions();
                            let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                                rgba.as_raw(),
                                w_img,
                                h_img,
                            );
                            w.set_register_avatar_source(Image::from_rgba8(buffer));
                        }
                        Err(e) => {
                            tracing::error!("Failed to decode default avatar for preview: {}", e);
                        }
                    }
                }
            }
        }
    });

    window.on_register_select_avatar({
        let weak = weak_window.clone();
        move || {
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

                            // Update UI preview
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(w) = weak.upgrade() {
                                    // Reset default index
                                    w.set_register_selected_default_idx(-1);

                                    // Try to load as image for preview
                                    match Image::load_from_path(&path) {
                                        Ok(img) => {
                                            w.set_register_avatar_source(img);
                                            w.set_register_has_avatar(true);
                                        }
                                        Err(e) => {
                                            tracing::error!("Failed to load selected avatar for preview: {}", e);
                                        }
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
    });

    window.on_register(
        move |username: slint::SharedString, password: slint::SharedString, display_name: slint::SharedString| {
            let ui_ctx = ui_ctx.clone();
            let weak = weak_window.clone();
            let username = username.to_string();
            let password = password.to_string();
            let display_name = display_name.to_string();

            tokio::spawn(async move {
                // Set loading state
                let weak_for_loading = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak_for_loading.upgrade() {
                        w.set_register_loading(true);
                        w.set_register_error("".into());
                    }
                });

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
                    .do_register(&username, &password, &display_name, "linux-client", Some(avatar_bytes))
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
                        let _bootstrap_handle = chatter_linux::inbox::spawn_dm_bootstrap_handler_from_bus(
                            ui_ctx.database.clone(),
                            ui_ctx.grpc_pool.clone(),
                            &ui_ctx.event_bus,
                        );
                        tracing::debug!("DM bootstrap handler spawned");

                        // Spawn channel events handler for member join/leave notifications
                        let _channel_events_handle = spawn_channel_events_handler(
                            weak.clone(),
                            &ui_ctx,
                            ui_ctx.channel_event_service.clone(),
                        );
                        tracing::debug!("Channel events handler spawned");

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

                        // Spawn anon DM request handler for processing incoming message requests
                        let _anon_dm_handle = crate::bin_ui::features::modals::spawn_anon_dm_request_handler(
                            weak.clone(),
                            &ui_ctx,
                        );
                        tracing::debug!("Anon DM request handler spawned");

                        // NOW start inbox provider - handlers are already subscribed
                        {
                            let weak_for_inbox = weak.clone();
                            let ui_ctx_for_inbox = ui_ctx.clone();
                            tokio::spawn(async move {
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

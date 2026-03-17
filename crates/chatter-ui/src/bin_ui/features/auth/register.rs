//! Register callback wiring.

use std::sync::Arc;
use std::sync::Mutex;
use lazy_static::lazy_static;

use slint::{ComponentHandle, Image};

use chatter_core::client::ChatterClient;
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

pub fn wire_register(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
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
                if ui_ctx.auth().endpoint() != server_url {
                    if let Err(e) = ui_ctx.auth().set_server_url(&server_url) {
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
                if !ui_ctx.auth().is_connected() {
                    if let Err(e) = ui_ctx.auth().connect().await {
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
                let auth_service = &ui_ctx.auth();
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


                // Register via SDK (handles auth + state + inbox + presence)
                match ui_ctx.register(&username, &password, &display_name, "linux-client", Some(avatar_bytes), Some(&pin)).await {
                    Ok(_result) => {
                        if let Ok(mut guard) = SELECTED_AVATAR.lock() {
                            *guard = None;
                        }

                        // Spawn UI-only event handlers
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
                                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                                    }
                                }
                            });
                        }

                        // Subscribe to raw InboxEvent for UI-level handlers
                        if let Some(mut event_rx) = ui_ctx.start_inbox_provider().await {
                            let weak_for_inbox = weak.clone();
                            let ui_ctx_for_inbox = ui_ctx.clone();
                            tokio::spawn(async move {
                                loop {
                                    match event_rx.recv().await {
                                        Ok(event) => {
                                            super::handle_inbox_event(event, &weak_for_inbox, &ui_ctx_for_inbox).await;
                                        }
                                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                                    }
                                }
                            });
                        }

                        // Switch to main view
                        let weak_for_view = weak.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak_for_view.upgrade() {
                                w.set_register_loading(false);
                                w.set_current_view("main".into());
                            }
                        });

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

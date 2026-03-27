//! Register callback wiring.

use std::sync::{Arc, Mutex, OnceLock};

use slint::{ComponentHandle, Image};

use chatter_core::client::ChatterClient;
#[cfg(not(target_os = "android"))]
use chatter_core::media::{FileFilter, FilePicker, FilePickerConfig};

fn selected_avatar() -> &'static Mutex<Option<Vec<u8>>> {
    static INSTANCE: OnceLock<Mutex<Option<Vec<u8>>>> = OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(None))
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
    if let Ok(mut g) = selected_avatar().lock() { *g = None; }

    window.on_register_select_default_avatar({
        let weak = weak_window.clone();
        move |idx| {
            let idx = idx as usize;
            if idx < DEFAULT_AVATARS.len() {
                let bytes = DEFAULT_AVATARS[idx].to_vec();
                if let Ok(mut g) = selected_avatar().lock() { *g = Some(bytes.clone()); }

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
                crate::rt_handle().spawn(async move {
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
                                if let Ok(mut g) = selected_avatar().lock() { *g = Some(bytes.clone()); }

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
            tracing::info!("REGISTER BUTTON TAPPED: user={}, server={}", username, server_url);
            let ui_ctx = ui_ctx.clone();
            let weak = weak_window.clone();
            let username = username.to_string();
            let password = password.to_string();
            let display_name = display_name.to_string();
            let pin = pin.to_string();
            let server_url = server_url.to_string();

            // Set loading state synchronously (we're on UI thread)
            if let Some(w) = weak.upgrade() {
                w.set_register_loading(true);
                w.set_register_error("".into());
            }

            // Update server URL synchronously
            if ui_ctx.auth().endpoint() != server_url {
                if let Err(e) = ui_ctx.auth().set_server_url(&server_url) {
                    if let Some(w) = weak.upgrade() {
                        w.set_register_loading(false);
                        w.set_register_error(format!("Invalid Server URL: {}", e).into());
                    }
                    return;
                }
            }

            // Prepare avatar bytes synchronously
            let avatar_bytes = {
                let user_avatar = selected_avatar().lock().ok().and_then(|g| g.clone());
                if let Some(bytes) = user_avatar {
                    bytes
                } else {
                    let random_idx = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_millis() as usize % DEFAULT_AVATARS.len();
                    DEFAULT_AVATARS[random_idx].to_vec()
                }
            };

            // Spawn background thread with its own tokio runtime (energy-monitor pattern).
            // Each async operation gets a fresh runtime that can drive I/O independently.
            std::thread::spawn(move || {
                tracing::info!("Register: background thread started, creating runtime...");
                let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");
                tracing::info!("Register: runtime created, entering block_on...");
                rt.block_on(async {
                    tracing::info!("Register: inside block_on, checking connection...");

                // Test raw TCP first
                tracing::info!("Register: testing raw TCP to server...");
                match tokio::net::TcpStream::connect("192.168.0.7:50051").await {
                    Ok(_) => tracing::info!("Register: raw TCP connect SUCCESS"),
                    Err(e) => tracing::error!("Register: raw TCP connect FAILED: {}", e),
                }

                // Connect if needed
                if !ui_ctx.auth().is_connected() {
                    tracing::info!("Register: not connected, calling grpc connect()...");
                    if let Err(e) = ui_ctx.auth().connect().await {
                        let error_msg = format!("Failed to connect: {}", e);
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                w.set_register_loading(false);
                                w.set_register_error(error_msg.into());
                            }
                        });
                        return;
                    }
                }

                tracing::info!("Register: connected! Calling register()...");
                // Register
                match ui_ctx.register(&username, &password, &display_name, "linux-client", Some(&avatar_bytes), Some(&pin)).await {
                    Ok(result) => {
                        tracing::info!("Registration successful!");
                        if let Ok(mut g) = selected_avatar().lock() { *g = None; }

                        // Subscribe to DomainEvent for UI updates
                        {
                            let mut domain_rx = ui_ctx.event_bus().subscribe_domain();
                            let weak_for_domain = weak.clone();
                            let ui_ctx_for_domain = ui_ctx.clone();
                            crate::rt_handle().spawn(async move {
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

                        let has_recovery_codes = result.recovery_codes
                            .as_ref()
                            .is_some_and(|codes| !codes.is_empty());

                        let weak_for_ui = weak.clone();
                        let ui_ctx_for_handlers = ui_ctx.clone();
                        if has_recovery_codes {
                            let codes = result.recovery_codes.unwrap();
                            let _ = slint::invoke_from_event_loop(move || {
                                // Spawn UI event handlers (must be on UI thread)
                                crate::bin_ui::features::modals::spawn_anon_group_invite_handler(weak_for_ui.clone(), &ui_ctx_for_handlers);
                                crate::bin_ui::features::auth::inbox::spawn_anon_group_event_handler(weak_for_ui.clone(), &ui_ctx_for_handlers);
                                crate::bin_ui::features::modals::spawn_anon_dm_request_handler(weak_for_ui.clone(), &ui_ctx_for_handlers);

                                if let Some(w) = weak_for_ui.upgrade() {
                                    let model: Vec<slint::SharedString> = codes.iter()
                                        .map(|c| slint::SharedString::from(c.as_str()))
                                        .collect();
                                    let model_rc = std::rc::Rc::new(slint::VecModel::from(model));
                                    w.set_recovery_codes(slint::ModelRc::from(model_rc));
                                    w.set_register_loading(false);
                                    w.set_current_view("main".into());
                                    w.set_active_modal("recovery-codes".into());
                                }
                            });
                        } else {
                            let _ = slint::invoke_from_event_loop(move || {
                                crate::bin_ui::features::modals::spawn_anon_group_invite_handler(weak_for_ui.clone(), &ui_ctx_for_handlers);
                                crate::bin_ui::features::auth::inbox::spawn_anon_group_event_handler(weak_for_ui.clone(), &ui_ctx_for_handlers);
                                crate::bin_ui::features::modals::spawn_anon_dm_request_handler(weak_for_ui.clone(), &ui_ctx_for_handlers);

                                if let Some(w) = weak_for_ui.upgrade() {
                                    w.set_register_loading(false);
                                    w.set_current_view("main".into());
                                }
                            });
                        }

                        load_initial_data(weak, &ui_ctx).await;
                    }
                    Err(e) => {
                        tracing::error!("Registration failed: {}", e);
                        let _ = ui_ctx.auth().cleanup_failed_registration().await;
                        let error_msg = e.to_string();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                w.set_register_loading(false);
                                w.set_register_error(error_msg.into());
                            }
                        });
                    }
                }
                }); // block_on
            }); // thread::spawn
        },
    );
}

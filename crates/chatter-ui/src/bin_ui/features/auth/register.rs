//! Register callback wiring.

use std::sync::{Arc, Mutex, OnceLock};

use slint::ComponentHandle;

use chatter_core::client::ChatterClient;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
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
                                w.set_register_avatar_source(crate::bin_ui::slint_conversions::rgba_to_slint_image(&pixels, width, height,));
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
            {
                let weak = weak.clone();
                crate::rt_handle().spawn(async move {
                    #[cfg(not(any(target_os = "android", target_os = "ios")))]
                    let picked = {
                        let config = FilePickerConfig {
                            title: Some("Select Avatar".to_string()),
                            filters: vec![FileFilter {
                                name: "Images".to_string(),
                                extensions: vec!["jpg".to_string(), "jpeg".to_string(), "png".to_string(), "webp".to_string()],
                            }],
                            ..Default::default()
                        };
                        FilePicker::pick_file(config).await
                    };
                    // Android has no `rfd` backend; go through the SAF bridge (§112p).
                    #[cfg(target_os = "android")]
                    let picked = crate::platform::android::file_picker::pick_files(&["image/*"])
                        .await
                        .into_iter()
                        .next();
                    #[cfg(target_os = "ios")]
                    let picked: Option<std::path::PathBuf> = {
                        tracing::warn!("File picker not supported on iOS");
                        None
                    };

                    if let Some(path) = picked {
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
                                            w.set_register_avatar_source(crate::bin_ui::slint_conversions::rgba_to_slint_image(&pixels, width, height,));
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
        }
    });

    window.on_register_password_changed({
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx.clone();
        move |password: slint::SharedString| {
            let strength = ui_ctx.password_strength(password.as_str());
            if let Some(w) = weak.upgrade() {
                w.set_register_password_strength(strength);
            }
        }
    });

    window.on_register_pin_changed({
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx.clone();
        move |pin: slint::SharedString| {
            let strength = ui_ctx.pin_strength(pin.as_str());
            if let Some(w) = weak.upgrade() {
                w.set_register_pin_strength(strength);
            }
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
            let shield = ui_ctx.anon_mode_enabled();
            if let Some(w) = weak.upgrade() {
                w.set_register_loading(true);
                w.set_register_error("".into());
                if shield {
                    w.global::<crate::AnonMode>()
                        .set_onboarding_progress(super::shield::CONNECTING_OVER_TOR.into());
                }
            }

            // Update server URL synchronously
            if ui_ctx.endpoint() != server_url {
                if let Err(e) = ui_ctx.set_server_url(&server_url) {
                    if let Some(w) = weak.upgrade() {
                        w.set_register_loading(false);
                        w.set_register_error(format!("Invalid Server URL: {}", e).into());
                        w.global::<crate::AnonMode>().set_onboarding_progress("".into());
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

            // Spawn on the main tokio runtime so that tasks spawned by client.start()
            // (inbox provider, channel streams, etc.) live on the long-lived runtime
            // instead of a temporary one that would be dropped after registration.
            // Using a temporary Runtime::new() + block_on here was the root cause of
            // GRA-215: the inbox polling task was silently killed when the temporary
            // runtime was dropped.
            crate::rt_handle().spawn(async move {
                // Connect if needed
                if !ui_ctx.is_connected() {
                    if let Err(e) = ui_ctx.connect().await {
                        let error_msg = super::shield::connect_error(shield, &e);
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                w.set_register_loading(false);
                                w.set_register_error(error_msg.into());
                                w.global::<crate::AnonMode>().set_onboarding_progress("".into());
                            }
                        });
                        return;
                    }
                }
                if shield {
                    let weak = weak.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.global::<crate::AnonMode>().set_onboarding_progress(
                                "Connected over Tor. Creating your account (includes a short anti-spam puzzle)…".into(),
                            );
                        }
                    });
                }

                tracing::info!("Register: connected! Calling register()...");
                // Register
                match ui_ctx.register(&username, &password, &display_name, &chatter_core::services::link_device_service::LinkDeviceService::current_device_name(), Some(&avatar_bytes), Some(&pin)).await {
                    Ok(result) => {
                        tracing::info!("Registration successful!");
                        if let Ok(mut g) = selected_avatar().lock() { *g = None; }

                        // Subscribe to domain events for UI updates via the SDK facade.
                        {
                            let mut domain_rx = ui_ctx.subscribe_events();
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
                                crate::bin_ui::features::modals::spawn_anon_dm_request_handler(weak_for_ui.clone(), &ui_ctx_for_handlers);
                                crate::bin_ui::features::modals::load_channel_invites(weak_for_ui.clone(), &ui_ctx_for_handlers);

                                if let Some(w) = weak_for_ui.upgrade() {
                                    let model: Vec<slint::SharedString> = codes.iter()
                                        .map(|c| slint::SharedString::from(c.as_str()))
                                        .collect();
                                    let model_rc = std::rc::Rc::new(slint::VecModel::from(model));
                                    w.set_recovery_codes(slint::ModelRc::from(model_rc));
                                    w.set_register_loading(false);
                                    w.global::<crate::AnonMode>().set_onboarding_progress("".into());
                                    // Signed-in state is what app.slint gates the
                                    // shell on; the welcome page is a *view*, not a
                                    // signed-out state. Taking only the view (as
                                    // feat/wire-gaps did) would render the welcome
                                    // page to a shell that still thinks nobody is
                                    // logged in.
                                    w.set_is_signed_in(true);
                                    w.set_current_view("welcome".into());
                                    crate::bin_ui::features::back::push(&w, "recovery-codes");
                                }
                            });
                        } else {
                            let _ = slint::invoke_from_event_loop(move || {
                                crate::bin_ui::features::modals::spawn_anon_group_invite_handler(weak_for_ui.clone(), &ui_ctx_for_handlers);
                                crate::bin_ui::features::modals::spawn_anon_dm_request_handler(weak_for_ui.clone(), &ui_ctx_for_handlers);
                                crate::bin_ui::features::modals::load_channel_invites(weak_for_ui.clone(), &ui_ctx_for_handlers);

                                if let Some(w) = weak_for_ui.upgrade() {
                                    w.set_register_loading(false);
                                    w.global::<crate::AnonMode>().set_onboarding_progress("".into());
                                    // Signed-in state is what app.slint gates the
                                    // shell on; the welcome page is a *view*, not a
                                    // signed-out state. Taking only the view (as
                                    // feat/wire-gaps did) would render the welcome
                                    // page to a shell that still thinks nobody is
                                    // logged in.
                                    w.set_is_signed_in(true);
                                    w.set_current_view("welcome".into());
                                }
                            });
                        }

                        load_initial_data(weak, &ui_ctx).await;
                    }
                    Err(e) => {
                        tracing::error!("Registration failed: {}", e);
                        let _ = ui_ctx.cleanup_failed_registration().await;
                        let error_msg = e.user_message();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                w.set_register_loading(false);
                                w.set_register_error(error_msg.into());
                                w.global::<crate::AnonMode>().set_onboarding_progress("".into());
                            }
                        });
                    }
                }
                }); // rt_handle().spawn
        },
    );
}

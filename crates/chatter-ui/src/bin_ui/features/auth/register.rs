//! Register callback wiring.

use std::sync::{Arc, Mutex, OnceLock};

use slint::ComponentHandle;

use chatter_core::client::ChatterClient;
use chatter_core::error::CryptoError;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
use chatter_core::media::{FileFilter, FilePicker, FilePickerConfig};

fn selected_avatar() -> &'static Mutex<Option<Vec<u8>>> {
    static INSTANCE: OnceLock<Mutex<Option<Vec<u8>>>> = OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(None))
}

use super::initial_data::load_initial_data;
use crate::MainWindow;

fn pin_validation_message(pin: &str) -> String {
    match chatter_core::crypto::svr::validate_pin(pin) {
        Ok(()) => String::new(),
        Err(CryptoError::Validation(message)) => message,
        Err(error) => error.to_string(),
    }
}

fn registration_error_message(message: &str) -> &str {
    message
        .strip_prefix("validation error: ")
        .unwrap_or(message)
}

pub fn wire_register(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let ui_ctx = ui_ctx.clone();
    let weak_window = window.as_weak();

    // Reset selection on startup
    if let Ok(mut g) = selected_avatar().lock() {
        *g = None;
    }
    super::super::avatar_defaults::install(window);
    super::super::avatar_defaults::show_register_default(
        window,
        crate::bin_ui::converters::random_preset_avatar_index(),
    );

    window
        .global::<crate::AuthFlow>()
        .on_register_select_default_avatar({
            let weak = weak_window.clone();
            move |idx| {
                if (0..crate::bin_ui::converters::DEFAULT_AVATARS.len() as i32).contains(&idx) {
                    if let Ok(mut g) = selected_avatar().lock() {
                        *g = None;
                    }
                    if let Some(w) = weak.upgrade() {
                        super::super::avatar_defaults::show_register_default(&w, idx as usize);
                    }
                }
            }
        });

    window
        .global::<crate::AuthFlow>()
        .on_register_remove_avatar({
            let weak = weak_window.clone();
            move || {
                if let Ok(mut g) = selected_avatar().lock() {
                    *g = None;
                }
                if let Some(w) = weak.upgrade() {
                    let idx = w
                        .global::<crate::AuthFlow>()
                        .get_register_selected_default_idx()
                        .max(0) as usize;
                    super::super::avatar_defaults::show_register_default(&w, idx);
                }
            }
        });

    window.global::<crate::AuthFlow>().on_register_select_avatar({
        let weak = weak_window.clone();
        move || {
            {
                let weak = weak.clone();
                crate::rt_handle().spawn(async move {
                    #[cfg(not(any(target_os = "android", target_os = "ios")))]
                    let picked = {
                        let config = FilePickerConfig {
                            title: Some("Select avatar".to_string()),
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
                                // Decode image BEFORE invoke_from_event_loop (already in tokio::spawn)
                                let decoded = image::load_from_memory(&bytes).ok().map(|img| {
                                    let rgba = img.to_rgba8();
                                    let (w_img, h_img) = rgba.dimensions();
                                    (rgba.into_raw(), w_img, h_img)
                                });

                                // Update UI preview
                                let _ = slint::invoke_from_event_loop(move || {
                                    if let Some(w) = weak.upgrade() {
                                        // Create slint::Image from pre-decoded pixels
                                        if let Some((pixels, width, height)) = decoded {
                                            if let Ok(mut g) = selected_avatar().lock() { *g = Some(bytes); }
                                            w.global::<crate::AuthFlow>().set_register_avatar_source(crate::bin_ui::slint_conversions::rgba_to_slint_image(&pixels, width, height,));
                                            w.global::<crate::AuthFlow>().set_register_has_avatar(true);
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

    window
        .global::<crate::AuthFlow>()
        .on_register_username_validation(|username| {
            chatter_core::services::validation::validate_registration_username(username.as_str())
                .err()
                .unwrap_or_default()
                .into()
        });

    window
        .global::<crate::AuthFlow>()
        .on_register_pin_validation(|pin| pin_validation_message(pin.as_str()).into());
    window.global::<crate::AuthFlow>().on_register_clear_error({
        let weak = weak_window.clone();
        move || {
            if let Some(w) = weak.upgrade() {
                w.global::<crate::AuthFlow>().set_register_error("".into());
            }
        }
    });

    window
        .global::<crate::AuthFlow>()
        .on_register_password_changed({
            let weak = weak_window.clone();
            let ui_ctx = ui_ctx.clone();
            move |password: slint::SharedString| {
                let strength = ui_ctx.password_strength(password.as_str());
                if let Some(w) = weak.upgrade() {
                    w.global::<crate::AuthFlow>()
                        .set_register_password_strength(strength);
                }
            }
        });

    window.global::<crate::AuthFlow>().on_register_pin_changed({
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx.clone();
        move |pin: slint::SharedString| {
            let strength = ui_ctx.pin_strength(pin.as_str());
            if let Some(w) = weak.upgrade() {
                w.global::<crate::AuthFlow>()
                    .set_register_pin_strength(strength);
            }
        }
    });

    window.global::<crate::AuthFlow>().on_register(
        move |username: slint::SharedString, password: slint::SharedString, display_name: slint::SharedString, pin: slint::SharedString, server_url: slint::SharedString| {
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
                w.global::<crate::AuthFlow>().set_register_loading(true);
                w.global::<crate::AuthFlow>().set_register_error("".into());
                if shield {
                    w.global::<crate::AnonMode>()
                        .set_onboarding_progress(super::shield::CONNECTING_OVER_TOR.into());
                }
            }

            // Update server URL synchronously
            if ui_ctx.endpoint() != server_url {
                if let Err(e) = ui_ctx.set_server_url(&server_url) {
                    if let Some(w) = weak.upgrade() {
                        w.global::<crate::AuthFlow>().set_register_loading(false);
                        w.global::<crate::AuthFlow>().set_register_error(format!("Invalid Server URL: {}", registration_error_message(&e.to_string())).into());
                        w.global::<crate::AnonMode>().set_onboarding_progress("".into());
                    }
                    return;
                }
            }

            // Prepare the uploaded avatar from the current name and color.
            let avatar_bytes = {
                let user_avatar = selected_avatar().lock().ok().and_then(|g| g.clone());
                if let Some(bytes) = user_avatar {
                    bytes
                } else {
                    let index = weak_window.upgrade()
                        .map(|w| w.global::<crate::AuthFlow>().get_register_selected_default_idx().max(0) as usize)
                        .unwrap_or(0);
                    crate::bin_ui::converters::default_avatar_bytes(index, &display_name).unwrap_or_default()
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
                                w.global::<crate::AuthFlow>().set_register_loading(false);
                                w.global::<crate::AuthFlow>().set_register_error(error_msg.into());
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
                            let welcome_name = display_name.clone();
                            let _ = slint::invoke_from_event_loop(move || {
                                // Spawn UI event handlers (must be on UI thread)
                                crate::bin_ui::features::modals::spawn_anon_group_invite_handler(weak_for_ui.clone(), &ui_ctx_for_handlers);
                                crate::bin_ui::features::modals::spawn_anon_dm_request_handler(weak_for_ui.clone(), &ui_ctx_for_handlers);
                                crate::bin_ui::features::modals::load_channel_invites(weak_for_ui.clone(), &ui_ctx_for_handlers);

                                if let Some(w) = weak_for_ui.upgrade() {
                                    clear_registration_secrets(&w);
                                    let model: Vec<slint::SharedString> = codes.iter()
                                        .map(|c| slint::SharedString::from(c.as_str()))
                                        .collect();
                                    let model_rc = std::rc::Rc::new(slint::VecModel::from(model));
                                    w.global::<crate::AuthFlow>().set_recovery_codes(slint::ModelRc::from(model_rc));
                                    w.global::<crate::AuthFlow>().set_register_loading(false);
                                    w.global::<crate::AnonMode>().set_onboarding_progress("".into());
                                    // Signed-in state is what app.slint gates the
                                    // shell on; the welcome page is a *view*, not a
                                    // signed-out state. Taking only the view (as
                                    // feat/wire-gaps did) would render the welcome
                                    // page to a shell that still thinks nobody is
                                    // logged in.
                                    w.global::<crate::AuthFlow>().set_is_signed_in(true);
                                    w.global::<crate::SidebarPane>().set_user_display_name(welcome_name.into());
                                    w.set_current_view("welcome".into());
                                    crate::bin_ui::features::back::push(&w, "recovery-codes");
                                }
                            });
                        } else {
                            let welcome_name = display_name.clone();
                            let _ = slint::invoke_from_event_loop(move || {
                                crate::bin_ui::features::modals::spawn_anon_group_invite_handler(weak_for_ui.clone(), &ui_ctx_for_handlers);
                                crate::bin_ui::features::modals::spawn_anon_dm_request_handler(weak_for_ui.clone(), &ui_ctx_for_handlers);
                                crate::bin_ui::features::modals::load_channel_invites(weak_for_ui.clone(), &ui_ctx_for_handlers);

                                if let Some(w) = weak_for_ui.upgrade() {
                                    clear_registration_secrets(&w);
                                    w.global::<crate::AuthFlow>().set_register_loading(false);
                                    w.global::<crate::AnonMode>().set_onboarding_progress("".into());
                                    // Signed-in state is what app.slint gates the
                                    // shell on; the welcome page is a *view*, not a
                                    // signed-out state. Taking only the view (as
                                    // feat/wire-gaps did) would render the welcome
                                    // page to a shell that still thinks nobody is
                                    // logged in.
                                    w.global::<crate::AuthFlow>().set_is_signed_in(true);
                                    w.global::<crate::SidebarPane>().set_user_display_name(welcome_name.into());
                                    w.set_current_view("welcome".into());
                                }
                            });
                        }

                        load_initial_data(weak, &ui_ctx).await;
                    }
                    Err(e) => {
                        tracing::error!("Registration failed");
                        let _ = ui_ctx.cleanup_failed_registration().await;
                        let error_msg = e.user_message();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                w.global::<crate::AuthFlow>().set_register_loading(false);
                                w.global::<crate::AuthFlow>().set_register_error(registration_error_message(&error_msg).into());
                                w.global::<crate::AnonMode>().set_onboarding_progress("".into());
                            }
                        });
                    }
                }
                }); // rt_handle().spawn
        },
    );
}

fn clear_registration_secrets(window: &MainWindow) {
    let auth = window.global::<crate::AuthFlow>();
    auth.set_register_password("".into());
    auth.set_register_confirm_password("".into());
    auth.set_register_pin("".into());
}

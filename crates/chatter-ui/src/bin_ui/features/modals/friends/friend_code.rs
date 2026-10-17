//! Friend code operations: generate code, QR, copy, connect via code.
//!
//! Privacy-first friend discovery without username search.

use std::sync::Arc;

use slint::{ComponentHandle, SharedString};

use chatter_core::client::{ChatterClient, FriendCodeConnection, FriendCodeConnectionError};

// refresh_sent_requests removed — core emits FriendRequestsUpdated after
// every send/respond/remove and the dispatcher refreshes the sidebar.
use crate::MainWindow;

/// Wire callback to generate and display the current user's friend code.
///
/// When private_mode is false: generates `chatter://connect/{user_id}` format.
/// When private_mode is true: generates `chatter://private/{base32}` with embedded keys.
pub(crate) fn wire_generate_friend_code(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ui_ctx_gen = ui_ctx.clone();

    window.on_friends_generate_code(move |private_mode: bool| {
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx_gen.clone();

        // Generate appropriate friend code based on mode
        let result = if private_mode {
            ui_ctx.generate_private_friend_code()
        } else {
            ui_ctx.generate_friend_code()
        };

        match result {
            Ok(code) => {
                let mode_str = if private_mode { "private" } else { "regular" };
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        w.set_friends_my_code(code.into());
                        tracing::debug!("Generated {} friend code", mode_str);
                    }
                });
            }
            Err(e) => {
                let error_msg = e.user_message();
                tracing::error!("Failed to generate friend code: {}", e);
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        w.set_friends_code_status(error_msg.into());
                    }
                });
            }
        }
    });

    // Wire QR code generation
    wire_generate_qr_code(window, ui_ctx);

    // Wire copy to clipboard
    wire_copy_friend_code(window, ui_ctx);

    // Wire paste-from-clipboard (Slint's Ctrl+V is unreliable on Wayland
    // because it reads via a different selection protocol than what
    // wl-copy and arboard-with-wayland-data-control write to).
    wire_paste_friend_code(window, ui_ctx);

    #[cfg(target_os = "android")]
    wire_scan_friend_code(window);
}

/// "Scan QR code": the decoded text lands in the friend-code input exactly
/// as Paste would put it there. Connect still has to be tapped, and goes
/// through the same core code router.
#[cfg(target_os = "android")]
fn wire_scan_friend_code(window: &MainWindow) {
    use crate::platform::android::{permissions, qr_scanner};

    let weak_window = window.as_weak();
    window.on_friends_scan_code(move || {
        if let Some(w) = weak_window.upgrade() {
            let rationale = if permissions::has(permissions::CAMERA) {
                ""
            } else {
                "Chatter uses the camera only to read your friend's QR code."
            };
            w.set_friends_code_status(rationale.into());
        }
        let weak = weak_window.clone();
        crate::rt_handle().spawn(async move {
            let outcome = qr_scanner::scan().await;
            let _ = slint::invoke_from_event_loop(move || {
                let Some(w) = weak.upgrade() else { return };
                match outcome {
                    qr_scanner::ScanOutcome::Scanned(text) => {
                        w.set_friends_code_input(text.trim().into());
                        w.set_friends_code_status("Code scanned — tap Connect".into());
                    }
                    qr_scanner::ScanOutcome::Cancelled => w.set_friends_code_status("".into()),
                    qr_scanner::ScanOutcome::CameraDenied => w.set_friends_code_status(
                        "Camera access is off. Paste the code instead, or allow the camera for Chatter in system settings."
                            .into(),
                    ),
                }
            });
        });
    });
}

/// Read the system clipboard via arboard and push the contents into the
/// friend-code input. Hops onto a worker thread so the arboard read
/// doesn't block the UI; result is dispatched back via
/// `invoke_from_event_loop`.
fn wire_paste_friend_code(window: &MainWindow, _ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    window.on_friends_paste_code(move || {
        let weak = weak_window.clone();
        std::thread::spawn(move || {
            // Prefer wl-paste for Wayland (matches the wl-copy used in
            // `wire_copy_friend_code`). Fall back to xclip on X11, then
            // arboard as a final attempt.
            let text = std::process::Command::new("wl-paste")
                .arg("--no-newline")
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
                .or_else(|| {
                    std::process::Command::new("xclip")
                        .args(["-selection", "clipboard", "-o"])
                        .output()
                        .ok()
                        .filter(|o| o.status.success())
                        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
                })
                .or_else(|| {
                    #[cfg(feature = "desktop")]
                    {
                        arboard::Clipboard::new()
                            .ok()
                            .and_then(|mut c| c.get_text().ok())
                    }
                    #[cfg(not(feature = "desktop"))]
                    {
                        None::<String>
                    }
                })
                .or_else(|| {
                    // Android has none of the tools above; read the framework
                    // clipboard directly (OPEN-ITEMS §112f).
                    #[cfg(target_os = "android")]
                    {
                        match crate::platform::android::clipboard::get_text() {
                            Ok(t) => t,
                            Err(e) => {
                                tracing::error!("Paste: clipboard read failed: {}", e);
                                None
                            }
                        }
                    }
                    #[cfg(not(target_os = "android"))]
                    {
                        None::<String>
                    }
                });

            let Some(text) = text else {
                tracing::warn!("Paste: no clipboard tool succeeded");
                return;
            };
            let trimmed = text.trim().to_string();
            if trimmed.is_empty() {
                tracing::warn!("Paste: clipboard empty");
                return;
            }
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    w.set_friends_code_input(trimmed.into());
                    w.set_friends_code_status("Pasted from clipboard".into());
                }
            });
        });
    });
}

/// Wire callback to generate QR code image for the friend code.
fn wire_generate_qr_code(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ui_ctx_qr = ui_ctx.clone();

    window.on_friends_generate_qr(move |private_mode: bool| {
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx_qr.clone();

        // Generate appropriate QR code based on mode (150x150 pixels)
        let result = if private_mode {
            ui_ctx.generate_private_friend_qr(150)
        } else {
            ui_ctx.generate_friend_qr(150)
        };

        match result {
            Ok(png_bytes) => {
                // Decode PNG outside UI thread - this is image processing work
                let decoded = image::load_from_memory(&png_bytes).map(|img| {
                    let rgba = img.to_rgba8();
                    let (width, height) = rgba.dimensions();
                    let pixels = rgba.into_raw();
                    (pixels, width, height)
                });

                match decoded {
                    Ok((pixels, width, height)) => {
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                // UI thread only: create Slint buffer from pre-decoded pixels
                                let mut buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(
                                    width, height,
                                );
                                buffer.make_mut_bytes().copy_from_slice(&pixels);

                                let image = slint::Image::from_rgba8(buffer);
                                w.set_friends_qr_code_image(image);
                                tracing::debug!("Generated QR code image");
                            }
                        });
                    }
                    Err(e) => {
                        tracing::error!("Failed to decode QR code PNG: {}", e);
                    }
                }
            }
            Err(e) => {
                tracing::error!("Failed to generate QR code: {}", e);
            }
        }
    });
}

/// Wire callback to copy friend code to clipboard.
fn wire_copy_friend_code(window: &MainWindow, _ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();

    window.on_friends_copy_code(move || {
        if let Some(w) = weak_window.upgrade() {
            let code = w.get_friends_my_code().to_string();
            if !code.is_empty() {
                // Desktop clipboard access can block while contacting Wayland/X11.
                #[cfg(not(any(target_os = "android", target_os = "ios")))]
                {
                    let code_clone = code.clone();
                    std::thread::spawn(move || {
                        match crate::platform::clipboard::set_text(
                            "Friend code",
                            &code_clone,
                            false,
                        ) {
                            Ok(()) => tracing::info!("Copied friend code to clipboard"),
                            Err(e) => tracing::error!("Failed to copy to clipboard: {}", e),
                        }
                    });
                }
                #[cfg(any(target_os = "android", target_os = "ios"))]
                match crate::platform::clipboard::set_text("Friend code", &code, false) {
                    Ok(()) => {
                        tracing::info!("Copied friend code to clipboard");
                        w.set_friends_code_status("Copied to clipboard!".into());
                    }
                    Err(e) => {
                        tracing::error!("Failed to copy friend code to clipboard: {}", e);
                        w.set_friends_code_status("Couldn't copy — use Show QR".into());
                    }
                }

                // Desktop reports success unconditionally because the copy runs
                // on a detached thread above. Mobile sets its own status in the
                // match arms, so this must NOT also claim success — that is how
                // the button came to say "Copied to clipboard!" on Android while
                // doing nothing at all (OPEN-ITEMS §112f).
                #[cfg(not(any(target_os = "android", target_os = "ios")))]
                w.set_friends_code_status("Copied to clipboard!".into());
            } else {
                tracing::warn!("No friend code to copy");
            }
        }
    });
}

/// Connect using the code itself to choose a friend request or secret chat.
pub(crate) fn wire_connect_via_friend_code(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ui_ctx_connect = ui_ctx.clone();

    window.on_friends_connect_via_code(move |code: SharedString| {
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx_connect.clone();
        let code_str = code.to_string();
        if code_str.trim().is_empty() {
            return;
        }

        let (note, display_name) = weak
            .upgrade()
            .map(|w| {
                (
                    w.get_friends_request_note().to_string(),
                    ui_ctx.auth_state().get().display_name,
                )
            })
            .unwrap_or_default();
        tokio::spawn(async move {
            let is_private = chatter_core::domain::PrivateFriendCode::is_private_code(&code_str);
            let weak_loading = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_loading.upgrade() {
                    w.set_friends_loading(true);
                    w.set_friends_code_status(
                        (if is_private {
                            "Secret chat is getting ready…"
                        } else {
                            "Connecting…"
                        })
                        .into(),
                    );
                }
            });

            let note = note.trim();
            let message = if note.is_empty() { None } else { Some(note) };
            match ui_ctx
                .connect_friend_code(&code_str, message, display_name.as_deref())
                .await
            {
                Ok(FriendCodeConnection::FriendRequest(friendship)) => {
                    tracing::info!(
                        "Sent friend request via code to {}",
                        friendship.addressee_id
                    );
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_friends_loading(false);
                            w.set_friends_code_input("".into());
                            w.set_friends_request_note("".into());
                            w.set_friends_code_status("Friend request sent!".into());
                        }
                    });
                }
                Ok(FriendCodeConnection::SecretChat(dm)) => {
                    tracing::info!("Started secret chat {} with {}", dm.id, dm.peer_id);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            crate::bin_ui::features::sidebar::refresh_chat_list(&w, &ui_ctx);
                            w.set_friends_loading(false);
                            w.set_friends_code_input("".into());
                            w.set_friends_request_note("".into());
                            w.set_friends_code_status("Secret chat started!".into());
                        }
                    });
                }
                Err(FriendCodeConnectionError::ExistingSecretChat(id)) => {
                    tracing::info!(%id, "Friend code points to an existing secret chat");
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_friends_loading(false);
                            w.set_friends_code_input("".into());
                            w.set_friends_request_note("".into());
                            w.set_friends_code_status(
                                "You already have a secret chat with them.".into(),
                            );
                            crate::bin_ui::features::back::reset_to_chats(&w);
                            w.invoke_dm_selected(id.to_string().into());
                        }
                    });
                }
                Err(error) => {
                    if matches!(
                        error,
                        FriendCodeConnectionError::InvalidCode
                            | FriendCodeConnectionError::UsernameInsteadOfCode
                            | FriendCodeConnectionError::ServerCodeInShield
                            | FriendCodeConnectionError::FriendRequest(
                                chatter_core::services::UserError::InvalidInput(_)
                            )
                            | FriendCodeConnectionError::FriendRequest(
                                chatter_core::services::UserError::UserNotFound(_)
                            )
                            | FriendCodeConnectionError::SecretChat(
                                chatter_core::services::anon_dm::AnonDmError::CannotMessageSelf
                            )
                    ) {
                        tracing::info!("Friend code input rejected: {}", error);
                    } else {
                        tracing::error!("Failed to connect via friend code: {}", error);
                    }
                    let message = error.user_message();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_friends_loading(false);
                            w.set_friends_code_status(message.into());
                        }
                    });
                }
            }
        });
    });
}

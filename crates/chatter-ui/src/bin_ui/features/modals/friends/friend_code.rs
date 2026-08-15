//! Friend code operations: generate code, QR, copy, connect via code.
//!
//! Privacy-first friend discovery without username search.

use std::sync::Arc;

use slint::{ComponentHandle, Model, SharedString};

use chatter_core::client::ChatterClient;

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
                let error_msg = format!("{}", e);
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
                        arboard::Clipboard::new().ok().and_then(|mut c| c.get_text().ok())
                    }
                    #[cfg(not(feature = "desktop"))]
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
                let decoded = image::load_from_memory(&png_bytes)
                    .map(|img| {
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
                                let mut buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(width, height);
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
                // Use wl-copy for Wayland (more reliable than arboard on Wayland).
                // Desktop-only: subprocess clipboard tools don't exist on mobile
                // and `arboard` isn't a dependency there.
                #[cfg(not(any(target_os = "android", target_os = "ios")))]
                {
                let code_clone = code.clone();
                std::thread::spawn(move || {
                    // Try wl-copy first (Wayland), fall back to xclip (X11),
                    // then arboard (macOS/Windows, or Linux without either tool).
                    let result = std::process::Command::new("wl-copy")
                        .arg(&code_clone)
                        .status()
                        .or_else(|_| {
                            std::process::Command::new("xclip")
                                .args(["-selection", "clipboard"])
                                .stdin(std::process::Stdio::piped())
                                .spawn()
                                .and_then(|mut child| {
                                    use std::io::Write;
                                    if let Some(stdin) = child.stdin.as_mut() {
                                        stdin.write_all(code_clone.as_bytes())?;
                                    }
                                    child.wait()
                                })
                        });

                    match result {
                        Ok(status) if status.success() => {
                            tracing::info!("Copied friend code to clipboard: {}", code_clone);
                        }
                        _ => {
                            match arboard::Clipboard::new()
                                .and_then(|mut c| c.set_text(code_clone.clone()))
                            {
                                Ok(()) => {
                                    tracing::info!("Copied friend code to clipboard: {}", code_clone);
                                }
                                Err(e) => {
                                    tracing::error!("Failed to copy to clipboard: {}", e);
                                }
                            }
                        }
                    }
                });
                }
                #[cfg(any(target_os = "android", target_os = "ios"))]
                tracing::warn!("Clipboard not supported on mobile");

                w.set_friends_code_status("Copied to clipboard!".into());
            } else {
                tracing::warn!("No friend code to copy");
            }
        }
    });
}

/// Wire callback to connect with a user via friend code.
///
/// Accepts friend codes in formats:
/// - `chatter://connect/{user_id}` (URL format)
/// - `{user_id}` (plain UUID)
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

        // Read the optional intro note on the UI thread before spawning.
        let note = weak
            .upgrade()
            .map(|w| w.get_friends_request_note().to_string())
            .unwrap_or_default();

        tokio::spawn(async move {
            // Set loading state
            let weak_loading = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_loading.upgrade() {
                    w.set_friends_loading(true);
                }
            });

            let note_trimmed = note.trim();
            let note_arg = if note_trimmed.is_empty() { None } else { Some(note_trimmed) };
            match ui_ctx.connect_via_friend_code(&code_str, note_arg).await {
                Ok(friendship) => {
                    let friend_id = friendship.addressee_id.to_string();
                    tracing::info!("Sent friend request via code to {}", friend_id);

                    // Core emits FriendRequestsUpdated — no explicit refresh needed.
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_friends_loading(false);
                            // Clear the input field
                            w.set_friends_code_input("".into());
                            w.set_friends_request_note("".into());
                            // Show success message (if UI supports it)
                            w.set_friends_code_status("Friend request sent!".into());
                        }
                    });
                }
                Err(e) => {
                    let error_msg = format!("{}", e);
                    tracing::error!("Failed to connect via friend code: {}", error_msg);

                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_friends_loading(false);
                            // Show error message
                            w.set_friends_code_status(error_msg.into());
                        }
                    });
                }
            }
        });
    });
}

/// Wire callback to connect privately via anon DM.
///
/// This uses the anonymous DM service which has NO server involvement.
/// The connection is established via blind signature tokens and HPKE encryption.
///
/// Accepts codes in formats:
/// - `chatter://private/{base32}` (NDSS'21 compliant - includes identity key + delivery token)
/// - `chatter://connect/{user_id}` (Legacy - requires prekey fetch, NOT fully private)
/// - `{user_id}` (Legacy - plain UUID)
///
/// # NDSS'21 Compliance
///
/// The `chatter://private/` format embeds all key material needed for initial
/// contact. NO prekey fetch is required, so the server learns NOTHING about
/// who is contacting whom.
pub(crate) fn wire_connect_private(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let auth_state = ui_ctx.auth_state().clone();
    let ui_ctx_private = ui_ctx.clone();

    window.on_friends_connect_private(move |code: SharedString| {
        let weak = weak_window.clone();
        let auth_state = auth_state.clone();
        let ui_ctx = ui_ctx_private.clone();
        let code_str = code.to_string();

        if code_str.trim().is_empty() {
            return;
        }

        tokio::spawn(async move {
            // Set loading state
            let weak_loading = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_loading.upgrade() {
                    w.set_friends_loading(true);
                }
            });

            // Check if this is an NDSS'21 compliant private code
            if chatter_core::domain::PrivateFriendCode::is_private_code(&code_str) {
                // Parse the private friend code (includes identity key + delivery token)
                let friend_code = match chatter_core::domain::PrivateFriendCode::parse(&code_str) {
                    Ok(fc) => fc,
                    Err(e) => {
                        let error_msg = format!("Invalid private code: {}", e);
                        tracing::error!("Failed to parse private friend code: {}", e);

                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                w.set_friends_loading(false);
                                w.set_friends_code_status(error_msg.into());
                            }
                        });
                        return;
                    }
                };

                // Get our display name from auth state
                let my_display_name = auth_state.get().display_name;

                // Start NDSS'21 compliant conversation (NO prekey fetch, full privacy)
                match ui_ctx
                    .start_anon_dm_conversation_private(&friend_code, Some("Hello!"), my_display_name.as_deref())
                    .await
                {
                    Ok(dm) => {
                        let peer_id = dm.peer_id;
                        let peer_id_str = peer_id.to_string();
                        let dm_id = dm.id;
                        let updated_at = dm.updated_at;

                        tracing::info!(
                            "Started NDSS'21-compliant private conversation {} with {} (no prekey fetch)",
                            dm_id,
                            peer_id_str
                        );

                        // Add conversation to sidebar so initiator can see it
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                // Create DM sidebar entry
                                let dm_item = crate::DMListItemData {
                                    channel_id: dm_id.to_string().into(),
                                    user_id: peer_id.to_string().into(),
                                    display_name: "Private (pending)".into(),
                                    avatar_source: slint::Image::default(),
                                    has_avatar: false,
                                    initials: "?".into(), // Unknown until they accept
                                    presence_status: "offline".into(),
                                    last_message: "Waiting for response...".into(),
                                    timestamp: updated_at
                                        .with_timezone(&chrono::Local)
                                        .format("%H:%M")
                                        .to_string()
                                        .into(),
                                    unread_count: 0,
                                    is_typing: false,
                                    is_anon: true,
                                    is_selected: false,
                                };

                                // Add to DM list
                                let current_dms = w.get_dm_conversations();
                                let mut dm_vec: Vec<crate::DMListItemData> = Vec::new();

                                // Copy existing DMs, excluding any with the same peer
                                // (removes old pending conversations when retrying)
                                let new_user_id = dm_item.user_id.as_str();
                                for i in 0..current_dms.row_count() {
                                    if let Some(existing_dm) = current_dms.row_data(i) {
                                        if existing_dm.user_id.as_str() != new_user_id {
                                            dm_vec.push(existing_dm);
                                        }
                                    }
                                }

                                // Add new DM at the start (most recent)
                                dm_vec.insert(0, dm_item);

                                // Update UI
                                w.set_dm_conversations(slint::ModelRc::new(slint::VecModel::from(dm_vec)));

                                w.set_friends_loading(false);
                                w.set_friends_code_input("".into());
                                w.set_friends_code_status("Private conversation started!".into());
                            }
                        });
                    }
                    Err(e) => {
                        let error_msg = format!("{}", e);
                        tracing::error!("Failed to start private conversation: {}", error_msg);

                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                w.set_friends_loading(false);
                                w.set_friends_code_status(error_msg.into());
                            }
                        });
                    }
                }
            } else {
                // Not a private code - show helpful error
                // Private mode requires a chatter://private/ code with embedded keys
                let error_msg = "Private mode requires a chatter://private/ code. Ask your friend to share their private code, or disable private mode.".to_string();
                tracing::warn!(
                    "Private mode enabled but received non-private code: {}",
                    code_str
                );

                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        w.set_friends_loading(false);
                        w.set_friends_code_status(error_msg.into());
                    }
                });
            }
        });
    });
}

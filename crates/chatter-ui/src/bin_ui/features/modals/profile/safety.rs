use super::*;
use std::sync::Mutex;

static VERIFYING_PEERS: Mutex<Vec<String>> = Mutex::new(Vec::new());

pub(super) fn verification_pending_for(user_id: &UserId) -> bool {
    VERIFYING_PEERS.lock().unwrap_or_else(|e| e.into_inner())
        .contains(&user_id.to_string())
}

fn finish_verification(profile_user_id: &str) {
    VERIFYING_PEERS.lock().unwrap_or_else(|e| e.into_inner())
        .retain(|id| id != profile_user_id);
}

pub(super) fn wire(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    // View safety number — generates and displays the 60-digit fingerprint
    let weak_window = window.as_weak();
    let ui_ctx_safety = ui_ctx.clone();
    window
        .global::<crate::ModalHostState>()
        .on_profile_view_safety_number(move || {
            let weak = weak_window.clone();
            let ui_ctx = ui_ctx_safety.clone();

            let profile_user_id = weak.upgrade().map(|w| {
                w.global::<crate::ModalHostState>()
                    .get_profile_user_id()
                    .to_string()
            });
            let profile_user_id = match profile_user_id {
                Some(id) if !id.is_empty() => id,
                _ => {
                    tracing::error!("No profile user ID for safety number");
                    return;
                }
            };

            tokio::spawn(async move {
                let peer_id: UserId = match profile_user_id.parse() {
                    Ok(id) => id,
                    Err(e) => {
                        tracing::error!("Invalid user ID for safety number: {}", e);
                        return;
                    }
                };

                match ui_ctx.get_safety_number(&peer_id).await {
                    Ok(safety_number) => {
                        // 12 groups of 5 digits as Signal lays them out: three
                        // rows of four, so the number never wraps 7 + 5.
                        let groups: Vec<String> = safety_number
                            .chars()
                            .collect::<Vec<_>>()
                            .chunks(5)
                            .map(|chunk| chunk.iter().collect::<String>())
                            .collect();
                        let formatted = groups
                            .chunks(4)
                            .map(|row| row.join(" "))
                            .collect::<Vec<_>>()
                            .join("\n");

                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade().filter(|w| {
                                w.global::<crate::ModalHostState>()
                                    .get_profile_user_id().as_str() == profile_user_id.as_str()
                            }) {
                                w.global::<crate::ModalHostState>()
                                    .set_profile_safety_number(formatted.into());
                                w.global::<crate::ModalHostState>()
                                    .set_profile_show_safety_number(true);
                            }
                        });
                    }
                    Err(e) => {
                        tracing::error!("Failed to generate safety number: {}", e);
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade().filter(|w| {
                                w.global::<crate::ModalHostState>()
                                    .get_profile_user_id().as_str() == profile_user_id.as_str()
                            }) {
                                w.set_toast_message("Could not load the safety number. Try again.".into());
                                w.set_toast_variant("error".into());
                                w.set_toast_visible(true);
                            }
                        });
                    }
                }
            });
        });

    // Toggle verified state
    let weak_window = window.as_weak();
    let ui_ctx_verify = ui_ctx.clone();
    window
        .global::<crate::ModalHostState>()
        .on_profile_toggle_verified(move || {
            let weak = weak_window.clone();
            let ui_ctx = ui_ctx_verify.clone();

            let (profile_user_id, currently_verified) = match weak.upgrade() {
                Some(w) => {
                    let state = w.global::<crate::ModalHostState>();
                    if state.get_profile_verification_pending() {
                        return;
                    }
                    (state.get_profile_user_id().to_string(), state.get_profile_is_verified())
                }
                None => {
                    tracing::debug!("Profile verification tap ignored: window closed");
                    return;
                }
            };

            if profile_user_id.is_empty() {
                tracing::debug!("Profile verification tap ignored: no profile user");
                return;
            }

            let peer_id: UserId = match profile_user_id.parse() {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!("Invalid user ID for verification toggle: {}", e);
                    return;
                }
            };

            {
                let mut verifying = VERIFYING_PEERS.lock().unwrap_or_else(|e| e.into_inner());
                if verifying.contains(&profile_user_id) {
                    return;
                }
                verifying.push(profile_user_id.clone());
            }

            if let Some(w) = weak.upgrade() {
                w.global::<crate::ModalHostState>()
                    .set_profile_verification_pending(true);
            }

            if currently_verified {
                // Local storage can block; keep it off the Slint event loop.
                tokio::spawn(async move {
                    match tokio::task::spawn_blocking(move || ui_ctx.mark_peer_unverified(&peer_id)).await {
                        Ok(Ok(())) => {
                            let _ = slint::invoke_from_event_loop(move || {
                                finish_verification(&profile_user_id);
                                if let Some(w) = weak.upgrade().filter(|w| {
                                    w.global::<crate::ModalHostState>()
                                        .get_profile_user_id().as_str() == profile_user_id.as_str()
                                }) {
                                    w.global::<crate::ModalHostState>()
                                        .set_profile_is_verified(false);
                                    w.global::<crate::ModalHostState>()
                                        .set_profile_verification_pending(false);
                                }
                            });
                        }
                        error => {
                            tracing::error!(?error, "Failed to unverify peer");
                            show_verification_error(weak, profile_user_id);
                        }
                    }
                });
            } else {
                // Verify — async (needs to generate safety number first)
                tokio::spawn(async move {
                    match ui_ctx.mark_peer_verified(&peer_id).await {
                        Ok(()) => {
                            let _ = slint::invoke_from_event_loop(move || {
                                finish_verification(&profile_user_id);
                                if let Some(w) = weak.upgrade().filter(|w| {
                                    w.global::<crate::ModalHostState>()
                                        .get_profile_user_id().as_str() == profile_user_id.as_str()
                                }) {
                                    w.global::<crate::ModalHostState>()
                                        .set_profile_is_verified(true);
                                    w.global::<crate::ModalHostState>()
                                        .set_profile_verification_pending(false);
                                }
                            });
                        }
                        Err(e) => {
                            tracing::error!("Failed to verify peer: {}", e);
                            show_verification_error(weak, profile_user_id);
                        }
                    }
                });
            }
        });
}

fn show_verification_error(weak: slint::Weak<MainWindow>, profile_user_id: String) {
    let _ = slint::invoke_from_event_loop(move || {
        finish_verification(&profile_user_id);
        if let Some(w) = weak.upgrade().filter(|w| {
            w.global::<crate::ModalHostState>().get_profile_user_id().as_str() == profile_user_id.as_str()
        }) {
            w.global::<crate::ModalHostState>()
                .set_profile_verification_pending(false);
            w.set_toast_message("Could not change verification. Try again.".into());
            w.set_toast_variant("error".into());
            w.set_toast_visible(true);
        }
    });
}

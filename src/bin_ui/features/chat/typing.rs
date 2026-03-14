//! Typing indicator callback for DM and channel conversations.
//!
//! Signal-style typing indicator implementation with:
//! - Refresh timer (10s) - re-sends STARTED while typing
//! - Pause timer (3s) - sends STOPPED after user stops typing
//! - Timer presence = typing state (no separate boolean)
//! - Clear on message send

use std::cell::RefCell;
use std::sync::Arc;
use std::time::{Duration, Instant};

use slint::{ComponentHandle, Timer, TimerMode};

use chatter_linux::app::UIContext;
use chatter_linux::domain::anon_dm::AnonDmId;
use chatter_linux::domain::anon_group::AnonGroupId;
use chatter_linux::domain::UserId;
use chatter_linux::mailbox::group::GroupCoordinator;
use chatter_linux::services::mailbox_manager::MailboxManager;
use chatter_linux::services::message::MessageService;
use chatter_linux::services::{build_typing_content, build_typing_content_with_channel};
use chatter_linux::state::AuthState;

use super::helpers::{get_peer_id_from_dm_channel, send_channel_content, send_dm_content};
use crate::MainWindow;

/// Typing refresh interval (Signal: 10 seconds)
const TYPING_REFRESH_MS: u64 = 10_000;

/// Typing pause timeout (Signal: 3 seconds)
const TYPING_PAUSE_MS: u64 = 3_000;

/// Throttle interval for bumpTyping calls (Signal: 300ms)
const TYPING_THROTTLE_MS: u64 = 300;

/// Outgoing typing state for a conversation.
struct OutgoingTypingState {
    /// Timer that re-sends STARTED every 10s (kept alive, fires via Repeated mode)
    _refresh_timer: Timer,
    /// Timer that sends STOPPED after 3s of no input
    pause_timer: Timer,
    /// Last time we called bumpTyping (for throttling)
    last_bump: Instant,
    /// Conversation ID (peer_id for DM, channel_id for channels)
    conversation_id: String,
    /// True if this is a DM conversation
    is_dm: bool,
}

thread_local! {
    /// Thread-local outgoing typing state.
    /// Key: conversation ID (peer_id or channel_id)
    /// Must be thread_local because slint::Timer is not Send.
    static OUTGOING_TYPING: RefCell<Option<OutgoingTypingState>> = const { RefCell::new(None) };
}

/// Clear outgoing typing state (no STOPPED message needed).
/// Called when a message is sent (like Signal's clearTypingTimers).
/// The message itself implies "stopped typing" - receiver clears on message receipt.
pub fn clear_outgoing_typing() {
    OUTGOING_TYPING.with(|state_cell| {
        if let Some(state) = state_cell.borrow_mut().take() {
            tracing::debug!(
                conversation_id = %state.conversation_id,
                "TYPING_SEND: Clearing typing state on message send"
            );
            // Timers are dropped here, stopping them
        }
    });
}

/// Wire typing indicator callback for DM and channel conversations.
pub fn wire_typing_changed(window: &MainWindow, ui_ctx: &Arc<UIContext>) {
    let ui_ctx_for_activity = ui_ctx.clone();
    let auth_state = ui_ctx.auth_state.clone();
    let mailbox_manager = ui_ctx.mailbox_manager.clone();
    let chat_state = ui_ctx.chat_state.clone();
    let message_service = ui_ctx.message_service.clone();
    let group_coordinator = ui_ctx.group_coordinator.clone();
    let anon_dm_service = ui_ctx.anon_dm_service.clone();
    let anon_group_service = ui_ctx.anon_group_service.clone();
    let weak_window = window.as_weak();

    window.on_typing_changed(move |is_typing: bool| {
        // Track user activity for presence updates
        if is_typing {
            let ui_ctx = ui_ctx_for_activity.clone();
            tokio::spawn(async move {
                crate::bin_ui::features::auth::presence_integration::track_activity(&ui_ctx).await;
            });
        }
        // Get current conversation context
        let (selected_dm_id, peer_user_id, selected_channel_id, selected_private_group_id) = weak_window
            .upgrade()
            .map(|w| {
                let dm_id = w.get_selected_dm_id().to_string();
                let peer_id = if !dm_id.is_empty() {
                    get_peer_id_from_dm_channel(&w, &dm_id)
                } else {
                    None
                };
                let channel_state = chat_state.get();
                let channel_id = channel_state.active_channel_id.map(|id| id.to_string());
                let group_id = w.get_selected_private_group_id().to_string();
                (dm_id, peer_id, channel_id, group_id)
            })
            .unwrap_or_default();

        // Handle anonymous DMs using the same timer-based pattern as regular DMs
        if let Some(anon_dm_id_str) = selected_dm_id.strip_prefix("anon_dm:") {
            let conversation_id = format!("anon_dm:{}", anon_dm_id_str);

            if is_typing {
                // Use bump_typing pattern with anon DM service
                bump_typing_anon_dm(
                    &conversation_id,
                    anon_dm_id_str.to_string(),
                    anon_dm_service.clone(),
                );
            } else {
                // User explicitly stopped typing - clear state and send STOPPED
                OUTGOING_TYPING.with(|state_cell| {
                    let state = state_cell.borrow_mut().take();
                    if let Some(state) = state {
                        if state.conversation_id == conversation_id {
                            tracing::debug!(
                                conversation_id = %conversation_id,
                                "TYPING_SEND: User stopped typing explicitly (anon DM)"
                            );

                            let anon_dm_id: AnonDmId = match anon_dm_id_str.parse() {
                                Ok(id) => id,
                                Err(_) => return,
                            };
                            let service = anon_dm_service.clone();
                            tokio::spawn(async move {
                                let _ = service.send_typing_indicator(&anon_dm_id, false).await;
                            });
                        }
                    }
                });
            }
            return;
        }

        // Handle anonymous groups using the same timer-based pattern
        if !selected_private_group_id.is_empty() {
            let conversation_id = format!("anon_group:{}", selected_private_group_id);

            if is_typing {
                // Use bump_typing pattern with anon group service
                bump_typing_anon_group(
                    &conversation_id,
                    selected_private_group_id.clone(),
                    anon_group_service.clone(),
                );
            } else {
                // User explicitly stopped typing - clear state and send STOPPED
                OUTGOING_TYPING.with(|state_cell| {
                    let state = state_cell.borrow_mut().take();
                    if let Some(state) = state {
                        if state.conversation_id == conversation_id {
                            tracing::debug!(
                                conversation_id = %conversation_id,
                                "TYPING_SEND: User stopped typing explicitly (anon group)"
                            );

                            let anon_group_id: AnonGroupId = match selected_private_group_id.parse() {
                                Ok(id) => id,
                                Err(_) => return,
                            };
                            let service = anon_group_service.clone();
                            tokio::spawn(async move {
                                let _ = service.send_typing_indicator(&anon_group_id, false).await;
                            });
                        }
                    }
                });
            }
            return;
        }

        let is_dm = !selected_dm_id.is_empty();
        let conversation_id = if is_dm {
            peer_user_id.clone().unwrap_or_default()
        } else {
            selected_channel_id.clone().unwrap_or_default()
        };

        if conversation_id.is_empty() {
            return;
        }

        // Skip if typing to self
        if let Some(user_id) = auth_state.get().current_user_id {
            if conversation_id == user_id.to_string() {
                return;
            }
        }

        if is_typing {
            // User started typing - use bumpTyping pattern
            bump_typing(
                &conversation_id,
                is_dm,
                auth_state.clone(),
                mailbox_manager.clone(),
                message_service.clone(),
                group_coordinator.clone(),
            );
        } else {
            // User explicitly stopped typing (e.g., backspaced to empty)
            // Clear state and send STOPPED
            OUTGOING_TYPING.with(|state_cell| {
                let state = state_cell.borrow_mut().take();
                if let Some(state) = state {
                    if state.conversation_id == conversation_id {
                        tracing::debug!(
                            conversation_id = %conversation_id,
                            "TYPING_SEND: User stopped typing explicitly"
                        );

                        // Send STOPPED
                        let message_service = message_service.clone();
                        let mailbox_manager = mailbox_manager.clone();
                        let group_coordinator = group_coordinator.clone();
                        let auth_state = auth_state.clone();
                        let conv_id = conversation_id.clone();

                        tokio::spawn(async move {
                            if !auth_state.is_logged_in() {
                                return;
                            }

                            if is_dm {
                                let peer_id: UserId = match conv_id.parse() {
                                    Ok(id) => id,
                                    Err(_) => return,
                                };
                                let content = build_typing_content(false);
                                let _ = send_dm_content(content, &peer_id, message_service, mailbox_manager).await;
                            } else {
                                let channel_id: chatter_linux::domain::ChannelId = match conv_id.parse() {
                                    Ok(id) => id,
                                    Err(_) => return,
                                };
                                let content = build_typing_content_with_channel(false, Some(&conv_id));
                                let _ = send_channel_content(content, &channel_id, message_service, group_coordinator).await;
                            }
                        });
                    }
                }
            });
        }
    });
}

/// Signal-style bumpTyping - called on each keystroke, throttled.
fn bump_typing(
    conversation_id: &str,
    is_dm: bool,
    auth_state: AuthState,
    mailbox_manager: Arc<MailboxManager>,
    message_service: Arc<MessageService>,
    group_coordinator: Arc<GroupCoordinator>,
) {
    OUTGOING_TYPING.with(|state_cell| {
        let mut state_opt = state_cell.borrow_mut();

        // Check if we have existing state for this conversation
        let needs_new_state = match &*state_opt {
            Some(state) => state.conversation_id != conversation_id,
            None => true,
        };

        if needs_new_state {
            // Different conversation or no state - create new
            // First, drop old state (sends STOPPED if needed via Drop? No, we handle manually)
            if let Some(old_state) = state_opt.take() {
                // Send STOPPED for old conversation
                let old_conv_id = old_state.conversation_id.clone();
                let old_is_dm = old_state.is_dm;
                let message_service = message_service.clone();
                let mailbox_manager = mailbox_manager.clone();
                let group_coordinator = group_coordinator.clone();
                let auth_state = auth_state.clone();

                tokio::spawn(async move {
                    if !auth_state.is_logged_in() {
                        return;
                    }

                    if old_is_dm {
                        let peer_id: UserId = match old_conv_id.parse() {
                            Ok(id) => id,
                            Err(_) => return,
                        };
                        let content = build_typing_content(false);
                        let _ = send_dm_content(content, &peer_id, message_service, mailbox_manager).await;
                    } else {
                        let channel_id: chatter_linux::domain::ChannelId = match old_conv_id.parse() {
                            Ok(id) => id,
                            Err(_) => return,
                        };
                        let content = build_typing_content_with_channel(false, Some(&old_conv_id));
                        let _ = send_channel_content(content, &channel_id, message_service, group_coordinator).await;
                    }
                });
            }

            // Create new state with timers
            let refresh_timer = Timer::default();
            let pause_timer = Timer::default();

            // Send initial STARTED
            tracing::info!(
                conversation_id = %conversation_id,
                is_dm,
                "TYPING_SEND: Starting typing (first keystroke)"
            );

            let conv_id_send = conversation_id.to_string();
            let ms = message_service.clone();
            let mm = mailbox_manager.clone();
            let gc = group_coordinator.clone();
            let auth = auth_state.clone();

            tokio::spawn(async move {
                if !auth.is_logged_in() {
                    return;
                }

                if is_dm {
                    let peer_id: UserId = match conv_id_send.parse() {
                        Ok(id) => id,
                        Err(_) => return,
                    };
                    let content = build_typing_content(true);
                    let _ = send_dm_content(content, &peer_id, ms, mm).await;
                } else {
                    let channel_id: chatter_linux::domain::ChannelId = match conv_id_send.parse() {
                        Ok(id) => id,
                        Err(_) => return,
                    };
                    let content = build_typing_content_with_channel(true, Some(&conv_id_send));
                    let _ = send_channel_content(content, &channel_id, ms, gc).await;
                }
            });

            // Set up refresh timer (re-sends STARTED every 10s)
            setup_refresh_timer(
                &refresh_timer,
                conversation_id.to_string(),
                is_dm,
                auth_state.clone(),
                mailbox_manager.clone(),
                message_service.clone(),
                group_coordinator.clone(),
            );

            // Set up pause timer (sends STOPPED after 3s)
            setup_pause_timer(
                &pause_timer,
                conversation_id.to_string(),
                is_dm,
                auth_state.clone(),
                mailbox_manager.clone(),
                message_service.clone(),
                group_coordinator.clone(),
            );

            *state_opt = Some(OutgoingTypingState {
                _refresh_timer: refresh_timer,
                pause_timer,
                last_bump: Instant::now(),
                conversation_id: conversation_id.to_string(),
                is_dm,
            });
        } else {
            // Same conversation - just reset pause timer (throttled)
            let state = state_opt.as_mut().unwrap();
            let now = Instant::now();

            // Throttle to 300ms like Signal
            if now.duration_since(state.last_bump) < Duration::from_millis(TYPING_THROTTLE_MS) {
                return;
            }

            state.last_bump = now;

            // Reset pause timer
            setup_pause_timer(
                &state.pause_timer,
                conversation_id.to_string(),
                is_dm,
                auth_state.clone(),
                mailbox_manager.clone(),
                message_service.clone(),
                group_coordinator.clone(),
            );
        }
    });
}

/// Set up refresh timer - re-sends STARTED every 10s.
fn setup_refresh_timer(
    timer: &Timer,
    conversation_id: String,
    is_dm: bool,
    auth_state: AuthState,
    mailbox_manager: Arc<MailboxManager>,
    message_service: Arc<MessageService>,
    group_coordinator: Arc<GroupCoordinator>,
) {
    let conv_id = conversation_id.clone();
    let ms = message_service.clone();
    let mm = mailbox_manager.clone();
    let gc = group_coordinator.clone();
    let auth = auth_state.clone();

    timer.start(
        TimerMode::Repeated,
        Duration::from_millis(TYPING_REFRESH_MS),
        move || {
            // Re-send STARTED to keep typing indicator alive
            tracing::debug!(
                conversation_id = %conv_id,
                "TYPING_SEND: Refresh timer - re-sending STARTED"
            );

            let conv_id = conv_id.clone();
            let ms = ms.clone();
            let mm = mm.clone();
            let gc = gc.clone();
            let auth = auth.clone();

            tokio::spawn(async move {
                if !auth.is_logged_in() {
                    return;
                }

                if is_dm {
                    let peer_id: UserId = match conv_id.parse() {
                        Ok(id) => id,
                        Err(_) => return,
                    };
                    let content = build_typing_content(true);
                    let _ = send_dm_content(content, &peer_id, ms, mm).await;
                } else {
                    let channel_id: chatter_linux::domain::ChannelId = match conv_id.parse() {
                        Ok(id) => id,
                        Err(_) => return,
                    };
                    let content = build_typing_content_with_channel(true, Some(&conv_id));
                    let _ = send_channel_content(content, &channel_id, ms, gc).await;
                }
            });
        },
    );
}

/// Set up pause timer - sends STOPPED after 3s and clears state.
fn setup_pause_timer(
    timer: &Timer,
    conversation_id: String,
    is_dm: bool,
    auth_state: AuthState,
    mailbox_manager: Arc<MailboxManager>,
    message_service: Arc<MessageService>,
    group_coordinator: Arc<GroupCoordinator>,
) {
    let conv_id = conversation_id.clone();

    timer.start(
        TimerMode::SingleShot,
        Duration::from_millis(TYPING_PAUSE_MS),
        move || {
            tracing::debug!(
                conversation_id = %conv_id,
                "TYPING_SEND: Pause timer fired - sending STOPPED"
            );

            // Clear state
            OUTGOING_TYPING.with(|state_cell| {
                let state = state_cell.borrow_mut().take();
                if let Some(state) = state {
                    if state.conversation_id == conv_id {
                        // Send STOPPED
                        let ms = message_service.clone();
                        let mm = mailbox_manager.clone();
                        let gc = group_coordinator.clone();
                        let auth = auth_state.clone();
                        let conv_id = conv_id.clone();

                        tokio::spawn(async move {
                            if !auth.is_logged_in() {
                                return;
                            }

                            if is_dm {
                                let peer_id: UserId = match conv_id.parse() {
                                    Ok(id) => id,
                                    Err(_) => return,
                                };
                                let content = build_typing_content(false);
                                let _ = send_dm_content(content, &peer_id, ms, mm).await;
                            } else {
                                let channel_id: chatter_linux::domain::ChannelId = match conv_id.parse() {
                                    Ok(id) => id,
                                    Err(_) => return,
                                };
                                let content = build_typing_content_with_channel(false, Some(&conv_id));
                                let _ = send_channel_content(content, &channel_id, ms, gc).await;
                            }
                        });
                    } else {
                        // Conversation changed, put state back
                        *state_cell.borrow_mut() = Some(state);
                    }
                }
            });
        },
    );
}

// ============================================================================
// Anonymous DM typing indicator support
// ============================================================================

use chatter_linux::services::{AnonDmService, AnonGroupService};

/// Signal-style bumpTyping for anonymous DMs - called on each keystroke, throttled.
fn bump_typing_anon_dm(
    conversation_id: &str,
    anon_dm_id_str: String,
    anon_dm_service: Arc<AnonDmService>,
) {
    OUTGOING_TYPING.with(|state_cell| {
        let mut state_opt = state_cell.borrow_mut();

        // Check if we have existing state for this conversation
        let needs_new_state = match &*state_opt {
            Some(state) => state.conversation_id != conversation_id,
            None => true,
        };

        if needs_new_state {
            // Different conversation or no state - create new
            // First, send STOPPED for old conversation if it was also an anon DM
            if let Some(old_state) = state_opt.take() {
                if let Some(old_anon_id_str) = old_state.conversation_id.strip_prefix("anon_dm:") {
                    let old_id: AnonDmId = match old_anon_id_str.parse() {
                        Ok(id) => id,
                        Err(_) => {
                            // Not a valid anon DM ID, skip
                            return;
                        }
                    };
                    let service = anon_dm_service.clone();
                    tokio::spawn(async move {
                        let _ = service.send_typing_indicator(&old_id, false).await;
                    });
                }
                // Note: If old conversation was regular DM/channel, it will have
                // its own timers that will fire. This is acceptable.
            }

            // Create new state with timers
            let refresh_timer = Timer::default();
            let pause_timer = Timer::default();

            // Send initial STARTED
            tracing::info!(
                conversation_id = %conversation_id,
                "TYPING_SEND: Starting typing (first keystroke, anon DM)"
            );

            let anon_dm_id: AnonDmId = match anon_dm_id_str.parse() {
                Ok(id) => id,
                Err(_) => return,
            };
            let service = anon_dm_service.clone();
            tokio::spawn(async move {
                let _ = service.send_typing_indicator(&anon_dm_id, true).await;
            });

            // Set up refresh timer (re-sends STARTED every 10s)
            setup_refresh_timer_anon_dm(
                &refresh_timer,
                conversation_id.to_string(),
                anon_dm_id_str.clone(),
                anon_dm_service.clone(),
            );

            // Set up pause timer (sends STOPPED after 3s)
            setup_pause_timer_anon_dm(
                &pause_timer,
                conversation_id.to_string(),
                anon_dm_id_str,
                anon_dm_service,
            );

            *state_opt = Some(OutgoingTypingState {
                _refresh_timer: refresh_timer,
                pause_timer,
                last_bump: Instant::now(),
                conversation_id: conversation_id.to_string(),
                is_dm: true, // Anon DMs are always DMs
            });
        } else {
            // Same conversation - just reset pause timer (throttled)
            let state = state_opt.as_mut().unwrap();
            let now = Instant::now();

            // Throttle to 300ms like Signal
            if now.duration_since(state.last_bump) < Duration::from_millis(TYPING_THROTTLE_MS) {
                return;
            }

            state.last_bump = now;

            // Reset pause timer
            let anon_dm_id_str_clone = match conversation_id.strip_prefix("anon_dm:") {
                Some(s) => s.to_string(),
                None => return,
            };
            setup_pause_timer_anon_dm(
                &state.pause_timer,
                conversation_id.to_string(),
                anon_dm_id_str_clone,
                anon_dm_service,
            );
        }
    });
}

/// Set up refresh timer for anon DM - re-sends STARTED every 10s.
fn setup_refresh_timer_anon_dm(
    timer: &Timer,
    conversation_id: String,
    anon_dm_id_str: String,
    anon_dm_service: Arc<AnonDmService>,
) {
    let conv_id = conversation_id.clone();
    let id_str = anon_dm_id_str.clone();
    let service = anon_dm_service.clone();

    timer.start(
        TimerMode::Repeated,
        Duration::from_millis(TYPING_REFRESH_MS),
        move || {
            tracing::debug!(
                conversation_id = %conv_id,
                "TYPING_SEND: Refresh timer - re-sending STARTED (anon DM)"
            );

            let anon_dm_id: AnonDmId = match id_str.parse() {
                Ok(id) => id,
                Err(_) => return,
            };
            let service = service.clone();
            tokio::spawn(async move {
                let _ = service.send_typing_indicator(&anon_dm_id, true).await;
            });
        },
    );
}

/// Set up pause timer for anon DM - sends STOPPED after 3s and clears state.
fn setup_pause_timer_anon_dm(
    timer: &Timer,
    conversation_id: String,
    anon_dm_id_str: String,
    anon_dm_service: Arc<AnonDmService>,
) {
    let conv_id = conversation_id.clone();
    let id_str = anon_dm_id_str.clone();
    let service = anon_dm_service.clone();

    timer.start(
        TimerMode::SingleShot,
        Duration::from_millis(TYPING_PAUSE_MS),
        move || {
            tracing::debug!(
                conversation_id = %conv_id,
                "TYPING_SEND: Pause timer fired - sending STOPPED (anon DM)"
            );

            // Clear state
            OUTGOING_TYPING.with(|state_cell| {
                let state = state_cell.borrow_mut().take();
                if let Some(state) = state {
                    if state.conversation_id == conv_id {
                        let anon_dm_id: AnonDmId = match id_str.parse() {
                            Ok(id) => id,
                            Err(_) => return,
                        };
                        let service = service.clone();
                        tokio::spawn(async move {
                            let _ = service.send_typing_indicator(&anon_dm_id, false).await;
                        });
                    } else {
                        // Conversation changed, put state back
                        *state_cell.borrow_mut() = Some(state);
                    }
                }
            });
        },
    );
}

// ============================================================================
// Anonymous Group typing indicator support
// ============================================================================

/// Signal-style bumpTyping for anonymous groups - called on each keystroke, throttled.
fn bump_typing_anon_group(
    conversation_id: &str,
    anon_group_id_str: String,
    anon_group_service: Arc<AnonGroupService>,
) {
    OUTGOING_TYPING.with(|state_cell| {
        let mut state_opt = state_cell.borrow_mut();

        // Check if we have existing state for this conversation
        let needs_new_state = match &*state_opt {
            Some(state) => state.conversation_id != conversation_id,
            None => true,
        };

        if needs_new_state {
            // Different conversation or no state - create new
            // First, send STOPPED for old conversation if it was also an anon group
            if let Some(old_state) = state_opt.take() {
                if let Some(old_anon_id_str) = old_state.conversation_id.strip_prefix("anon_group:") {
                    let old_id: AnonGroupId = match old_anon_id_str.parse() {
                        Ok(id) => id,
                        Err(_) => {
                            // Not a valid anon group ID, skip
                            return;
                        }
                    };
                    let service = anon_group_service.clone();
                    tokio::spawn(async move {
                        let _ = service.send_typing_indicator(&old_id, false).await;
                    });
                }
            }

            // Create new state with timers
            let refresh_timer = Timer::default();
            let pause_timer = Timer::default();

            // Send initial STARTED
            tracing::info!(
                conversation_id = %conversation_id,
                "TYPING_SEND: Starting typing (first keystroke, anon group)"
            );

            let anon_group_id: AnonGroupId = match anon_group_id_str.parse() {
                Ok(id) => id,
                Err(_) => return,
            };
            let service = anon_group_service.clone();
            tokio::spawn(async move {
                let _ = service.send_typing_indicator(&anon_group_id, true).await;
            });

            // Set up refresh timer (re-sends STARTED every 10s)
            setup_refresh_timer_anon_group(
                &refresh_timer,
                conversation_id.to_string(),
                anon_group_id_str.clone(),
                anon_group_service.clone(),
            );

            // Set up pause timer (sends STOPPED after 3s)
            setup_pause_timer_anon_group(
                &pause_timer,
                conversation_id.to_string(),
                anon_group_id_str,
                anon_group_service,
            );

            *state_opt = Some(OutgoingTypingState {
                _refresh_timer: refresh_timer,
                pause_timer,
                last_bump: Instant::now(),
                conversation_id: conversation_id.to_string(),
                is_dm: false, // Anon groups are not DMs
            });
        } else {
            // Same conversation - just reset pause timer (throttled)
            let state = state_opt.as_mut().unwrap();
            let now = Instant::now();

            // Throttle to 300ms like Signal
            if now.duration_since(state.last_bump) < Duration::from_millis(TYPING_THROTTLE_MS) {
                return;
            }

            state.last_bump = now;

            // Reset pause timer
            let anon_group_id_str_clone = match conversation_id.strip_prefix("anon_group:") {
                Some(s) => s.to_string(),
                None => return,
            };
            setup_pause_timer_anon_group(
                &state.pause_timer,
                conversation_id.to_string(),
                anon_group_id_str_clone,
                anon_group_service,
            );
        }
    });
}

/// Set up refresh timer for anon group - re-sends STARTED every 10s.
fn setup_refresh_timer_anon_group(
    timer: &Timer,
    conversation_id: String,
    anon_group_id_str: String,
    anon_group_service: Arc<AnonGroupService>,
) {
    let conv_id = conversation_id.clone();
    let id_str = anon_group_id_str.clone();
    let service = anon_group_service.clone();

    timer.start(
        TimerMode::Repeated,
        Duration::from_millis(TYPING_REFRESH_MS),
        move || {
            tracing::debug!(
                conversation_id = %conv_id,
                "TYPING_SEND: Refresh timer - re-sending STARTED (anon group)"
            );

            let anon_group_id: AnonGroupId = match id_str.parse() {
                Ok(id) => id,
                Err(_) => return,
            };
            let service = service.clone();
            tokio::spawn(async move {
                let _ = service.send_typing_indicator(&anon_group_id, true).await;
            });
        },
    );
}

/// Set up pause timer for anon group - sends STOPPED after 3s and clears state.
fn setup_pause_timer_anon_group(
    timer: &Timer,
    conversation_id: String,
    anon_group_id_str: String,
    anon_group_service: Arc<AnonGroupService>,
) {
    let conv_id = conversation_id.clone();
    let id_str = anon_group_id_str.clone();
    let service = anon_group_service.clone();

    timer.start(
        TimerMode::SingleShot,
        Duration::from_millis(TYPING_PAUSE_MS),
        move || {
            tracing::debug!(
                conversation_id = %conv_id,
                "TYPING_SEND: Pause timer fired - sending STOPPED (anon group)"
            );

            // Clear state
            OUTGOING_TYPING.with(|state_cell| {
                let state = state_cell.borrow_mut().take();
                if let Some(state) = state {
                    if state.conversation_id == conv_id {
                        let anon_group_id: AnonGroupId = match id_str.parse() {
                            Ok(id) => id,
                            Err(_) => return,
                        };
                        let service = service.clone();
                        tokio::spawn(async move {
                            let _ = service.send_typing_indicator(&anon_group_id, false).await;
                        });
                    } else {
                        // Conversation changed, put state back
                        *state_cell.borrow_mut() = Some(state);
                    }
                }
            });
        },
    );
}

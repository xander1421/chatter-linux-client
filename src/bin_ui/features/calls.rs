//! Call UI callback wiring.
//!
//! Wires call-related Slint callbacks to CallService.

use std::cell::RefCell;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use slint::{ComponentHandle, Model, ModelRc, Timer, TimerMode, VecModel};

use chrono::{Duration, Utc};
use uuid::Uuid;

use chatter_linux::app::AppContext;
use chatter_linux::calls::{AudioCapture, LiveKitRoomEvent, ScreenCapture, ScreenSourceType};
use chatter_linux::domain::call::CallType;
use chatter_linux::domain::ids::{CallId, ChannelId, MessageId, UserId};
use chatter_linux::domain::message::{CallMessage, CallMessageType, Message, MessageContent};
use chatter_linux::inbox::handlers::DomainEvent;
use chatter_linux::services::CallWorkerHandle;
use chatter_linux::services::mailbox_manager::MailboxManager;
use chatter_linux::services::message_service::MessageService;
use chatter_linux::storage::{ChannelRepository, Database, MessageRepository, UserRepository};
use crate::{ActiveCallData, AudioDeviceData, CallParticipantData, MainWindow, ScreenSourceData};

thread_local! {
    /// Call duration timer (must be on UI thread).
    static CALL_TIMER: RefCell<Option<Timer>> = const { RefCell::new(None) };
    /// Call start timestamp (nanoseconds since epoch) for synchronized duration.
    static CALL_START_NS: RefCell<Option<u64>> = const { RefCell::new(None) };
    /// Video frame polling timer (30fps for smooth video).
    static VIDEO_FRAME_TIMER: RefCell<Option<Timer>> = const { RefCell::new(None) };
}

/// Wire all call-related callbacks.
pub fn wire(window: &MainWindow, ctx: &AppContext) {
    wire_start_call(window, ctx);
    wire_incoming_call(window, ctx);
    wire_active_call(window, ctx);
    wire_room_events(window, ctx);
    wire_device_picker(window, ctx);
    wire_call_hangup_handler(window, ctx);
}

/// Get current time in nanoseconds since epoch.
fn current_time_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// Start the call duration timer.
///
/// # Arguments
/// * `call_start_ns` - Optional call start timestamp in nanoseconds since epoch.
///                     If None, uses current time (caller side).
///                     If Some, uses provided timestamp (receiver side for sync).
fn start_call_timer(window: &MainWindow, call_start_ns: Option<u64>) {
    let weak = window.as_weak();

    // Use provided timestamp or current time
    let start_timestamp = call_start_ns.unwrap_or_else(current_time_ns);

    CALL_START_NS.with(|start| {
        *start.borrow_mut() = Some(start_timestamp);
    });

    // Update UI immediately with current elapsed time
    let now = current_time_ns();
    let initial_elapsed = if now > start_timestamp {
        ((now - start_timestamp) / 1_000_000_000) as i32
    } else {
        0
    };

    if let Some(w) = weak.upgrade() {
        let mut data = w.get_active_call_data();
        data.duration_seconds = initial_elapsed;
        w.set_active_call_data(data);
    }

    CALL_TIMER.with(|timer_cell| {
        // Stop existing timer if any
        if let Some(ref timer) = *timer_cell.borrow() {
            timer.stop();
        }

        // Create new timer that updates every second
        let timer = Timer::default();
        timer.start(
            TimerMode::Repeated,
            std::time::Duration::from_secs(1),
            move || {
                if let Some(w) = weak.upgrade() {
                    CALL_START_NS.with(|start| {
                        if let Some(start_timestamp) = *start.borrow() {
                            let now = current_time_ns();
                            let elapsed = if now > start_timestamp {
                                ((now - start_timestamp) / 1_000_000_000) as i32
                            } else {
                                0
                            };
                            let mut data = w.get_active_call_data();
                            data.duration_seconds = elapsed;
                            w.set_active_call_data(data);
                        }
                    });
                }
            },
        );
        *timer_cell.borrow_mut() = Some(timer);
    });

    tracing::debug!(
        start_timestamp_ns = start_timestamp,
        initial_elapsed_secs = initial_elapsed,
        "Call timer started"
    );
}

/// Stop the call duration timer.
fn stop_call_timer() {
    CALL_TIMER.with(|timer_cell| {
        if let Some(ref timer) = *timer_cell.borrow() {
            timer.stop();
        }
        *timer_cell.borrow_mut() = None;
    });

    CALL_START_NS.with(|start| {
        *start.borrow_mut() = None;
    });

    tracing::debug!("Call timer stopped");
}

/// Start the video frame polling timer.
///
/// Polls SharedVideoFrames at ~30fps and updates Slint image properties.
fn start_video_frame_timer(window: &MainWindow, call_service: &CallWorkerHandle) {
    let weak = window.as_weak();
    let shared_frames = call_service.video_frames().clone();

    VIDEO_FRAME_TIMER.with(|timer_cell| {
        // Stop existing timer if any
        if let Some(ref timer) = *timer_cell.borrow() {
            timer.stop();
        }

        // Create new timer that polls at ~30fps (33ms interval)
        let timer = Timer::default();
        let first_frame_logged = std::cell::Cell::new(false);
        timer.start(
            TimerMode::Repeated,
            std::time::Duration::from_millis(33),
            move || {
                if let Some(w) = weak.upgrade() {
                    // Get all frames (camera + screen share) as Slint images
                    let all_frames = shared_frames.get_all_slint_images();

                    if !all_frames.is_empty() {
                        // Log first frame received
                        if !first_frame_logged.get() {
                            first_frame_logged.set(true);
                            tracing::info!(
                                "Video frame timer: First frames received, updating UI with {} remote video frames",
                                all_frames.len()
                            );
                        }

                        // Convert to Slint array
                        let remote_images: Vec<slint::Image> = all_frames
                            .into_iter()
                            .map(|(_, image)| image)
                            .collect();

                        // Update remote video frames
                        w.set_call_remote_video_frames(
                            ModelRc::new(VecModel::from(remote_images))
                        );
                    }
                }
            },
        );
        *timer_cell.borrow_mut() = Some(timer);
    });

    tracing::debug!("Video frame timer started");
}

/// Stop the video frame polling timer.
fn stop_video_frame_timer() {
    VIDEO_FRAME_TIMER.with(|timer_cell| {
        if let Some(ref timer) = *timer_cell.borrow() {
            timer.stop();
        }
        *timer_cell.borrow_mut() = None;
    });

    tracing::debug!("Video frame timer stopped");
}

/// Get the current call duration in seconds.
///
/// Returns 0 if no call is active or timer hasn't started.
fn get_call_duration_secs() -> i64 {
    CALL_START_NS.with(|start| {
        if let Some(start_ns) = *start.borrow() {
            let now = current_time_ns();
            if now > start_ns {
                ((now - start_ns) / 1_000_000_000) as i64
            } else {
                0
            }
        } else {
            0
        }
    })
}

/// Clean up call UI state when a call ends remotely (via hangup notification).
///
/// This must be called from the main UI thread via `invoke_from_event_loop`.
/// Use this from inbox_events.rs when receiving CallHangup.
pub fn cleanup_call_ui_state() {
    stop_call_timer();
    stop_video_frame_timer();
    tracing::debug!("Call UI state cleaned up (remote hangup)");
}

/// Create and save a call history message to the database and UI.
///
/// # Arguments
/// * `database` - Database connection
/// * `channel_id` - Channel where the call occurred
/// * `call_id` - Unique call identifier
/// * `initiator_id` - User who initiated the call
/// * `initiator_name` - Display name of the initiator
/// * `is_outgoing` - True if we initiated the call
/// * `duration_secs` - Call duration in seconds (0 for missed calls)
/// * `is_missed` - True if the call was missed/declined
fn create_call_history_message(
    database: Arc<Database>,
    channel_id: &ChannelId,
    call_id: &str,
    initiator_id: &UserId,
    initiator_name: &str,
    is_outgoing: bool,
    duration_secs: i64,
    is_missed: bool,
) {
    // Create call message content
    let call_type = if is_missed {
        CallMessageType::Missed
    } else {
        CallMessageType::Ended
    };

    let duration = if duration_secs > 0 {
        Some(Duration::seconds(duration_secs))
    } else {
        None
    };

    let call_msg = CallMessage {
        call_id: call_id.to_string(),
        call_type,
        initiator_id: *initiator_id,
        initiator_name: initiator_name.to_string(),
        started_at: None, // We don't track this currently
        ended_at: Some(Utc::now()),
        duration,
        participant_count: 2, // DM calls have 2 participants
    };

    // Create the message
    let message = Message {
        id: MessageId(Uuid::new_v4()),
        channel_id: *channel_id,
        sender_id: Some(*initiator_id),
        content: MessageContent::Call(call_msg),
        thread_id: None,
        reply_to: None,
        attachments: vec![],
        is_pinned: false,
        is_edited: false,
        is_deleted: false,
        sent_at: current_time_ns(),
        created_at: Utc::now(),
        updated_at: Utc::now(),
        edited_at: None,
        reaction_counts: vec![],
        current_user_reactions: vec![],
        mentioned_integrations: vec![],
        targets_ai_bot: false,
    };

    // Save to database
    let message_repo = MessageRepository::new(database);
    if let Err(e) = message_repo.insert(&message) {
        tracing::error!(error = %e, "Failed to save call history message");
    } else {
        tracing::info!(
            call_id = call_id,
            channel_id = %channel_id,
            is_outgoing = is_outgoing,
            is_missed = is_missed,
            duration_secs = duration_secs,
            "Saved call history message"
        );
    }
}

/// Wire room events to update UI (participant count, etc.).
fn wire_room_events(window: &MainWindow, ctx: &AppContext) {
    let weak = window.as_weak();
    let call_service = ctx.call_service.clone();

    // Spawn a tokio task to consume room events (tokio runtime needed for channels and sleep)
    tokio::spawn(async move {
        // Wait for call service to be initialized
        let service = loop {
            if let Some(svc) = call_service.get() {
                break svc.clone();
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        };

        // Take the room events receiver
        let Some(mut rx) = service.take_room_events() else {
            tracing::warn!("Room events receiver already taken");
            return;
        };

        tracing::info!("Started room events listener");

        while let Some(event) = rx.recv().await {
            tracing::debug!("UI received room event: {:?}", event);
            let weak = weak.clone();

            match event {
                LiveKitRoomEvent::StateChanged(state) => {
                    if let chatter_linux::calls::RoomState::Connected { participant_count, .. } = state {
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                let mut data = w.get_active_call_data();
                                data.participant_count = participant_count as i32;
                                w.set_active_call_data(data);
                            }
                        });
                    }
                }
                LiveKitRoomEvent::ParticipantJoined { identity, display_name } => {
                    tracing::info!("Participant joined: {} ({:?})", identity, display_name);
                    let display = display_name.clone().unwrap_or_else(|| identity.clone());
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            let mut data = w.get_active_call_data();
                            data.participant_count += 1;

                            // Add participant to list
                            let new_participant = CallParticipantData {
                                user_id: identity.into(),
                                display_name: display.into(),
                                avatar_url: "".into(),
                                is_muted: false,
                                is_video_enabled: false,
                                is_screen_sharing: false,
                                is_speaking: false,
                            };

                            let mut participants: Vec<CallParticipantData> =
                                (0..data.participants.row_count())
                                    .map(|i| data.participants.row_data(i).unwrap())
                                    .collect();
                            // Insert remote participants at the beginning, keeping local participant last
                            // This matches the Slint layout which assumes remote participants come first
                            participants.insert(0, new_participant);
                            data.participants = ModelRc::new(VecModel::from(participants));

                            w.set_active_call_data(data);
                            tracing::debug!("UI updated for participant join");
                        }
                    });
                }
                LiveKitRoomEvent::ParticipantLeft { identity } => {
                    tracing::info!("Participant left: {}", identity);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            let mut data = w.get_active_call_data();
                            data.participant_count = (data.participant_count - 1).max(0);

                            // Remove participant from list
                            let participants: Vec<CallParticipantData> =
                                (0..data.participants.row_count())
                                    .filter_map(|i| {
                                        let p = data.participants.row_data(i)?;
                                        if p.user_id.to_string() != identity {
                                            Some(p)
                                        } else {
                                            None
                                        }
                                    })
                                    .collect();
                            data.participants = ModelRc::new(VecModel::from(participants));

                            w.set_active_call_data(data);
                            tracing::debug!("UI updated for participant leave");
                        }
                    });
                }
                LiveKitRoomEvent::TrackSubscribed { participant_identity, track_sid, is_video, is_screen_share } => {
                    tracing::info!(
                        "Track subscribed: {} from {} (video: {}, screen_share: {})",
                        track_sid, participant_identity, is_video, is_screen_share
                    );

                    // Update participant's video/screen share state
                    if is_video {
                        let identity = participant_identity.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                let mut data = w.get_active_call_data();
                                let participants: Vec<CallParticipantData> =
                                    (0..data.participants.row_count())
                                        .map(|i| {
                                            let mut p = data.participants.row_data(i).unwrap();
                                            if p.user_id.to_string() == identity {
                                                if is_screen_share {
                                                    p.is_screen_sharing = true;
                                                } else {
                                                    p.is_video_enabled = true;
                                                }
                                            }
                                            p
                                        })
                                        .collect();
                                data.participants = ModelRc::new(VecModel::from(participants));
                                w.set_active_call_data(data);
                                tracing::debug!(
                                    "Updated participant {} video_enabled={} screen_sharing={}",
                                    identity, !is_screen_share, is_screen_share
                                );
                            }
                        });
                    }
                }
                LiveKitRoomEvent::TrackUnsubscribed { participant_identity, track_sid } => {
                    tracing::info!(
                        "Track unsubscribed: {} from {}",
                        track_sid, participant_identity
                    );
                    // Note: We don't know if this was video or audio track
                    // A more robust solution would include track type in the event
                }
                LiveKitRoomEvent::EncryptionStateChanged { participant_identity, is_encrypted } => {
                    tracing::debug!(
                        "Encryption state changed: {} - encrypted: {}",
                        participant_identity, is_encrypted
                    );
                }
            }
        }

        tracing::info!("Room events listener stopped");
    });
}

fn wire_start_call(window: &MainWindow, ctx: &AppContext) {
    let weak = window.as_weak();
    let ctx_call_service = ctx.call_service.clone();
    let ctx_settings = ctx.settings_repo.clone();
    let ctx_message_service = ctx.message_service.clone();
    let ctx_database = ctx.database.clone();
    let ctx_mailbox_manager = ctx.mailbox_manager.clone();
    let ctx_channel_repo = ctx.channel_repo.clone();
    let ctx_identity_manager = ctx.identity_manager.clone();
    let ctx_key_store = ctx.key_store.clone();

    // Start voice call
    window.on_start_voice_call({
        let weak = weak.clone();
        let call_service = ctx_call_service.clone();
        let settings = ctx_settings.clone();
        let message_service = ctx_message_service.clone();
        let database = ctx_database.clone();
        let mailbox_manager = ctx_mailbox_manager.clone();
        let channel_repo = ctx_channel_repo.clone();
        let identity_manager = ctx_identity_manager.clone();
        let key_store = ctx_key_store.clone();

        move || {
            let weak = weak.clone();
            let settings = settings.clone();
            let message_service = message_service.clone();
            let database = database.clone();
            let mailbox_manager = mailbox_manager.clone();
            let channel_repo = channel_repo.clone();
            let identity_manager = identity_manager.clone();
            let key_store = key_store.clone();

            // Get the call service from OnceLock
            let call_service = match call_service.get() {
                Some(svc) => svc.clone(),
                None => {
                    tracing::error!("CallService not initialized");
                    return;
                }
            };

            slint::spawn_local(async move {
                start_call_impl(
                    weak,
                    call_service,
                    settings,
                    CallType::Voice,
                    message_service,
                    database,
                    mailbox_manager,
                    channel_repo,
                    identity_manager,
                    key_store,
                ).await;
            }).unwrap();
        }
    });

    // Start video call
    window.on_start_video_call({
        let weak = weak.clone();
        let call_service = ctx_call_service.clone();
        let settings = ctx_settings.clone();
        let message_service = ctx_message_service.clone();
        let database = ctx_database.clone();
        let mailbox_manager = ctx_mailbox_manager.clone();
        let channel_repo = ctx_channel_repo.clone();
        let identity_manager = ctx_identity_manager.clone();
        let key_store = ctx_key_store.clone();

        move || {
            let weak = weak.clone();
            let settings = settings.clone();
            let message_service = message_service.clone();
            let database = database.clone();
            let mailbox_manager = mailbox_manager.clone();
            let channel_repo = channel_repo.clone();
            let identity_manager = identity_manager.clone();
            let key_store = key_store.clone();

            // Get the call service from OnceLock
            let call_service = match call_service.get() {
                Some(svc) => svc.clone(),
                None => {
                    tracing::error!("CallService not initialized");
                    return;
                }
            };

            slint::spawn_local(async move {
                start_call_impl(
                    weak,
                    call_service,
                    settings,
                    CallType::Video,
                    message_service,
                    database,
                    mailbox_manager,
                    channel_repo,
                    identity_manager,
                    key_store,
                ).await;
            }).unwrap();
        }
    });
}

#[allow(clippy::too_many_arguments)]
async fn start_call_impl(
    weak: slint::Weak<MainWindow>,
    call_service: CallWorkerHandle,
    settings: Arc<chatter_linux::storage::SettingsRepository>,
    call_type: CallType,
    message_service: Arc<MessageService>,
    database: Arc<Database>,
    mailbox_manager: Arc<MailboxManager>,
    channel_repo: Arc<ChannelRepository>,
    identity_manager: Arc<chatter_linux::crypto::IdentityKeyManager>,
    key_store: Arc<chatter_linux::storage::KeyStore>,
) {
    // Get current channel ID and name from the window
    // Check both selected-channel-id (for channels) and selected-dm-id (for DMs)
    let (channel_id_str, channel_name, is_dm) = if let Some(w) = weak.upgrade() {
        let channel_id = w.get_selected_channel_id().to_string();
        let dm_id = w.get_selected_dm_id().to_string();

        if !channel_id.is_empty() {
            (channel_id, w.get_channel_name().to_string(), false)
        } else if !dm_id.is_empty() {
            // For DMs, the dm_id IS the channel_id
            (dm_id, w.get_channel_name().to_string(), true)
        } else {
            (String::new(), String::new(), false)
        }
    } else {
        return;
    };

    if channel_id_str.is_empty() {
        tracing::warn!("No channel or DM selected for call");
        return;
    }

    let channel_id = match ChannelId::from_str(&channel_id_str) {
        Ok(id) => id,
        Err(e) => {
            tracing::error!("Invalid channel ID: {}", e);
            return;
        }
    };

    // Get current user ID and display name
    let user_id_str = match settings.get_string(chatter_linux::storage::settings_keys::CURRENT_USER_ID) {
        Ok(Some(id)) => id,
        _ => {
            tracing::error!("No user ID available");
            return;
        }
    };

    // Display name shown as "You" for local participant
    let display_name = "You".to_string();

    let user_id = match UserId::from_str(&user_id_str) {
        Ok(id) => id,
        Err(e) => {
            tracing::error!("Invalid user ID: {}", e);
            return;
        }
    };

    // Derive encryption key from identity keys
    let encryption_key = if is_dm {
        // For DM calls, derive from ECDH(our_identity, peer_identity)
        match derive_call_encryption_key(&identity_manager, &key_store, &channel_repo, &channel_id, &user_id) {
            Ok(key) => key,
            Err(e) => {
                // No secure channel established yet - user needs to send a message first
                tracing::warn!(
                    "Cannot start call - no secure channel with peer: {}. Send a message first to establish encryption.",
                    e
                );
                return;
            }
        }
    } else {
        // For group calls, use sender key derivation (not yet implemented)
        tracing::warn!("Group call encryption not yet implemented, using placeholder key");
        [0u8; 32]
    };

    let is_video_call = matches!(call_type, CallType::Video);
    let call_type_str = match call_type {
        CallType::Voice => "Voice Call",
        CallType::Video => "Video Call",
        CallType::Group => "Group Call",
    };

    tracing::info!("Starting {} in channel {}", call_type_str, channel_id);

    match call_service.initiate_call(
        channel_id,
        call_type_str,
        call_type,
        &encryption_key,
        user_id,
    ).await {
        Ok(call) => {
            tracing::info!("Started call: {:?}", call.id);

            // Publish microphone immediately after joining
            if let Err(e) = call_service.publish_microphone(None).await {
                tracing::error!("Failed to publish microphone: {}", e);
            } else {
                tracing::info!("Microphone published successfully");
            }

            // Publish camera if video call
            if is_video_call {
                if let Err(e) = call_service.publish_camera(None).await {
                    tracing::error!("Failed to publish camera: {}", e);
                } else {
                    tracing::info!("Camera published successfully");
                }
            }

            // For DM calls, send call notification to peer via sealed sender
            if is_dm {
                // Get the peer user ID from the DM channel
                if let Ok(Some(peer_id)) = get_dm_peer_id(&channel_repo, &channel_id, &user_id) {
                    tracing::info!(
                        peer_id = %peer_id,
                        call_id = %call.id,
                        "Sending call notification to peer via sealed sender"
                    );

                    // Build call key content
                    let content = chatter_linux::services::build_call_key_content(
                        &call.id.to_string(),
                        encryption_key.to_vec(),
                        call_type_str,
                        &channel_id_str,
                    );

                    // Send via DM (sealed sender)
                    if let Err(e) = message_service
                        .send_dm(&peer_id, content, database.clone(), &mailbox_manager)
                        .await
                    {
                        tracing::error!(
                            peer_id = %peer_id,
                            error = %e,
                            "Failed to send call notification"
                        );
                    } else {
                        tracing::info!(
                            peer_id = %peer_id,
                            call_id = %call.id,
                            "Call notification sent successfully"
                        );
                    }
                } else {
                    tracing::warn!(
                        channel_id = %channel_id,
                        "Could not determine peer ID for DM call notification"
                    );
                }
            }

            if let Some(w) = weak.upgrade() {
                // Create local participant
                let local_participant = CallParticipantData {
                    user_id: user_id_str.into(),
                    display_name: display_name.into(),
                    avatar_url: "".into(),
                    is_muted: false,
                    is_video_enabled: is_video_call,
                    is_screen_sharing: false,
                    is_speaking: false,
                };

                // Derive verification emojis for secure call verification
                let verification = chatter_linux::services::CallVerification::derive_verification_emojis(
                    &encryption_key,
                    &call.id.to_string(),
                );
                let (e1, e2, e3, e4) = chatter_linux::services::CallVerification::to_slint_tuple(&verification);

                // Create call data for UI
                let call_data = ActiveCallData {
                    call_id: call.id.to_string().into(),
                    channel_id: channel_id_str.into(),
                    title: if channel_name.is_empty() { call_type_str.into() } else { channel_name.into() },
                    is_video_call,
                    is_encrypted: true,
                    participant_count: 1,
                    duration_seconds: 0,
                    participants: ModelRc::new(VecModel::from(vec![local_participant])),
                    verification_emoji_1: e1.into(),
                    verification_emoji_2: e2.into(),
                    verification_emoji_3: e3.into(),
                    verification_emoji_4: e4.into(),
                };

                w.set_active_call_data(call_data);
                w.set_call_is_muted(false);
                w.set_call_is_video_enabled(is_video_call);
                w.set_call_is_screen_sharing(false);
                w.set_active_call_visible(true);

                // Start call duration timer (caller uses current time)
                start_call_timer(&w, None);

                // Start video frame polling timer
                start_video_frame_timer(&w, &call_service);
            }
        }
        Err(e) => {
            tracing::error!("Failed to start call: {}", e);
        }
    }
}

/// Get the peer's user ID from a DM channel.
///
/// DM channels have exactly 2 members - we return the one that's not the current user.
fn get_dm_peer_id(
    channel_repo: &ChannelRepository,
    channel_id: &ChannelId,
    current_user_id: &UserId,
) -> Result<Option<UserId>, Box<dyn std::error::Error + Send + Sync>> {
    let memberships = channel_repo.list_members(channel_id)?;

    for membership in memberships {
        if membership.user_id != *current_user_id {
            return Ok(Some(membership.user_id));
        }
    }

    Ok(None)
}

/// Derive call encryption key from identity keys for DM calls.
///
/// Uses ECDH between our identity key and peer's identity key.
fn derive_call_encryption_key(
    identity_manager: &chatter_linux::crypto::IdentityKeyManager,
    key_store: &chatter_linux::storage::KeyStore,
    channel_repo: &ChannelRepository,
    channel_id: &ChannelId,
    current_user_id: &UserId,
) -> Result<[u8; 32], Box<dyn std::error::Error + Send + Sync>> {
    use libsignal_protocol::IdentityKey;

    // Get our identity key pair
    let (_, our_identity) = identity_manager
        .get_identity()?
        .ok_or("No local identity key")?;

    // Get peer's identity from DM channel
    let peer_id = get_dm_peer_id(channel_repo, channel_id, current_user_id)?
        .ok_or("Could not find peer in DM channel")?;

    // Get peer's identity key from key store
    // Use UUID-based lookup to find any device's identity key for this peer
    // This handles cases where device ID might not be .1
    let peer_identity_bytes = key_store
        .get_identity_by_uuid(&peer_id.to_string())?
        .ok_or_else(|| format!(
            "No identity key found for peer {} (no messages received from them yet?)",
            peer_id
        ))?;

    // Parse peer's identity key (stored as serialized IdentityKey bytes)
    // Use try_from to match how the Signal store decodes identity keys
    let peer_identity = IdentityKey::try_from(peer_identity_bytes.as_slice())
        .map_err(|e| format!("Failed to decode peer identity key ({} bytes): {:?}", peer_identity_bytes.len(), e))?;

    // Derive shared secret using ECDH
    let shared_secret = chatter_linux::crypto::derive_call_shared_secret(&our_identity, &peer_identity)?;

    Ok(shared_secret)
}

fn wire_incoming_call(window: &MainWindow, ctx: &AppContext) {
    let weak = window.as_weak();
    let ctx_call_service = ctx.call_service.clone();
    let ctx_settings = ctx.settings_repo.clone();

    // Accept call callback
    window.on_accept_incoming_call({
        let weak = weak.clone();
        let call_service = ctx_call_service.clone();
        let settings = ctx_settings.clone();

        move |call_id| {
            let call_id_str = call_id.to_string();
            tracing::info!("Accept call: {}", call_id_str);

            let weak = weak.clone();
            let call_service = call_service.clone();
            let settings = settings.clone();

            slint::spawn_local(async move {
                // Get incoming call data to extract encryption key and timestamp
                let (encryption_key_b64, is_video_call, caller_name, call_start_timestamp_ns) = if let Some(w) = weak.upgrade() {
                    let data = w.get_incoming_call_data();
                    // Parse timestamp from string (nanoseconds since epoch)
                    let timestamp_ns = data.call_start_timestamp_ns.to_string()
                        .parse::<u64>()
                        .ok();
                    (
                        data.encryption_key.to_string(),
                        data.is_video_call,
                        data.caller_name.to_string(),
                        timestamp_ns,
                    )
                } else {
                    return;
                };

                // Decode encryption key from base64
                let encryption_key: [u8; 32] = match base64::Engine::decode(
                    &base64::engine::general_purpose::STANDARD,
                    &encryption_key_b64,
                ) {
                    Ok(bytes) if bytes.len() == 32 => {
                        let mut key = [0u8; 32];
                        key.copy_from_slice(&bytes);
                        key
                    }
                    Ok(bytes) => {
                        tracing::error!("Invalid encryption key length: {} (expected 32)", bytes.len());
                        [0u8; 32] // Fallback to zeroed key
                    }
                    Err(e) => {
                        tracing::error!("Failed to decode encryption key: {}", e);
                        [0u8; 32] // Fallback to zeroed key
                    }
                };

                // Parse call ID
                let call_id = match CallId::from_str(&call_id_str) {
                    Ok(id) => id,
                    Err(e) => {
                        tracing::error!("Invalid call ID: {}", e);
                        return;
                    }
                };

                // Get current user ID
                let user_id_str = match settings.get_string(chatter_linux::storage::settings_keys::CURRENT_USER_ID) {
                    Ok(Some(id)) => id,
                    _ => {
                        tracing::error!("No user ID available");
                        return;
                    }
                };

                let user_id = match UserId::from_str(&user_id_str) {
                    Ok(id) => id,
                    Err(e) => {
                        tracing::error!("Invalid user ID: {}", e);
                        return;
                    }
                };

                // Get the CallService from context (same instance used for room events)
                let service = match call_service.get() {
                    Some(svc) => svc.clone(),
                    None => {
                        tracing::error!("CallService not initialized");
                        if let Some(w) = weak.upgrade() {
                            w.set_incoming_call_visible(false);
                        }
                        return;
                    }
                };

                match service.join_call(call_id, &encryption_key, user_id).await {
                    Ok(call) => {
                        tracing::info!("Joined call: {:?}", call.id);

                        // Publish microphone immediately after joining
                        if let Err(e) = service.publish_microphone(None).await {
                            tracing::error!("Failed to publish microphone: {}", e);
                        } else {
                            tracing::info!("Microphone published successfully");
                        }

                        // Update UI
                        if let Some(w) = weak.upgrade() {
                            w.set_incoming_call_visible(false);

                            // Create local participant
                            let local_participant = CallParticipantData {
                                user_id: user_id_str.clone().into(),
                                display_name: "You".into(),
                                avatar_url: "".into(),
                                is_muted: false,
                                is_video_enabled: is_video_call,
                                is_screen_sharing: false,
                                is_speaking: false,
                            };

                            // Create caller participant
                            let caller_participant = CallParticipantData {
                                user_id: call.initiator_id.to_string().into(),
                                display_name: caller_name.into(),
                                avatar_url: "".into(),
                                is_muted: false,
                                is_video_enabled: is_video_call,
                                is_screen_sharing: false,
                                is_speaking: false,
                            };

                            // Derive verification emojis for secure call verification
                            let verification = chatter_linux::services::CallVerification::derive_verification_emojis(
                                &encryption_key,
                                &call.id.to_string(),
                            );
                            let (e1, e2, e3, e4) = chatter_linux::services::CallVerification::to_slint_tuple(&verification);

                            // Create call data for UI
                            let call_data = ActiveCallData {
                                call_id: call.id.to_string().into(),
                                channel_id: call.channel_id.to_string().into(),
                                title: "Call".into(),
                                is_video_call,
                                is_encrypted: true,
                                participant_count: 2, // We + caller
                                duration_seconds: 0,
                                participants: ModelRc::new(VecModel::from(vec![caller_participant, local_participant])),
                                verification_emoji_1: e1.into(),
                                verification_emoji_2: e2.into(),
                                verification_emoji_3: e3.into(),
                                verification_emoji_4: e4.into(),
                            };

                            w.set_active_call_data(call_data);
                            w.set_call_is_muted(false);
                            w.set_call_is_video_enabled(is_video_call);
                            w.set_call_is_screen_sharing(false);
                            w.set_active_call_visible(true);

                            // Start call duration timer (receiver uses caller's timestamp for sync)
                            start_call_timer(&w, call_start_timestamp_ns);

                            // Start video frame polling timer
                            start_video_frame_timer(&w, &service);

                            // Publish camera if video call
                            if is_video_call {
                                let service = service.clone();
                                slint::spawn_local(async move {
                                    if let Err(e) = service.publish_camera(None).await {
                                        tracing::error!("Failed to publish camera: {}", e);
                                    } else {
                                        tracing::info!("Camera published successfully");
                                    }
                                }).unwrap();
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!("Failed to join call: {}", e);
                        if let Some(w) = weak.upgrade() {
                            w.set_incoming_call_visible(false);
                        }
                    }
                }
            }).unwrap();
        }
    });

    let weak = window.as_weak();
    let ctx_grpc_decline = ctx.grpc_pool.clone();

    // Decline call callback
    window.on_decline_incoming_call(move |call_id| {
        let call_id_str = call_id.to_string();
        tracing::info!("Decline call: {}", call_id_str);

        let weak = weak.clone();
        let grpc = ctx_grpc_decline.clone();

        // Hide the modal immediately
        if let Some(w) = weak.upgrade() {
            w.set_incoming_call_visible(false);
        }

        // Notify server that call was declined by attempting to leave
        slint::spawn_local(async move {
            let mut call_client = chatter_linux::network::grpc::CallClient::new(
                grpc.channel(),
                grpc.auth_clone(),
            );

            // Leave call signals decline (we never joined, so this just marks our intent)
            if let Err(e) = call_client.leave_call(&call_id_str).await {
                // This error is expected if we never joined - just log at debug level
                tracing::debug!("Decline call notification: {}", e);
            } else {
                tracing::info!("Call decline notification sent for {}", call_id_str);
            }
        }).unwrap();
    });
}

fn wire_active_call(window: &MainWindow, ctx: &AppContext) {
    let call_service = ctx.call_service.clone();

    // Toggle mute
    window.on_call_toggle_mute({
        let call_service = call_service.clone();
        let weak = window.as_weak();
        move || {
            let call_service = call_service.clone();
            let weak = weak.clone();
            slint::spawn_local(async move {
                if let Some(service) = call_service.get() {
                    let is_muted = service.is_microphone_muted().await;
                    let new_state = !is_muted;
                    service.set_microphone_muted(new_state);
                    tracing::info!("Microphone muted: {}", new_state);

                    if let Some(w) = weak.upgrade() {
                        w.set_call_is_muted(new_state);
                    }
                }
            }).unwrap();
        }
    });

    // Toggle video
    window.on_call_toggle_video({
        let call_service = call_service.clone();
        let weak = window.as_weak();
        move || {
            let call_service = call_service.clone();
            let weak = weak.clone();
            slint::spawn_local(async move {
                if let Some(service) = call_service.get() {
                    let is_enabled = service.is_camera_enabled().await;
                    let new_state = !is_enabled;
                    service.set_camera_enabled(new_state);
                    tracing::info!("Camera enabled: {}", new_state);

                    if let Some(w) = weak.upgrade() {
                        w.set_call_is_video_enabled(new_state);
                    }
                }
            }).unwrap();
        }
    });

    // Toggle screen share
    window.on_call_toggle_screen_share({
        let call_service = call_service.clone();
        let weak = window.as_weak();
        move || {
            let call_service = call_service.clone();
            let weak = weak.clone();
            slint::spawn_local(async move {
                if let Some(service) = call_service.get() {
                    let is_sharing = service.is_screen_sharing().await;
                    let new_state = !is_sharing;

                    if new_state {
                        // Start screen share - use primary display
                        match chatter_linux::calls::ScreenCapture::primary_source() {
                            Ok(source) => {
                                if let Err(e) = service.publish_screen_share(source).await {
                                    tracing::error!("Failed to start screen share: {}", e);
                                } else {
                                    tracing::info!("Screen share started");
                                    if let Some(w) = weak.upgrade() {
                                        w.set_call_is_screen_sharing(true);
                                    }
                                }
                            }
                            Err(e) => {
                                tracing::error!("No screen source available: {}", e);
                            }
                        }
                    } else {
                        // Stop screen share
                        service.stop_screen_share();
                        tracing::info!("Screen share stopped");
                        if let Some(w) = weak.upgrade() {
                            w.set_call_is_screen_sharing(false);
                        }
                    }
                }
            }).unwrap();
        }
    });

    let weak = window.as_weak();
    let database = ctx.database.clone();
    let message_service = ctx.message_service.clone();
    let mailbox_manager = ctx.mailbox_manager.clone();
    let settings_repo = ctx.settings_repo.clone();

    // End call
    window.on_call_end({
        let call_service = call_service.clone();
        let database = database.clone();
        let message_service = message_service.clone();
        let mailbox_manager = mailbox_manager.clone();
        let settings_repo = settings_repo.clone();

        move || {
            tracing::info!("End call");

            let weak = weak.clone();
            let call_service = call_service.clone();
            let database = database.clone();
            let message_service = message_service.clone();
            let mailbox_manager = mailbox_manager.clone();
            let settings_repo = settings_repo.clone();

            slint::spawn_local(async move {
                // IMPORTANT: Update UI immediately BEFORE async operations
                // This prevents the modal from staying visible if network calls block
                stop_call_timer();
                stop_video_frame_timer();
                if let Some(w) = weak.upgrade() {
                    w.set_active_call_visible(false);
                }
                if let Some(service) = call_service.get() {
                    // Get call info before leaving
                    let call_info = service.active_call().await;

                    // Send hangup notification for DM calls
                    if let Some(ref call) = call_info {
                        let channel_id = &call.channel_id;

                        // Get current user ID
                        let user_id_str = settings_repo
                            .get_string(chatter_linux::storage::settings_keys::CURRENT_USER_ID)
                            .ok()
                            .flatten();

                        if let Some(user_id_str) = user_id_str {
                            if let Ok(user_id) = UserId::from_str(&user_id_str) {
                                // Create channel repo for peer lookup
                                let channel_repo = ChannelRepository::new(database.clone());
                                // Check if this is a DM call
                                if let Ok(Some(peer_id)) = get_dm_peer_id(&channel_repo, channel_id, &user_id) {
                                    tracing::info!(
                                        peer_id = %peer_id,
                                        call_id = %call.id,
                                        "Sending call hangup notification"
                                    );

                                    // Build hangup content
                                    let content = chatter_linux::services::build_call_hangup_content(
                                        &call.id.to_string(),
                                        chatter_linux::services::CallHangupReason::Normal,
                                    );

                                    // Send via sealed sender
                                    if let Err(e) = message_service
                                        .send_dm(&peer_id, content, database.clone(), &mailbox_manager)
                                        .await
                                    {
                                        tracing::error!(
                                            peer_id = %peer_id,
                                            error = %e,
                                            "Failed to send hangup notification"
                                        );
                                    } else {
                                        tracing::info!(
                                            peer_id = %peer_id,
                                            call_id = %call.id,
                                            "Hangup notification sent successfully"
                                        );
                                    }
                                }

                                // Create call history message
                                let is_outgoing = call.initiator_id == user_id;
                                let duration_secs = get_call_duration_secs();

                                // Get initiator display name
                                let user_repo = UserRepository::new(database.clone());
                                let initiator_name = user_repo
                                    .get(&call.initiator_id)
                                    .ok()
                                    .flatten()
                                    .map(|u| u.username.clone())
                                    .unwrap_or_else(|| call.initiator_id.to_string());

                                create_call_history_message(
                                    database.clone(),
                                    channel_id,
                                    &call.id.to_string(),
                                    &call.initiator_id,
                                    &initiator_name,
                                    is_outgoing,
                                    duration_secs,
                                    false, // not missed - call was answered and ended normally
                                );
                            }
                        }
                    }

                    // Leave the call (async cleanup - UI already updated above)
                    match service.leave_call().await {
                        Ok(()) => {
                            tracing::info!("Left call successfully");
                        }
                        Err(e) => {
                            tracing::error!("Failed to leave call: {}", e);
                        }
                    }
                }
            }).unwrap();
        }
    });
}

/// Wire call hangup handler to process remote hangup events.
///
/// Subscribes to `DomainEvent::CallHangupReceived` from the event bus
/// and calls `leave_call()` on the CallService to disconnect from LiveKit.
fn wire_call_hangup_handler(window: &MainWindow, ctx: &AppContext) {
    let call_service = ctx.call_service.clone();
    let mut event_rx = ctx.event_bus.subscribe_domain();
    let weak = window.as_weak();
    let database = ctx.database.clone();
    let settings_repo = ctx.settings_repo.clone();

    // Spawn a task to handle incoming hangup events
    // Note: This runs in a slint::spawn_local context which keeps CallService on UI thread
    slint::spawn_local(async move {
        while let Ok(event) = event_rx.recv().await {
            if let DomainEvent::CallHangupReceived { call_id, peer_id, reason } = event {
                tracing::info!(
                    call_id = %call_id,
                    peer_id = %peer_id,
                    reason = %reason,
                    "Processing remote call hangup"
                );

                // Get call duration BEFORE stopping the timer
                let duration_secs = get_call_duration_secs();

                // Get CallService and disconnect locally
                // IMPORTANT: Don't call leave_call() here - that would make a server
                // request which can block/timeout since the peer already ended the call.
                // Just disconnect locally from LiveKit.
                if let Some(service) = call_service.get() {
                    // Get call info BEFORE disconnecting (for call history message)
                    let call_info = service.active_call().await;

                    // Verify this hangup is for our active call
                    let is_our_call = call_info.as_ref()
                        .map(|c| c.id.to_string() == call_id)
                        .unwrap_or(false);

                    if is_our_call {
                        // Create call history message before disconnecting
                        if let Some(ref call) = call_info {
                            // Get current user ID to determine if this was incoming
                            let current_user_id = settings_repo
                                .get_string(chatter_linux::storage::settings_keys::CURRENT_USER_ID)
                                .ok()
                                .flatten()
                                .and_then(|s| UserId::from_str(&s).ok());

                            if let Some(current_user_id) = current_user_id {
                                // Incoming call = we didn't initiate it
                                let is_outgoing = call.initiator_id == current_user_id;

                                // Get initiator display name
                                let user_repo = UserRepository::new(database.clone());
                                let initiator_name = user_repo
                                    .get(&call.initiator_id)
                                    .ok()
                                    .flatten()
                                    .map(|u| u.username.clone())
                                    .unwrap_or_else(|| call.initiator_id.to_string());

                                create_call_history_message(
                                    database.clone(),
                                    &call.channel_id,
                                    &call.id.to_string(),
                                    &call.initiator_id,
                                    &initiator_name,
                                    is_outgoing,
                                    duration_secs,
                                    false, // not missed - peer hung up after call was connected
                                );
                            }
                        }

                        // Disconnect locally without calling server (peer already ended)
                        service.disconnect_local().await;
                        tracing::info!("Disconnected locally after peer hangup");
                    } else {
                        tracing::debug!(
                            call_id = %call_id,
                            "Ignoring hangup for non-active call"
                        );
                    }
                }

                // Stop the call timer and video frame timer, hide the call UI
                // IMPORTANT: Must stop timers to prevent calloop event loop issues
                stop_call_timer();
                stop_video_frame_timer();

                if let Some(w) = weak.upgrade() {
                    w.set_active_call_visible(false);
                    w.set_incoming_call_visible(false);
                }
            }
        }
    }).unwrap();
}

/// Wire device picker callbacks for audio device and screen source selection.
fn wire_device_picker(window: &MainWindow, ctx: &AppContext) {
    let weak = window.as_weak();
    let call_service = ctx.call_service.clone();

    // Show audio input device picker (microphones)
    window.on_show_audio_device_picker({
        let weak = weak.clone();
        move || {
            if let Some(w) = weak.upgrade() {
                // Enumerate audio input devices
                match AudioCapture::list_devices() {
                    Ok(devices) => {
                        let ui_devices: Vec<AudioDeviceData> = devices
                            .into_iter()
                            .map(|d| AudioDeviceData {
                                id: d.id.into(),
                                name: d.name.into(),
                                is_default: d.is_default,
                            })
                            .collect();

                        w.set_audio_devices(ModelRc::new(VecModel::from(ui_devices)));
                        w.set_device_picker_type("audio".into());
                        w.set_device_picker_visible(true);
                        tracing::info!("Showing audio input device picker");
                    }
                    Err(e) => {
                        tracing::error!("Failed to enumerate audio input devices: {}", e);
                    }
                }
            }
        }
    });

    // Show audio output device picker (speakers/headphones)
    window.on_show_audio_output_picker({
        let weak = weak.clone();
        move || {
            if let Some(w) = weak.upgrade() {
                // Enumerate audio output devices
                match AudioCapture::list_output_devices() {
                    Ok(devices) => {
                        let ui_devices: Vec<AudioDeviceData> = devices
                            .into_iter()
                            .map(|d| AudioDeviceData {
                                id: d.id.into(),
                                name: d.name.into(),
                                is_default: d.is_default,
                            })
                            .collect();

                        w.set_audio_output_devices(ModelRc::new(VecModel::from(ui_devices)));
                        w.set_device_picker_type("audio_output".into());
                        w.set_device_picker_visible(true);
                        tracing::info!("Showing audio output device picker");
                    }
                    Err(e) => {
                        tracing::error!("Failed to enumerate audio output devices: {}", e);
                    }
                }
            }
        }
    });

    // Show screen source picker
    window.on_show_screen_source_picker({
        let weak = weak.clone();
        move || {
            if let Some(w) = weak.upgrade() {
                // Enumerate screen sources using LiveKit's desktop_capturer
                match ScreenCapture::list_sources() {
                    Ok(sources) => {
                        let source_count = sources.len();
                        let ui_sources: Vec<ScreenSourceData> = sources
                            .into_iter()
                            .enumerate()
                            .map(|(idx, s)| ScreenSourceData {
                                id: s.id.to_string().into(),
                                name: s.name.into(),
                                width: 0, // Not available from libwebrtc
                                height: 0,
                                is_monitor: s.source_type == ScreenSourceType::Monitor,
                                is_primary: idx == 0 && s.source_type == ScreenSourceType::Monitor,
                            })
                            .collect();

                        w.set_screen_sources(ModelRc::new(VecModel::from(ui_sources)));
                        w.set_device_picker_type("screen".into());
                        w.set_device_picker_visible(true);
                        tracing::info!("Showing screen source picker with {} sources", source_count);
                    }
                    Err(e) => {
                        tracing::error!("Failed to enumerate screen sources: {}", e);
                    }
                }
            }
        }
    });

    // Handle audio input device selection
    window.on_audio_device_selected({
        let call_service = call_service.clone();
        let weak = weak.clone();
        move |device_id| {
            let device_id_str = device_id.to_string();
            tracing::info!("Audio input device selected: {}", device_id_str);

            if let Some(service) = call_service.get() {
                let service = service.clone();
                let weak = weak.clone();
                let device_id = device_id_str.clone();

                slint::spawn_local(async move {
                    // Stop current microphone and restart with new device
                    service.stop_microphone();

                    if let Err(e) = service.publish_microphone(Some(&device_id)).await {
                        tracing::error!("Failed to switch microphone to {}: {}", device_id, e);
                    } else {
                        tracing::info!("Switched to microphone: {}", device_id);
                        // Update UI to reflect the new device
                        if let Some(w) = weak.upgrade() {
                            w.set_selected_audio_device_id(device_id.into());
                        }
                    }
                }).unwrap();
            }
        }
    });

    // Handle audio output device selection
    window.on_audio_output_selected({
        let weak = weak.clone();
        move |device_id| {
            let device_id_str = device_id.to_string();
            tracing::info!("Audio output device selected: {}", device_id_str);

            // Note: Audio output device switching requires platform-specific handling
            // LiveKit handles audio output internally via the room's audio system
            // For now, we store the selection for reference
            if let Some(w) = weak.upgrade() {
                w.set_selected_audio_output_id(device_id_str.into());
                tracing::info!("Audio output device set (system default used for playback)");
            }
        }
    });

    // Handle screen source selection
    window.on_screen_source_selected({
        let call_service = call_service.clone();
        let weak = weak.clone();
        move |source_id| {
            let source_id_str = source_id.to_string();
            tracing::info!("Screen source selected: {}", source_id_str);

            // Parse source_id as u64
            let source_id_u64: u64 = match source_id_str.parse() {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!("Invalid screen source ID '{}': {}", source_id_str, e);
                    return;
                }
            };

            if let Some(service) = call_service.get() {
                let service = service.clone();
                let weak = weak.clone();
                let source_id_display = source_id_str.clone();

                slint::spawn_local(async move {
                    // Find the selected source in the list
                    match ScreenCapture::list_sources() {
                        Ok(sources) => {
                            if let Some(source) = sources.into_iter().find(|s| s.id == source_id_u64) {
                                // Start screen share with the selected source
                                if let Err(e) = service.publish_screen_share(source).await {
                                    tracing::error!("Failed to start screen share: {}", e);
                                } else {
                                    tracing::info!("Screen share started with source: {}", source_id_display);
                                    if let Some(w) = weak.upgrade() {
                                        w.set_call_is_screen_sharing(true);
                                        w.set_selected_screen_source_id(source_id_display.into());
                                    }
                                }
                            } else {
                                tracing::error!("Selected screen source not found: {}", source_id_display);
                            }
                        }
                        Err(e) => {
                            tracing::error!("Failed to enumerate screen sources: {}", e);
                        }
                    }
                }).unwrap();
            }
        }
    });
}

//! Call orchestration (DM / group / anonymous) plus call history and lifecycle.

#![allow(unused_imports)]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::task::JoinHandle;

use crate::app::context::{AppContext, ContextError};
use crate::app::core_services::CoreServices;
use crate::domain::attachment::Attachment;
use crate::domain::channel::{Channel, ChannelType};
use crate::domain::message::{ConversationType, Message};
use crate::error::NetworkError;
use crate::events::{DomainEvent, EventBus};
use crate::services::auth_service::{AuthError, LoginResult};
use crate::inbox::DeviceInboxProvider;
use crate::mailbox::group::GroupFanout;
use crate::proto::content_v1::AttachmentPointer;
use crate::services::ai_service::AIService;
use crate::services::anon_dm::AnonDmService;
use crate::services::anon_group::AnonGroupService;
use crate::services::attachment::AttachmentService;
use crate::services::auth_service::AuthService;
#[cfg(feature = "media")]
use crate::services::call_worker::CallWorkerHandle;
use crate::services::channel::ChannelService;
use crate::services::dm_bootstrap_service::DmBootstrapService;
use crate::services::gateway::outgoing::{
    OwnedOutgoingServices, OutgoingIntent, QuoteIntent, SendError, SendResult,
    route_outgoing_owned,
};
use crate::services::ai::LocalAIService;
use crate::services::mailbox_manager::MailboxManager;
use crate::services::message::MessageService;
use crate::services::pin_service::PinService;
use crate::services::session_service::{SessionError, SessionService};
use crate::services::signal_service::SignalService;
use crate::services::section_service::SectionService;
use crate::services::user::UserService;
use crate::services::conversation::ConversationHandle;
use crate::state::{
    ActiveConversation, AuthState, ChatState, MessageState, OfflineState,
    PrivacyState, UserState,
};
use crate::state::chat_state::PendingAttachment;
use crate::storage::settings_keys;
use crate::system::Directories;
use super::{ChatterClient, ClientError};

/// Result of [`ChatterClient::accept_presented_incoming_call`]. Carries enough
/// information for a UI layer to build its active-call view without poking
/// Plain-data snapshot of a room participant. Used across the FFI
/// boundary so bridge callers don't touch LiveKit types directly.
#[cfg(feature = "media")]
#[derive(Clone, Debug)]
pub struct CallParticipantSnapshot {
    pub identity: String,
    pub display_name: String,
    pub has_audio: bool,
    pub has_video: bool,
    pub is_speaking: bool,
    pub is_muted: bool,
    pub is_local: bool,
}

/// One load of the Calls screen: the rows to render plus the unseen
/// missed-call count for the tab badge. Both come from the same blocking read so the
/// list and the badge can never disagree.
#[derive(Clone, Debug, Default)]
pub struct CallHistoryPage {
    pub rows: Vec<crate::ui::CallHistoryRowData>,
    pub missed_count: u32,
}

/// orchestrator internals. `Clone` is intentionally omitted — the anonymous
/// setup's encryption key is `Zeroize`-only.
#[cfg(feature = "media")]
#[derive(Debug)]
pub enum AcceptedIncomingCall {
    /// Standard DM call (Signal-style ephemeral key exchange).
    Dm {
        setup: crate::domain::call::CallSetupResult,
        peer_id: crate::domain::ids::UserId,
        peer_name: String,
        is_video: bool,
    },
    /// Anonymous DM / anonymous group call.
    Anon {
        setup: crate::domain::call::AnonCallSetupResult,
        peer_id: crate::domain::ids::UserId,
    },
}

impl ChatterClient {
    // ── DM Call Orchestration ──────────────────────────────────────────

    /// UI-facing summary of a freshly started outgoing call.
    ///
    /// Returned by [`ChatterClient::start_call`]. The UI only has to render
    /// this struct — no branching on conversation type, no peer-id lookups.
    // Intentionally lives next to the facade: tightly coupled and not used
    // elsewhere.
    // (Defined at module scope below; referenced here for doc linkage.)

    /// Start an outgoing call for an arbitrary conversation.
    ///
    /// Resolves the conversation kind (channel / server-DM / mailbox-DM /
    /// anon-DM / anon-group) internally, looks up the peer when required,
    /// and dispatches to the correct signalling path. Returns an
    /// [`OutgoingCallStart`] with everything the UI needs to render the
    /// "Calling…" state.
    ///
    /// Also opens the call-history row for the outgoing side. Only DM and
    /// anon calls are registered with the orchestrator while they ring
    /// (`orch_register_outgoing`), and that registration names no
    /// conversation, so this is the only place that sees an outgoing call of
    /// *every* conversation type at the moment it is placed;
    /// `call_history_recorder` closes the row on the terminal event.
    #[cfg(feature = "media")]
    pub async fn start_call(
        &self,
        conversation: crate::state::ActiveConversation,
        call_type: crate::domain::call::CallType,
        display_name: &str,
    ) -> Result<OutgoingCallStart, ClientError> {
        let start = self.dispatch_start_call(conversation, call_type, display_name).await?;
        self.open_outgoing_history_row(conversation, &start).await;
        Ok(start)
    }

    /// Record the "we placed a call" row.
    ///
    /// Both SQLite touches run on a blocking worker — this is reached from
    /// the UI's call-start task, and a synchronous repository call there
    /// parks a tokio async worker. Best-effort: a history write must never be
    /// what stops a call from being placed, so failures are logged and
    /// dropped. Awaited rather than detached so the row always exists before
    /// the recorder can try to close it.
    #[cfg(feature = "media")]
    async fn open_outgoing_history_row(
        &self,
        conversation: crate::state::ActiveConversation,
        start: &OutgoingCallStart,
    ) {
        use crate::domain::call::{CallDirection, CallHistoryEntry, CallResult};

        let Some((conversation_type, conversation_id)) = conversation.routing_key() else {
            return;
        };
        let conversations = self.services.conversation_repo.clone();
        let history = self.call_history_repo();
        let id = start.call_id.clone();
        let has_video = start.is_video;

        let written = tokio::task::spawn_blocking(move || {
            let peer_name = conversations
                .get_conversation(&conversation_id, &conversation_type)
                .ok()
                .flatten()
                .map(|c| c.name)
                .unwrap_or_default();

            history.insert_entry(&CallHistoryEntry {
                id,
                conversation_id,
                conversation_type,
                peer_name,
                started_at: chrono::Utc::now(),
                ended_at: None,
                direction: CallDirection::Outgoing,
                // Nobody has picked up yet. The recorder promotes this to
                // `Completed` on hangup only if the call actually connected.
                result: CallResult::Missed,
                duration_secs: None,
                has_video,
            })
        })
        .await;

        match written {
            Ok(Err(e)) => tracing::warn!(error = %e, "call history: outgoing insert failed"),
            Err(e) => tracing::warn!(error = %e, "call history: outgoing insert task failed"),
            Ok(Ok(())) => {}
        }
    }

    #[cfg(feature = "media")]
    async fn dispatch_start_call(
        &self,
        conversation: crate::state::ActiveConversation,
        call_type: crate::domain::call::CallType,
        display_name: &str,
    ) -> Result<OutgoingCallStart, ClientError> {
        use crate::domain::call::CallType as CT;
        use crate::state::ActiveConversation as AC;

        let user_id = self.session_current_user_id()
            .ok_or_else(|| ClientError::Validation("not logged in".into()))?;
        let is_video = matches!(call_type, CT::Video);

        match conversation {
            AC::None => Err(ClientError::Validation(
                "no conversation selected for call".into(),
            )),

            AC::AnonDm(anon_dm_id) => {
                let offer = self
                    .start_anon_dm_call(&anon_dm_id, is_video, display_name)
                    .await?;
                let title = self.conversation_display_name(
                    &crate::domain::ids::ChannelId::from_uuid(*anon_dm_id.as_uuid()),
                    &crate::domain::message::ConversationType::AnonDm,
                );
                // The anon peer's call identity is an ephemeral key, learned
                // only when they join — so the tile carries no id yet.
                let peer = RingingPeer {
                    user_id: String::new(),
                    info: crate::ui::chat_data::CallParticipantInfo {
                        initials: crate::ui::chat_data::initials_from_name(&title),
                        display_name: title.clone(),
                        avatar_bytes: None,
                    },
                };
                Ok(OutgoingCallStart {
                    call_id: offer.room_name.clone(),
                    channel_id: anon_dm_id.to_string(),
                    is_video,
                    is_anonymous: true,
                    verification_emojis: None,
                    title,
                    peer: Some(peer),
                })
            }

            AC::AnonGroup(group_id) => {
                let offer = self
                    .start_anon_group_call(&group_id, is_video, display_name)
                    .await?;
                Ok(OutgoingCallStart {
                    call_id: offer.room_name.clone(),
                    channel_id: group_id.to_string(),
                    is_video,
                    is_anonymous: true,
                    verification_emojis: None,
                    title: self.conversation_display_name(
                        &crate::domain::ids::ChannelId::from_uuid(*group_id.as_uuid()),
                        &crate::domain::message::ConversationType::AnonGroup,
                    ),
                    peer: None,
                })
            }

            AC::ServerDm { peer_id } => {
                // Channel-free DM: the call's local scoping id is derived
                // from the peer, same as the message conversation_id.
                let channel_id: crate::domain::ids::ChannelId = peer_id.into();
                let call_id = self
                    .start_dm_call(&peer_id, channel_id, call_type, user_id)
                    .await?;
                let peer = RingingPeer {
                    user_id: peer_id.to_string(),
                    info: self.resolve_call_participant(&peer_id.to_string()),
                };
                Ok(OutgoingCallStart {
                    call_id: call_id.to_string(),
                    channel_id: channel_id.to_string(),
                    is_video,
                    is_anonymous: false,
                    verification_emojis: None,
                    title: peer.info.display_name.clone(),
                    peer: Some(peer),
                })
            }

            AC::Channel(channel_id) => {
                // Channel could be a 1:1 DM or a group — resolve peer locally.
                if let Some(peer_id) = self.get_dm_peer_local(&channel_id) {
                    let call_id = self
                        .start_dm_call(&peer_id, channel_id, call_type, user_id)
                        .await?;
                    let peer = RingingPeer {
                        user_id: peer_id.to_string(),
                        info: self.resolve_call_participant(&peer_id.to_string()),
                    };
                    Ok(OutgoingCallStart {
                        call_id: call_id.to_string(),
                        channel_id: channel_id.to_string(),
                        is_video,
                        is_anonymous: false,
                        verification_emojis: None,
                        title: peer.info.display_name.clone(),
                        peer: Some(peer),
                    })
                } else {
                    let setup = self
                        .start_group_call(channel_id, call_type, user_id)
                        .await?;
                    Ok(OutgoingCallStart {
                        call_id: setup.call_id.to_string(),
                        channel_id: channel_id.to_string(),
                        is_video,
                        is_anonymous: false,
                        verification_emojis: Some(setup.verification_emojis),
                        title: String::new(), // UI substitutes channel name.
                        peer: None,
                    })
                }
            }
        }
    }

    /// Start a DM call: generate ephemeral key, create call on server, send offer.
    ///
    /// Returns the call ID. The UI should show "Calling..." and wait for
    /// `complete_dm_call()` when the peer answers.
    #[cfg(feature = "media")]
    pub async fn start_dm_call(
        &self,
        peer_id: &crate::domain::ids::UserId,
        channel_id: crate::domain::ids::ChannelId,
        call_type: crate::domain::call::CallType,
        user_id: crate::domain::ids::UserId,
    ) -> Result<crate::domain::ids::CallId, ClientError> {
        let call_worker = self.call_service().get()
            .ok_or_else(|| ClientError::Inbox("Call service not initialized".into()))?
            .clone();
        self.call_signaling()
            .start_dm_call(peer_id, channel_id, call_type, &call_worker, user_id)
            .await
            .map_err(|e| ClientError::Inbox(e.to_string()))
    }

    /// Start a group call in a channel: generate key, initiate on server, distribute key.
    ///
    /// Returns a `CallSetupResult` with call_id and verification emojis for the UI.
    #[cfg(feature = "media")]
    pub async fn start_group_call(
        &self,
        channel_id: crate::domain::ids::ChannelId,
        call_type: crate::domain::call::CallType,
        user_id: crate::domain::ids::UserId,
    ) -> Result<crate::domain::call::CallSetupResult, ClientError> {
        use crate::crypto::generate_group_call_key;
        use crate::services::call_verification::CallVerification;

        let call_worker = self.call_service().get()
            .ok_or_else(|| ClientError::Inbox("Call service not initialized".into()))?
            .clone();

        let group_key = generate_group_call_key();

        let call_type_str = match call_type {
            crate::domain::call::CallType::Voice => "Voice Call",
            crate::domain::call::CallType::Video => "Video Call",
            crate::domain::call::CallType::Group => "Group Call",
        };

        let call = call_worker
            .initiate_call(channel_id, call_type_str, call_type, &group_key, user_id)
            .await
            .map_err(|e| ClientError::Inbox(e.to_string()))?;

        // Register the active call so the orchestrator can resolve its kind on
        // hangup. A group/channel call has no single peer, so we use a nil
        // UserId (the orchestrator only consults peer_id for the DM
        // replace-active policy, which group calls don't use). Without this,
        // `end_active_call` finds no active slot and never leaves the room.
        use crate::services::call_worker::{CallHandle, OrchestratedCallKind};
        call_worker.orch_mark_connected(
            CallHandle::from(call.id.to_string().as_str()),
            OrchestratedCallKind::GroupChannel,
            crate::domain::ids::UserId::from_uuid(uuid::Uuid::nil()),
            matches!(call_type, crate::domain::call::CallType::Video),
        );

        // Publish microphone immediately
        if let Err(e) = call_worker.publish_microphone(None).await {
            tracing::error!("Failed to publish microphone: {}", e);
        }

        // Distribute key to all channel members (fire-and-forget)
        let call_id_str = call.id.to_string();
        let signaling = self.call_signaling().clone();
        let ch_id = channel_id;
        tokio::spawn(async move {
            match signaling.distribute_group_call_key(&ch_id, &call_id_str, group_key, 0).await {
                Ok(count) => tracing::info!(call_id = %call_id_str, sent = count, "Group call key distributed"),
                Err(e) => tracing::error!(call_id = %call_id_str, error = %e, "Failed to distribute group call key"),
            }
        });

        let verification = CallVerification::derive_verification_emojis(&group_key, &call.id.to_string());
        let emojis = CallVerification::to_slint_tuple(&verification);

        Ok(crate::domain::call::CallSetupResult {
            call_id: call.id,
            channel_id,
            verification_emojis: [emojis.0, emojis.1, emojis.2, emojis.3],
            is_video: matches!(call_type, crate::domain::call::CallType::Video),
            started_at_ns: call.started_at
                .and_then(|d| d.timestamp_nanos_opt())
                .filter(|n| *n > 0)
                .map(|n| n as u64)
                .unwrap_or(0),
        })
    }

    /// Complete an outgoing DM call after receiving the peer's ephemeral key answer.
    ///
    /// Returns verification data (emojis) for the UI.
    #[cfg(feature = "media")]
    pub async fn complete_dm_call(
        &self,
        peer_ephemeral_pk: [u8; 32],
        call_id: &str,
        peer_id: &crate::domain::ids::UserId,
    ) -> Result<crate::domain::call::CallSetupResult, ClientError> {
        let call_worker = self.call_service().get()
            .ok_or_else(|| ClientError::Inbox("Call service not initialized".into()))?
            .clone();
        self.call_signaling()
            .complete_dm_call(
                peer_ephemeral_pk,
                call_id,
                peer_id,
                self.signal(),
                &call_worker,
            )
            .await
            .map_err(|e| ClientError::Inbox(e.to_string()))
    }

    /// Accept an incoming DM call: generate ephemeral key, derive shared secret, join call.
    ///
    /// Returns verification data (emojis) for the UI.
    #[cfg(feature = "media")]
    pub async fn accept_dm_call(
        &self,
        caller_ephemeral_pk: [u8; 32],
        call_id: &str,
        caller_id: &crate::domain::ids::UserId,
        user_id: crate::domain::ids::UserId,
    ) -> Result<crate::domain::call::CallSetupResult, ClientError> {
        let call_worker = self.call_service().get()
            .ok_or_else(|| ClientError::Inbox("Call service not initialized".into()))?
            .clone();
        self.call_signaling()
            .accept_dm_call(
                caller_ephemeral_pk,
                call_id,
                caller_id,
                self.signal(),
                &call_worker,
                user_id,
            )
            .await
            .map_err(|e| ClientError::Inbox(e.to_string()))
    }

    // ── High-level orchestrated accept / decline ─────────────────────────
    //
    // UI callbacks should call these instead of assembling the orchestrator
    // transitions + wire-level accept + mark-connected by hand. All call
    // state lives in the worker actor; the UI is a thin renderer.

    /// Accept a presented incoming call (the one the orchestrator fired
    /// `IncomingPresented` for). Routes the orchestrator transition, fires
    /// the wire-level hangup on the old active call when `replaces_active`,
    /// runs the appropriate accept flow (DM / anon / anon group), and
    /// promotes the orchestrator's active slot on success.
    #[cfg(feature = "media")]
    pub async fn accept_presented_incoming_call(
        &self,
        call_id: &str,
        replaces_active: bool,
    ) -> Result<AcceptedIncomingCall, ClientError> {
        use crate::services::call_worker::{CallEndReason, CallHandle, OrchestratedCallKind};

        let worker = self.call_service().get()
            .ok_or_else(|| ClientError::Inbox("Call service not initialized".into()))?
            .clone();

        let handle = CallHandle::from(call_id);
        let snapshot = if replaces_active {
            worker
                .orch_accept_incoming_replacing_active(handle.clone())
                .await
        } else {
            worker.orch_accept_incoming(handle.clone()).await
        }
        .map_err(|e| ClientError::Inbox(e.to_string()))?;
        // The call has left the ringing set, so its ring notification goes —
        // whether the answer came from the in-app modal or the notification's
        // own Accept, and whether or not the connect below succeeds.
        let _ = self.services.notification_service.cancel_incoming_call(call_id);

        if replaces_active {
            // Fire wire-level hangup on the current active call so the old
            // peer sees the call end. Best-effort — the new call proceeds
            // regardless.
            if let Err(e) = worker.end_call(Some("replaced")).await {
                tracing::debug!(error = %e, "End-call (replaced) failed");
            }
        }

        let kind = snapshot.kind;
        let peer_id = snapshot.peer_id;
        let is_video = snapshot.is_video;

        let accepted = async { Ok::<_, ClientError>(match kind {
            OrchestratedCallKind::DmStandard => {
                let ephemeral = snapshot.ephemeral_public_key.ok_or_else(|| {
                    ClientError::Inbox(
                        "DM accept missing ephemeral key in orchestrator snapshot".into(),
                    )
                })?;
                let user_id = self.session_current_user_id().ok_or_else(|| {
                    ClientError::Inbox("Not logged in".into())
                })?;
                let setup = self
                    .accept_dm_call(ephemeral, call_id, &peer_id, user_id)
                    .await?;
                worker.orch_mark_connected(
                    CallHandle::from(setup.call_id.to_string()),
                    kind,
                    peer_id,
                    is_video,
                );
                AcceptedIncomingCall::Dm {
                    setup,
                    peer_id,
                    peer_name: snapshot.peer_name,
                    is_video,
                }
            }
            OrchestratedCallKind::AnonDm | OrchestratedCallKind::AnonGroup => {
                let setup = self.accept_anon_call(call_id).await?;
                worker.orch_mark_connected(
                    CallHandle::from(setup.room_name.clone()),
                    kind,
                    peer_id,
                    setup.is_video,
                );
                AcceptedIncomingCall::Anon { setup, peer_id }
            }
            OrchestratedCallKind::GroupChannel => {
                return Err(ClientError::Inbox(
                    "Group channel incoming accept not supported".into(),
                ));
            }
        }) }
        .await;

        // The call left the ringing set above, so nothing else will ever end
        // it: the history row stayed "missed" and the caller — told we
        // answered, since the answer goes out before the join — sat in a room
        // nobody joins. End it here: `ActiveEnded(Error)` files the row as
        // failed and takes down the call view, and the hangup ends the caller's.
        if let Err(e) = &accepted {
            worker.orch_mark_ended(handle, CallEndReason::Error(e.to_string()));
            if kind == OrchestratedCallKind::DmStandard {
                if let Err(e) = self
                    .call_signaling()
                    .send_call_hangup(
                        &crate::domain::ids::ChannelId::from(peer_id),
                        call_id,
                        crate::services::gateway::CallHangupReason::NetworkError,
                    )
                    .await
                {
                    tracing::warn!(error = %e, call_id, "Failed-accept hangup not delivered");
                }
            }
        }
        accepted
    }

    // ── Media / encryption pass-throughs ─────────────────────────────
    //
    // UI callbacks use these instead of pulling a CallWorkerHandle out of
    // the OnceLock themselves. Keeps the "UI never touches the worker
    // handle directly" invariant the orchestrator refactor depends on.

    #[cfg(feature = "media")]
    pub async fn call_publish_microphone(
        &self,
        device_id: Option<&str>,
    ) -> Result<(), crate::error::CallError> {
        match self.call_service().get().cloned() {
            Some(worker) => worker.publish_microphone(device_id).await,
            None => Err(crate::error::CallError::WorkerShutdown),
        }
    }

    #[cfg(feature = "media")]
    pub async fn call_publish_camera(
        &self,
        device_path: Option<&str>,
    ) -> Result<(), crate::error::CallError> {
        match self.call_service().get().cloned() {
            Some(worker) => worker.publish_camera(device_path).await,
            None => Err(crate::error::CallError::WorkerShutdown),
        }
    }

    #[cfg(feature = "media")]
    pub fn call_stop_microphone(&self) {
        if let Some(worker) = self.call_service().get() {
            worker.stop_microphone();
        }
    }

    #[cfg(feature = "media")]
    pub fn call_stop_camera(&self) {
        if let Some(worker) = self.call_service().get() {
            worker.stop_camera();
        }
    }

    #[cfg(feature = "media")]
    pub fn call_set_microphone_muted(&self, muted: bool) {
        if let Some(worker) = self.call_service().get() {
            worker.set_microphone_muted(muted);
        }
    }

    #[cfg(feature = "media")]
    pub async fn call_is_microphone_muted(&self) -> bool {
        match self.call_service().get().cloned() {
            Some(worker) => worker.is_microphone_muted().await,
            None => false,
        }
    }

    #[cfg(feature = "media")]
    pub async fn call_is_camera_enabled(&self) -> bool {
        match self.call_service().get().cloned() {
            Some(worker) => worker.is_camera_enabled().await,
            None => false,
        }
    }

    #[cfg(feature = "media")]
    pub async fn call_is_screen_sharing(&self) -> bool {
        match self.call_service().get().cloned() {
            Some(worker) => worker.is_screen_sharing().await,
            None => false,
        }
    }

    /// True only when the LiveKit room is fully connected.
    ///
    /// Distinct from "there is an active call" — an outgoing call sits in the
    /// "Calling…" state (no room) until the peer answers. Media actions like
    /// screen share need the room, so the UI gates on this before opening the
    /// source picker; otherwise `publish_screen_share` fails `NotInCall` after
    /// the user has already interacted (and the OS portal dialog never opens).
    #[cfg(feature = "media")]
    pub async fn call_is_connected(&self) -> bool {
        match self.call_service().get().cloned() {
            Some(worker) => matches!(
                worker.room_state().await,
                crate::calls::RoomState::Connected { .. }
            ),
            None => false,
        }
    }

    /// Return the currently-active call, if any. Used by the remote-hangup
    /// handler to match incoming hangup events against the live call.
    #[cfg(feature = "media")]
    pub async fn call_active_call(&self) -> Option<crate::domain::call::Call> {
        match self.call_service().get().cloned() {
            Some(worker) => worker.active_call().await,
            None => None,
        }
    }

    /// Locally mute/unmute a remote participant's audio. The LiveKit
    /// stream stays subscribed; muted frames are dropped before mixing.
    #[cfg(feature = "media")]
    pub fn call_set_peer_audio_muted(&self, participant_id: String, muted: bool) {
        if let Some(worker) = self.call_service().get() {
            worker.set_peer_audio_muted(participant_id, muted);
        }
    }

    /// Route call audio to the output device `device` (an id from
    /// `AudioCapture::list_output_devices`). Applies mid-call, and to the
    /// next call if none is running.
    #[cfg(feature = "media")]
    pub async fn call_set_audio_output(&self, device: &str) -> Result<(), String> {
        match self.call_service().get().cloned() {
            Some(worker) => worker.set_audio_output(device.to_string()).await,
            None => Err("calls are not available".to_string()),
        }
    }

    /// Locally pause/resume a remote participant's video.
    #[cfg(feature = "media")]
    pub fn call_set_peer_video_paused(&self, participant_id: String, paused: bool) {
        if let Some(worker) = self.call_service().get() {
            worker.set_peer_video_paused(participant_id, paused);
        }
    }

    #[cfg(all(feature = "media", not(any(target_os = "android", target_os = "ios"))))]
    pub async fn call_publish_screen_share(
        &self,
        source: crate::calls::ScreenSource,
    ) -> Result<(), crate::error::CallError> {
        match self.call_service().get().cloned() {
            Some(worker) => worker.publish_screen_share(source).await,
            None => Err(crate::error::CallError::WorkerShutdown),
        }
    }

    #[cfg(all(feature = "media", not(any(target_os = "android", target_os = "ios"))))]
    pub fn call_stop_screen_share(&self) {
        if let Some(worker) = self.call_service().get() {
            worker.stop_screen_share();
        }
    }

    #[cfg(feature = "media")]
    pub async fn call_set_encryption_key(&self, key: Vec<u8>) -> bool {
        match self.call_service().get().cloned() {
            Some(worker) => worker.set_encryption_key(key).await,
            None => false,
        }
    }

    /// Subscribe to the orchestrator's broadcast stream. Returns `None`
    /// until the call service has been initialized (post-login).
    #[cfg(feature = "media")]
    pub fn subscribe_call_orchestrator_events(
        &self,
    ) -> Option<
        tokio::sync::broadcast::Receiver<
            crate::services::call_worker::CallOrchestratorEvent,
        >,
    > {
        self.call_service().get().map(|w| w.subscribe_call_events())
    }

    /// Clone the worker's shared video frames store. Cheap — the inner
    /// type is an `Arc` handle. Returns `None` until the call service
    /// has been initialized (post-login). The UI uses this to drive its
    /// video frame polling timer without poking the raw worker handle.
    #[cfg(feature = "media")]
    pub fn call_video_frames(&self) -> Option<crate::calls::SharedVideoFrames> {
        self.call_service().get().map(|w| w.video_frames().clone())
    }

    /// Take the LiveKit room events receiver from the worker. Can only
    /// be called once — the receiver is moved out of the worker. Returns
    /// `None` if the call service isn't initialized yet, or if the
    /// receiver has already been taken.
    #[cfg(feature = "media")]
    pub fn take_call_room_events(
        &self,
    ) -> Option<tokio::sync::mpsc::Receiver<crate::calls::LiveKitRoomEvent>> {
        self.call_service().get().and_then(|w| w.take_room_events())
    }

    /// Leave the currently-joined LiveKit room without clearing the
    /// orchestrator's active-call record. Bridge pairs this with
    /// `end_dm_call` / `end_anon_call` for the full hang-up flow.
    #[cfg(feature = "media")]
    pub async fn call_leave_room(&self) -> Result<(), crate::error::CallError> {
        if let Some(worker) = self.call_service().get().cloned() {
            worker.leave_call(crate::services::call_worker::CallEndReason::LocalHangup).await?;
        }
        Ok(())
    }

    /// Snapshot of the active call's participant list — identity, display
    /// name, audio/video flags, speaking + muted state, `is_local`.
    /// Returned in a Send-safe shape so the bridge can hand it over uniffi.
    #[cfg(feature = "media")]
    pub async fn call_active_participants(&self) -> Vec<CallParticipantSnapshot> {
        let Some(worker) = self.call_service().get().cloned() else { return vec![]; };
        worker.participant_snapshot().await
    }

    /// Cloned handle to the currently-initialised call worker, or None
    /// when no call service has been wired up yet. Useful for bridge code
    /// that wants to call worker methods directly (mic publish, screen
    /// share, etc.) without re-fetching via `call_service()` each time.
    #[cfg(feature = "media")]
    pub fn call_service_handle_opt(
        &self,
    ) -> Option<crate::services::call_worker::CallWorkerHandle> {
        self.call_service().get().cloned()
    }

    /// Decline a presented incoming call. Runs the orchestrator transition
    /// (which emits `IncomingWithdrawn { Declined }`) and then fires the
    /// wire-level decline on the appropriate transport.
    #[cfg(feature = "media")]
    pub async fn decline_presented_incoming_call(
        &self,
        call_id: &str,
    ) -> Result<(), ClientError> {
        use crate::services::call_worker::{CallHandle, OrchestratedCallKind};

        let worker = self.call_service().get()
            .ok_or_else(|| ClientError::Inbox("Call service not initialized".into()))?
            .clone();

        let snapshot = worker
            .orch_decline_incoming(CallHandle::from(call_id))
            .await
            .map_err(|e| ClientError::Inbox(e.to_string()))?;

        match snapshot.kind {
            OrchestratedCallKind::DmStandard | OrchestratedCallKind::GroupChannel => {
                if snapshot.kind == OrchestratedCallKind::DmStandard {
                    // The caller hears the decline end-to-end; our other
                    // devices get its DECLINED_ELSEWHERE copy and stop ringing.
                    if let Err(e) = self.call_signaling()
                        .send_call_decline(&snapshot.peer_id, call_id)
                        .await
                    {
                        tracing::warn!(error = %e, call_id, "Call decline not delivered");
                    }
                }
                self.decline_call(call_id).await.map_err(|e| {
                    ClientError::Inbox(format!("decline_call: {e}"))
                })?;
            }
            OrchestratedCallKind::AnonDm | OrchestratedCallKind::AnonGroup => {
                self.decline_anon_call(call_id).await.map_err(|e| {
                    ClientError::Inbox(format!("decline_anon_call: {e}"))
                })?;
            }
        }
        Ok(())
    }

    // ── Anonymous Call Orchestration ─────────────────────────────────

    /// Start an anonymous DM call: acquire token, send offer, store pending state.
    #[cfg(feature = "media")]
    pub async fn start_anon_dm_call(
        &self,
        anon_dm_id: &crate::domain::anon_dm::AnonDmId,
        is_video: bool,
        display_name: &str,
    ) -> Result<crate::services::anon_dm::AnonCallOfferResult, ClientError> {
        let call_worker = self.call_service().get()
            .ok_or_else(|| ClientError::Inbox("Call service not initialized".into()))?;
        self.call_signaling()
            .start_anon_dm_call(anon_dm_id, is_video, display_name, call_worker)
            .await
            .map_err(|e| ClientError::Inbox(e.to_string()))
    }

    /// Start an anonymous group call: acquire token, send offer to all members, store pending state.
    #[cfg(feature = "media")]
    pub async fn start_anon_group_call(
        &self,
        group_id: &crate::domain::anon_group::AnonGroupId,
        is_video: bool,
        display_name: &str,
    ) -> Result<crate::services::anon_dm::AnonCallOfferResult, ClientError> {
        let call_worker = self.call_service().get()
            .ok_or_else(|| ClientError::Inbox("Call service not initialized".into()))?;
        self.call_signaling()
            .start_anon_group_call(group_id, is_video, display_name, call_worker)
            .await
            .map_err(|e| ClientError::Inbox(e.to_string()))
    }

    /// Accept an incoming anonymous call: acquire token, send accept, join room, connect LiveKit.
    #[cfg(feature = "media")]
    pub async fn accept_anon_call(
        &self,
        room_name: &str,
    ) -> Result<crate::domain::call::AnonCallSetupResult, ClientError> {
        let call_worker = self.call_service().get()
            .ok_or_else(|| ClientError::Inbox("Call service not initialized".into()))?
            .clone();
        self.call_signaling()
            .accept_anon_call(room_name, &call_worker)
            .await
            .map_err(|e| ClientError::Inbox(e.to_string()))
    }

    // handle_anon_call_answer is now owned by the call orchestrator bridge
    // (chatter_core::services::call_worker::spawn_anon_call_answer_task).
    // No client facade — UI reacts to orchestrator events instead.

    /// Decline an incoming anonymous call: send decline answer, store missed call history.
    pub async fn decline_anon_call(
        &self,
        room_name: &str,
    ) -> Result<(), ClientError> {
        self.call_signaling()
            .decline_anon_call(room_name)
            .await
            .map_err(|e| ClientError::Inbox(e.to_string()))
    }

    // ── Call history ──────────────────────────────────────────────────

    // ── Local call history (v053) ──────────────────────────────────
    //
    // These methods talk to the `call_history` SQLite table via
    // `CallHistoryRepository`. Purely client-local state: the server exposes
    // no call-history RPC yet. Rows are opened by `start_call` (outgoing)
    // and `call_history_recorder` (incoming), and read by the Calls screen.

    /// Build the Calls screen's rows, newest first, plus the missed-call
    /// count the tab badge shows.
    ///
    /// Async because `CallHistoryRepository` is a synchronous SQLite handle:
    /// the read (and all the row shaping) runs on a blocking worker, never on
    /// the caller's thread. The Calls screen is painted from the Slint event
    /// loop, and a SQLite read there stalls every frame in the app.
    ///
    /// Returns display-ready data — the UI renders it without re-deriving
    /// wording, durations or routing.
    ///
    /// Only the Calls screen loads a page, so loading one is the user seeing
    /// their missed calls: it marks them seen and the badge clears.
    pub async fn call_history_page(
        &self,
        filter: crate::domain::call::CallHistoryFilter,
        limit: u32,
    ) -> Result<CallHistoryPage, ClientError> {
        let repo = self.call_history_repo();
        let mut page = tokio::task::spawn_blocking(move || {
            let entries = repo
                .list(filter, limit, None)
                .map_err(|e| ClientError::Inbox(format!("call history list: {}", e)))?;
            if let Err(e) = repo.mark_missed_seen() {
                tracing::warn!(error = %e, "call history: mark missed seen failed");
            }
            Ok::<_, ClientError>(CallHistoryPage {
                rows: entries.iter().map(crate::ui::CallHistoryRowData::from).collect(),
                missed_count: repo.missed_count().unwrap_or(0) as u32,
            })
        })
        .await
        .map_err(|e| ClientError::Inbox(format!("call history join: {}", e)))??;
        for row in &mut page.rows {
            row.avatar_bytes = self.call_peer_avatar(&row.conversation_type, &row.conversation_id);
        }
        Ok(page)
    }

    /// Avatar for a call-history row's peer, from the stores the chat list
    /// reads: the peer's profile for a server DM, the anon DM row for an
    /// anonymous one. Group and channel calls have no single peer → `None`.
    fn call_peer_avatar(&self, conversation_type: &str, conversation_id: &str) -> Option<Vec<u8>> {
        use crate::domain::message::ConversationType as CT;
        match CT::from_str_value(conversation_type).ok()? {
            CT::ServerDm => self.user_avatar(&conversation_id.parse().ok()?),
            CT::AnonDm => self.get_anon_dm_conversation(&conversation_id.parse().ok()?)?.peer_avatar,
            _ => None,
        }
    }

    /// Unseen missed incoming calls on their own, for painting the tab badge
    /// at startup without loading the list (which would mark them seen).
    pub async fn missed_call_count(&self) -> u32 {
        let repo = self.call_history_repo();
        tokio::task::spawn_blocking(move || repo.missed_count().unwrap_or(0) as u32)
            .await
            .unwrap_or(0)
    }

    /// Resolve a Calls-screen row back to the conversation it happened in,
    /// so the UI can reopen it and redial. `None` when the row's key no
    /// longer names a reachable conversation.
    pub fn call_history_conversation(
        &self,
        conversation_type: &str,
        conversation_id: &str,
    ) -> Option<crate::state::ActiveConversation> {
        let ct = crate::domain::message::ConversationType::from_str_value(conversation_type).ok()?;
        crate::state::ActiveConversation::from_routing_key(&ct, conversation_id)
    }

    /// Clear all local call history.
    pub async fn clear_call_history(&self) -> Result<u64, ClientError> {
        let repo = self.call_history_repo();
        tokio::task::spawn_blocking(move || {
            repo.delete_all()
                .map_err(|e| ClientError::Inbox(format!("call history clear: {}", e)))
        })
        .await
        .map_err(|e| ClientError::Inbox(format!("call history join: {}", e)))?
    }

    /// Accessor used by the history helpers above and by the signaling
    /// service when it wants to record events from inside core.
    fn call_history_repo(
        &self,
    ) -> crate::storage::repositories::CallHistoryRepository {
        crate::storage::repositories::CallHistoryRepository::new(
            self.services.database.clone(),
        )
    }

    // ── Call lifecycle ─────────────────────────────────────────────

    /// End an active DM call: send hangup, leave. The Call bubble follows
    /// from the call ending (`call_history_recorder`).
    #[cfg(feature = "media")]
    pub async fn end_dm_call(&self) -> Result<(), ClientError> {
        let call_worker = self.call_service().get()
            .ok_or_else(|| ClientError::Inbox("call service not initialized".into()))?;
        self.call_signaling()
            .end_dm_call(call_worker, crate::services::call_worker::CallEndReason::LocalHangup)
            .await
            .map_err(|e| ClientError::Inbox(e.to_string()))
    }

    /// End an active group/channel call: store history and leave the room.
    ///
    /// Unlike a DM call there is no single peer to notify — leaving the
    /// LiveKit room broadcasts `ParticipantDisconnected` to the other
    /// members, and the initiator rotates the key for those remaining. No
    /// DM hangup is sent (the DM hangup path resolves a single peer, which is
    /// meaningless for a multi-member channel).
    #[cfg(feature = "media")]
    pub async fn end_group_call(&self, duration_secs: i64) -> Result<(), ClientError> {
        let call_worker = self.call_service().get()
            .ok_or_else(|| ClientError::Inbox("call service not initialized".into()))?;
        self.call_signaling()
            .end_group_call(call_worker, duration_secs)
            .await
            .map_err(|e| ClientError::Inbox(e.to_string()))
    }

    /// End an active anonymous call: send hangup, store history, leave.
    ///
    /// Returns the conversation_id for UI bubble insertion.
    #[cfg(feature = "media")]
    pub async fn end_anon_call(&self, duration_secs: i64) -> Result<Option<crate::domain::anon_dm::AnonDmId>, ClientError> {
        let call_worker = self.call_service().get()
            .ok_or_else(|| ClientError::Inbox("call service not initialized".into()))?;
        self.call_signaling()
            .end_anon_call(call_worker, duration_secs)
            .await
            .map_err(|e| ClientError::Inbox(e.to_string()))
    }

    /// End the active call regardless of kind.
    ///
    /// Resolves the call kind from the orchestrator — the connected call, or
    /// our DM or anon call still ringing, so cancelling it sends the hangup — and
    /// dispatches to the correct wire-level tear-down. UI code should call
    /// this from its "End call" button instead of branching between
    /// `end_dm_call` / `end_anon_call` based on view state.
    #[cfg(feature = "media")]
    pub async fn end_active_call(&self, duration_secs: i64) -> Result<(), ClientError> {
        use crate::services::call_worker::OrchestratedCallKind;

        let worker = self.call_service().get()
            .ok_or_else(|| ClientError::Inbox("call service not initialized".into()))?;
        let kind = match worker.orch_active_kind().await {
            Some(k) => k,
            None => {
                tracing::debug!("end_active_call: no active call, nothing to do");
                return Ok(());
            }
        };
        match kind {
            OrchestratedCallKind::AnonDm | OrchestratedCallKind::AnonGroup => {
                self.end_anon_call(duration_secs).await.map(|_| ())
            }
            OrchestratedCallKind::DmStandard => {
                self.end_dm_call().await
            }
            OrchestratedCallKind::GroupChannel => {
                self.end_group_call(duration_secs).await
            }
        }
    }

    /// Handle a participant departing from a group call.
    ///
    /// If we are the initiator of a channel-based group call, generates a new
    /// random encryption key, rotates it locally, and redistributes it to the
    /// peers still in the room so `departed_identity` can't decrypt future
    /// frames.
    ///
    /// Only the initiator rotates to avoid race conditions.
    #[cfg(feature = "media")]
    pub async fn handle_participant_departed(&self, departed_identity: &str) -> Result<(), ClientError> {
        let call_worker = self.call_service().get()
            .ok_or_else(|| ClientError::Inbox("call service not initialized".into()))?;

        let call = match call_worker.active_call().await {
            Some(c) => c,
            None => return Ok(()), // No active call
        };

        // Only rotate for channel-based group calls (not anon, not DM).
        // Anonymous calls have a random channel_id that won't resolve.
        let channel = match self.channels().get_channel_local(&call.channel_id) {
            Ok(Some(c)) => c,
            _ => return Ok(()),
        };

        use crate::domain::channel::ChannelType;
        if matches!(channel.channel_type, ChannelType::DirectMessage | ChannelType::SavedMessages) {
            return Ok(());
        }

        // Only the initiator rotates to prevent race conditions
        let my_id = self.sessions().current_user_id()
            .ok_or_else(|| ClientError::Inbox("not logged in".into()))?;
        if my_id != call.initiator_id {
            return Ok(());
        }

        // Sample the roster BEFORE rotating. Rotation is irreversible — it bumps
        // the epoch on the local worker — so anything fallible that the
        // distribution depends on has to happen while failing is still free. The
        // reverse order leaves the local key changed and nobody else holding it,
        // which breaks the call for every remaining participant rather than for
        // the one who left.
        //
        // The departed identity is subtracted explicitly rather than trusting the
        // SDK to have evicted it before firing ParticipantLeft — not an ordering
        // assumption this path should depend on.
        let roster: Vec<String> = call_worker
            .remote_participant_identities()
            .await
            .into_iter()
            .filter(|id| id != departed_identity)
            .collect();

        // Generate new random key and rotate locally
        let new_key = crate::crypto::generate_group_call_key();
        let epoch = call_worker.rotate_key(new_key).await
            .map_err(|e| ClientError::Inbox(format!("key rotation failed: {e}")))?;

        // Brief delay for LiveKit key propagation before peers receive new-key frames
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

        // Redistribute to the peers actually still in the room, not to the
        // channel membership — a departed peer is still a member, so the old
        // membership-wide send handed the rotated key straight back to the
        // participant this rotation exists to lock out.
        if roster.is_empty() {
            // Still rotated: an empty room is not a reason to leave the departed
            // peer holding a working key.
            tracing::info!(epoch, "Rotated group call key; no remaining participants to redistribute to");
            return Ok(());
        }

        let call_id = call.id.to_string();
        match self.call_signaling()
            .redistribute_group_call_key(&call.channel_id, &call_id, new_key, epoch, &roster)
            .await
        {
            Ok(count) => {
                tracing::info!(epoch, sent = count, "Rotated group call key after participant departure");
            }
            Err(e) => {
                tracing::error!(error = %e, "Failed to redistribute rotated group call key");
            }
        }

        Ok(())
    }

}

/// Summary returned by [`ChatterClient::start_call`] for the UI's
/// "Calling…" placeholder. Holds only view data — no service handles, no
/// crypto material. UI code should treat this as an opaque payload to
/// render, not as orchestration state.
#[derive(Debug, Clone)]
pub struct OutgoingCallStart {
    /// Stable identifier for the outgoing call. For DM/group calls this is
    /// the server-assigned `CallId` stringified; for anon calls it is the
    /// LiveKit room name.
    pub call_id: String,
    /// Conversation / channel id to display in the call metadata. Stringified
    /// to avoid leaking domain ID types into the UI crate.
    pub channel_id: String,
    pub is_video: bool,
    /// True if this is an anonymous call (DM or group). Drives the "shield"
    /// branding and skips identity verification surfaces.
    pub is_anonymous: bool,
    /// Verification emojis, if already known at dial time (channel group
    /// calls). `None` for DM + anon calls — those populate after the peer's
    /// answer arrives.
    pub verification_emojis: Option<[String; 4]>,
    /// Who is being called: the peer's display name for a DM, the
    /// conversation name for an anon group. Empty for a channel group call,
    /// where the UI uses the channel name it already shows.
    pub title: String,
    /// The person being rung in a 1:1 call, so the ringing screen shows them
    /// rather than the caller. `None` for group calls.
    pub peer: Option<RingingPeer>,
}

/// The callee of a 1:1 outgoing call, resolved for display at dial time.
#[derive(Debug, Clone)]
pub struct RingingPeer {
    /// The peer's call identity; empty for an anon DM, whose identity is an
    /// ephemeral key only learned when they join.
    pub user_id: String,
    pub info: crate::ui::chat_data::CallParticipantInfo,
}

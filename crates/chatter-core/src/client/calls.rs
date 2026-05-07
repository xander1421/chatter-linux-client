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
use crate::services::jarvis::JarvisService;
use crate::services::ai::LocalAIService;
use crate::services::mailbox_manager::MailboxManager;
use crate::services::message::MessageService;
use crate::services::pin_service::PinService;
use crate::services::session_service::{SessionError, SessionService};
use crate::services::signal_service::SignalService;
use crate::services::section_service::SectionService;
use crate::services::typing_service::TypingService;
use crate::services::user::UserService;
use crate::services::conversation::ConversationHandle;
use crate::state::{
    ActiveConversation, AuthState, ChatState, JarvisState, MessageState, OfflineState,
    OrgState, PinnedState, PrivacyState, StreamingState, UIState, UserState,
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
    #[cfg(feature = "media")]
    pub async fn start_call(
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
                Ok(OutgoingCallStart {
                    call_id: offer.room_name.clone(),
                    channel_id: anon_dm_id.to_string(),
                    is_video,
                    is_anonymous: true,
                    verification_emojis: None,
                    title: "Calling...".into(),
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
                    title: "Calling...".into(),
                })
            }

            AC::ServerDm { channel_id, peer_id }
            | AC::MailboxDm { channel_id, peer_id } => {
                let call_id = self
                    .start_dm_call(&peer_id, channel_id, call_type, user_id)
                    .await?;
                Ok(OutgoingCallStart {
                    call_id: call_id.to_string(),
                    channel_id: channel_id.to_string(),
                    is_video,
                    is_anonymous: false,
                    verification_emojis: None,
                    title: "Calling...".into(),
                })
            }

            AC::Channel(channel_id) => {
                // Channel could be a 1:1 DM or a group — resolve peer locally.
                if let Some(peer_id) = self.get_dm_peer_local(&channel_id) {
                    let call_id = self
                        .start_dm_call(&peer_id, channel_id, call_type, user_id)
                        .await?;
                    Ok(OutgoingCallStart {
                        call_id: call_id.to_string(),
                        channel_id: channel_id.to_string(),
                        is_video,
                        is_anonymous: false,
                        verification_emojis: None,
                        title: "Calling...".into(),
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
        use crate::services::call_worker::{CallHandle, OrchestratedCallKind};

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

        let accepted = match kind {
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
        };

        Ok(accepted)
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

    /// Return the currently-active call, if any. Used by the remote-hangup
    /// handler to match incoming hangup events against the live call.
    #[cfg(feature = "media")]
    pub async fn call_active_call(&self) -> Option<crate::domain::call::Call> {
        match self.call_service().get().cloned() {
            Some(worker) => worker.active_call().await,
            None => None,
        }
    }

    #[cfg(feature = "media")]
    pub fn call_set_camera_enabled(&self, enabled: bool) {
        if let Some(worker) = self.call_service().get() {
            worker.set_camera_enabled(enabled);
        }
    }

    #[cfg(all(feature = "media", not(target_os = "android"), not(target_os = "ios")))]
    pub async fn call_publish_screen_share(
        &self,
        source: crate::calls::ScreenSource,
    ) -> Result<(), crate::error::CallError> {
        match self.call_service().get().cloned() {
            Some(worker) => worker.publish_screen_share(source).await,
            None => Err(crate::error::CallError::WorkerShutdown),
        }
    }

    #[cfg(all(feature = "media", not(target_os = "android"), not(target_os = "ios")))]
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

    /// Join a LiveKit room using pre-acquired setup credentials (url,
    /// token, SFrame key) — the single media-layer entry the mobile
    /// bridge calls after a signalling handshake produces a
    /// `BridgeCallSetup`. Wraps `call_worker::connect_with_token` and
    /// takes care of publishing microphone + optional camera via the
    /// worker's subsequent commands.
    #[cfg(feature = "media")]
    pub async fn call_connect_with_setup(
        &self,
        call_id: &str,
        channel_id: crate::domain::ids::ChannelId,
        livekit_url: &str,
        access_token: &str,
        encryption_key: Vec<u8>,
        room_name: &str,
        is_video: bool,
    ) -> Result<(), crate::error::CallError> {
        let worker = self.call_service().get()
            .cloned()
            .ok_or(crate::error::CallError::WorkerShutdown)?;
        let cid: crate::domain::ids::CallId = call_id
            .parse()
            .map_err(|_| crate::error::CallError::InvalidCallId)?;
        worker
            .connect_with_token(
                access_token,
                livekit_url,
                encryption_key,
                room_name,
                cid,
                channel_id,
                is_video,
            )
            .await?;
        // Publish default mic immediately. Camera is opt-in via UI toggle.
        worker.publish_microphone(None).await.ok();
        if is_video {
            worker.publish_camera(None).await.ok();
        }
        Ok(())
    }

    /// Leave the currently-joined LiveKit room without clearing the
    /// orchestrator's active-call record. Bridge pairs this with
    /// `end_dm_call` / `end_anon_call` for the full hang-up flow.
    #[cfg(feature = "media")]
    pub async fn call_leave_room(&self) -> Result<(), crate::error::CallError> {
        if let Some(worker) = self.call_service().get().cloned() {
            worker.leave_call().await?;
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

    /// Same as [`call_active_participants`] but resolves each participant's
    /// `display_name` via [`ChatterClient::get_display_name`]. The base
    /// worker snapshot fills `display_name` with the raw user id — this
    /// wrapper replaces it with the friendly name so the FFI caller doesn't
    /// need a second round-trip per participant.
    #[cfg(feature = "media")]
    pub async fn call_active_participants_named(&self) -> Vec<CallParticipantSnapshot> {
        let mut snapshot = self.call_active_participants().await;
        for p in snapshot.iter_mut() {
            if let Ok(uid) = p.identity.parse::<crate::domain::ids::UserId>() {
                p.display_name = self.get_display_name(&uid);
            }
        }
        snapshot
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

    /// Notify the orchestrator that an outgoing / pending call has connected
    /// and should be installed as the active call. Callers use this from the
    /// answer-handling path (caller side of a call).
    #[cfg(feature = "media")]
    pub fn notify_call_connected(
        &self,
        call_id: &str,
        kind: crate::services::call_worker::OrchestratedCallKind,
        peer_id: crate::domain::ids::UserId,
        is_video: bool,
    ) {
        if let Some(worker) = self.call_service().get().cloned() {
            worker.orch_mark_connected(
                crate::services::call_worker::CallHandle::from(call_id),
                kind,
                peer_id,
                is_video,
            );
        }
    }

    /// Notify the orchestrator that a tracked call ended (locally or by the
    /// remote peer). Clears the slot and emits `ActiveEnded` /
    /// `IncomingWithdrawn` depending on where the call lived.
    #[cfg(feature = "media")]
    pub fn notify_call_ended(
        &self,
        call_id: &str,
        reason: crate::services::call_worker::CallEndReason,
    ) {
        if let Some(worker) = self.call_service().get().cloned() {
            worker.orch_mark_ended(
                crate::services::call_worker::CallHandle::from(call_id),
                reason,
            );
        }
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
    pub async fn start_anon_dm_call(
        &self,
        anon_dm_id: &crate::domain::anon_dm::AnonDmId,
        is_video: bool,
        display_name: &str,
    ) -> Result<crate::services::anon_dm::AnonCallOfferResult, ClientError> {
        self.call_signaling()
            .start_anon_dm_call(anon_dm_id, is_video, display_name)
            .await
            .map_err(|e| ClientError::Inbox(e.to_string()))
    }

    /// Start an anonymous group call: acquire token, send offer to all members, store pending state.
    pub async fn start_anon_group_call(
        &self,
        group_id: &crate::domain::anon_group::AnonGroupId,
        is_video: bool,
        display_name: &str,
    ) -> Result<crate::services::anon_dm::AnonCallOfferResult, ClientError> {
        self.call_signaling()
            .start_anon_group_call(group_id, is_video, display_name)
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

    // ── Mobile Call Methods (no desktop-media dependency) ─────────────
    //
    // Thin delegates to `CallSignalingService` — the real orchestration
    // (gRPC create_call / join_call / leave_call, ephemeral key ECDH, sealed
    // sender signaling, pending-call state) lives in the service. These
    // wrappers just translate the service's `CallSignalingError` into
    // `ClientError` for the public SDK surface.

    pub async fn start_dm_call_mobile(
        &self,
        peer_id: &crate::domain::ids::UserId,
        channel_id: crate::domain::ids::ChannelId,
        call_type: crate::domain::call::CallType,
        _user_id: crate::domain::ids::UserId,
    ) -> Result<String, ClientError> {
        self.call_signaling()
            .start_dm_call_mobile(peer_id, channel_id, call_type)
            .await
            .map_err(|e| ClientError::Inbox(e.to_string()))
    }

    pub async fn complete_dm_call_mobile(
        &self,
        peer_ephemeral_pk: [u8; 32],
        call_id: &str,
        peer_id: &crate::domain::ids::UserId,
    ) -> Result<crate::domain::call::MobileCallSetup, ClientError> {
        self.call_signaling()
            .complete_dm_call_mobile(peer_ephemeral_pk, call_id, peer_id)
            .await
            .map_err(|e| ClientError::Inbox(e.to_string()))
    }

    pub async fn accept_dm_call_mobile(
        &self,
        caller_ephemeral_pk: [u8; 32],
        call_id: &str,
        caller_id: &crate::domain::ids::UserId,
    ) -> Result<crate::domain::call::MobileCallSetup, ClientError> {
        self.call_signaling()
            .accept_dm_call_mobile(caller_ephemeral_pk, call_id, caller_id)
            .await
            .map_err(|e| ClientError::Inbox(e.to_string()))
    }

    pub async fn start_group_call_mobile(
        &self,
        channel_id: crate::domain::ids::ChannelId,
        call_type: crate::domain::call::CallType,
        _user_id: crate::domain::ids::UserId,
    ) -> Result<crate::domain::call::MobileCallSetup, ClientError> {
        self.call_signaling()
            .start_group_call_mobile(channel_id, call_type)
            .await
            .map_err(|e| ClientError::Inbox(e.to_string()))
    }

    pub async fn end_dm_call_mobile(
        &self,
        call_id: &str,
        channel_id: &crate::domain::ids::ChannelId,
        duration_secs: i64,
    ) -> Result<(), ClientError> {
        self.call_signaling()
            .end_dm_call_mobile(call_id, channel_id, duration_secs)
            .await
            .map_err(|e| ClientError::Inbox(e.to_string()))
    }

    pub async fn end_anon_call_mobile(
        &self,
        room_name: &str,
        duration_secs: i64,
    ) -> Result<Option<crate::domain::anon_dm::AnonDmId>, ClientError> {
        self.call_signaling()
            .end_anon_call_mobile(room_name, duration_secs)
            .await
            .map_err(|e| ClientError::Inbox(e.to_string()))
    }

    pub async fn handle_remote_hangup_mobile(
        &self,
        call_id: &str,
    ) -> Result<(), ClientError> {
        self.call_signaling()
            .handle_remote_hangup_mobile(call_id)
            .await
            .map_err(|e| ClientError::Inbox(e.to_string()))
    }

    pub async fn accept_anon_call_mobile(
        &self,
        room_name: &str,
    ) -> Result<crate::domain::call::MobileCallSetup, ClientError> {
        self.call_signaling()
            .accept_anon_call_mobile(room_name)
            .await
            .map_err(|e| ClientError::Inbox(e.to_string()))
    }
    // ── Call history ──────────────────────────────────────────────────

    /// Store a call history message for a standard DM/channel call.
    pub fn store_call_history(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
        call_id: &str,
        initiator_id: &crate::domain::ids::UserId,
        initiator_name: &str,
        _is_outgoing: bool,
        duration_secs: i64,
        is_missed: bool,
    ) {
        use crate::domain::message::*;

        let call_type = if is_missed { CallMessageType::Missed } else { CallMessageType::Ended };
        let duration = if duration_secs > 0 {
            Some(chrono::Duration::seconds(duration_secs))
        } else {
            None
        };

        let call_msg = CallMessage {
            call_id: call_id.to_string(),
            call_type,
            initiator_id: *initiator_id,
            initiator_name: initiator_name.to_string(),
            started_at: None,
            ended_at: Some(chrono::Utc::now()),
            duration,
            participant_count: 2,
        };

        let message = Message {
            id: crate::domain::ids::MessageId::new(),
            conversation_id: *channel_id,
            conversation_type: ConversationType::AnonGroup,
            sender_id: Some(*initiator_id),
            device_id: None,
            content: MessageContent::Call(call_msg),
            encrypted_content: None,
            thread_id: None,
            reply_to: None,
            reply_to_author_id: None,
            reply_to_text: None,
            reply_to_thumbnail: None,
            reply_to_thumbnail_mime: None,
            attachments: vec![],
            is_pinned: false,
            is_edited: false,
            is_deleted: false,
            is_read: false,
            sent_at: crate::util::time::TimestampNanos::now().as_u64(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            edited_at: None,
            reaction_counts: vec![],
            current_user_reactions: vec![],
            mentioned_integrations: vec![],
            targets_ai_bot: false,
            delivery_status: String::new(),
        };

        if let Err(e) = self.messages().store_message_local(&message) {
            tracing::error!(error = %e, "Failed to save call history");
        }
    }

    /// Store a call history entry in an anon DM conversation.
    pub fn store_anon_call_history(
        &self,
        conversation_id: &crate::domain::anon_dm::AnonDmId,
        is_outgoing: bool,
        duration_seconds: i64,
    ) {
        let duration_str = if duration_seconds > 0 {
            format!("{}:{:02}", duration_seconds / 60, duration_seconds % 60)
        } else {
            "0:00".to_string()
        };
        let label = if is_outgoing { "Outgoing call" } else { "Incoming call" };
        let text = format!("\u{1F4DE} {} \u{00B7} {}", label, duration_str);
        let sent_at = crate::util::time::TimestampNanos::now().as_u64();
        let user_id = self.auth_state().get().current_user_id
            .unwrap_or_else(crate::domain::ids::UserId::new);

        if let Err(e) = self.anon_dms().store_text_message(
            conversation_id, &user_id, &text, sent_at,
        ) {
            tracing::error!(error = %e, "Failed to store anon call history");
        }
    }

    // ── Local call history (v053) ──────────────────────────────────
    //
    // These methods talk to the `call_history` SQLite table via
    // `CallHistoryRepository`. This is purely client-local state: the
    // server exposes no call-history RPC yet. Entries are produced by
    // `CallSignalingService` on signaling events (start / accept /
    // decline / end / missed) and consumed by the UI bridge.

    /// List recent local call history entries.
    ///
    /// Used by the bridge `get_call_history` FFI. Results are sorted
    /// newest-first. Pass `None` for `cursor` to start at the newest row.
    pub fn list_call_history(
        &self,
        filter: crate::domain::call::CallHistoryFilter,
        limit: u32,
        cursor: Option<(chrono::DateTime<chrono::Utc>, String)>,
    ) -> Result<Vec<crate::domain::call::CallHistoryEntry>, ClientError> {
        self.call_history_repo()
            .list(filter, limit, cursor)
            .map_err(|e| ClientError::Inbox(format!("call history list: {}", e)))
    }

    /// Insert (or replace) a call history entry. Invoked by the call
    /// signaling service — exposed on the client facade so other layers
    /// (e.g. tests, bridge-initiated anon calls) can record entries
    /// without depending on `CallSignalingService` directly.
    pub fn record_call_history(
        &self,
        entry: &crate::domain::call::CallHistoryEntry,
    ) -> Result<(), ClientError> {
        self.call_history_repo()
            .insert_entry(entry)
            .map_err(|e| ClientError::Inbox(format!("call history insert: {}", e)))
    }

    /// Mark a previously-inserted call as ended with a duration.
    pub fn mark_call_history_ended(
        &self,
        id: &str,
        duration_secs: i64,
    ) -> Result<(), ClientError> {
        self.call_history_repo()
            .mark_ended(id, duration_secs)
            .map_err(|e| ClientError::Inbox(format!("call history mark_ended: {}", e)))
    }

    /// Delete a single call history entry (swipe-to-delete in the UI).
    pub fn delete_call_history_entry(&self, id: &str) -> Result<(), ClientError> {
        self.call_history_repo()
            .delete(id)
            .map_err(|e| ClientError::Inbox(format!("call history delete: {}", e)))
    }

    /// Clear all local call history.
    pub fn clear_call_history(&self) -> Result<u64, ClientError> {
        self.call_history_repo()
            .delete_all()
            .map_err(|e| ClientError::Inbox(format!("call history clear: {}", e)))
    }

    /// Accessor used by the history helpers above and by the signaling
    /// service when it wants to record events from inside core.
    fn call_history_repo(
        &self,
    ) -> crate::storage::repositories::CallHistoryRepository {
        crate::storage::repositories::CallHistoryRepository::new(
            self.ui.database.clone(),
        )
    }

    // ── Call lifecycle ─────────────────────────────────────────────

    /// End an active DM/channel call: send hangup, store history, leave.
    ///
    /// `duration_secs` is the call duration from the UI timer.
    #[cfg(feature = "media")]
    pub async fn end_dm_call(&self, duration_secs: i64) -> Result<(), ClientError> {
        let call_worker = self.call_service().get()
            .ok_or_else(|| ClientError::Inbox("call service not initialized".into()))?;
        self.call_signaling()
            .end_dm_call(call_worker, duration_secs)
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
    /// Resolves the call kind from the orchestrator's active-slot and
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
            OrchestratedCallKind::DmStandard | OrchestratedCallKind::GroupChannel => {
                self.end_dm_call(duration_secs).await
            }
        }
    }

    /// Handle a participant departing from a group call.
    ///
    /// If we are the initiator of a channel-based group call, generates a new
    /// random encryption key, rotates it locally, and redistributes to remaining
    /// members so the departed participant can't decrypt future frames.
    ///
    /// Only the initiator rotates to avoid race conditions.
    #[cfg(feature = "media")]
    pub async fn handle_participant_departed(&self) -> Result<(), ClientError> {
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

        // Generate new random key and rotate locally
        let new_key = crate::crypto::generate_group_call_key();
        let epoch = call_worker.rotate_key(new_key).await
            .map_err(|e| ClientError::Inbox(format!("key rotation failed: {e}")))?;

        // Brief delay for LiveKit key propagation before peers receive new-key frames
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

        // Redistribute the new key to remaining channel members
        let call_id = call.id.to_string();
        match self.call_signaling()
            .distribute_group_call_key(&call.channel_id, &call_id, new_key, epoch)
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
    /// Title hint. Empty string means "let the UI use the channel name".
    pub title: String,
}

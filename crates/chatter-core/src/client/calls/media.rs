use super::*;

#[cfg(feature = "media")]
#[derive(Clone, Debug)]
pub struct CallRoomSnapshot {
    pub verification: crate::services::call_worker::CallEncryptionVerification,
    pub call: crate::domain::call::Call,
    /// None while ringing/disconnected: keep the pre-rendered peer presentation.
    pub remotes: Option<Vec<(String, crate::ui::chat_data::CallParticipantInfo)>>,
}

/// Screen presentations survive only while their owner is in the real roster.
#[cfg(feature = "media")]
pub fn room_roster_keeps_screen(
    tile_id: &str,
    tile_name: &str,
    remotes: &[(String, crate::ui::chat_data::CallParticipantInfo)],
    local: Option<&str>,
) -> bool {
    tile_name == crate::calls::tiles::LOCAL_SCREEN_TILE_NAME
        || remotes
            .iter()
            .map(|(id, _)| id.as_str())
            .chain(local)
            .any(|id| crate::calls::screen_share_frame_id(id, true) == tile_id)
}

impl ChatterClient {
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

    /// Flip the active camera between the front and back lens (mobile only —
    /// desktop has a single camera). Core owns the facing state; this just
    /// asks the worker to republish the live track with the opposite lens.
    #[cfg(feature = "media")]
    pub async fn call_swap_camera(&self) -> Result<(), crate::error::CallError> {
        match self.call_service().get().cloned() {
            Some(worker) => worker.swap_camera().await,
            None => Err(crate::error::CallError::WorkerShutdown),
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

    /// Read live membership bracketed by call identity checks. A leave/rejoin
    /// during the read must not attach the previous room's roster to a new call.
    #[cfg(feature = "media")]
    pub async fn call_room_snapshot(&self) -> Option<CallRoomSnapshot> {
        let worker = self.call_service().get()?.clone();
        let initial = worker.verification_snapshot().await?;
        let call = initial.call;
        let connected = matches!(
            worker.room_state().await,
            crate::calls::RoomState::Connected { .. }
        );
        let remotes = if connected && call.connected_at.is_some() {
            let mut identities = worker.remote_participant_identities().await;
            identities.sort();
            identities.dedup();
            Some(
                identities
                    .into_iter()
                    .map(|id| {
                        let info = self.resolve_call_participant(&id);
                        (id, info)
                    })
                    .collect(),
            )
        } else {
            None
        };
        let latest = worker.verification_snapshot().await?;
        let current = latest.call;
        if current.id != call.id
            || current.channel_id != call.channel_id
            || current.connected_at != call.connected_at
        {
            return None;
        }
        Some(CallRoomSnapshot {
            verification: latest.verification,
            call: current,
            remotes,
        })
    }

    /// Recovery UI needs connection identity as well as call identity. Bracket
    /// the existing call/roster snapshot so queued notices cannot acquire a
    /// replacement connection's owner.
    #[cfg(feature = "media")]
    pub async fn call_room_recovery_snapshot(
        &self,
    ) -> Option<(CallRoomSnapshot, u64, crate::calls::RoomState)> {
        let worker = self.call_service().get()?.clone();
        let (generation, state) = worker.room_connection().await?;
        let snapshot = self.call_room_snapshot().await?;
        let (latest_generation, latest_state) = worker.room_connection().await?;
        if generation != latest_generation || state != latest_state {
            return None;
        }
        Some((snapshot, generation, state))
    }

    /// Locally mute/unmute a remote participant's audio. The LiveKit
    /// stream stays subscribed; muted frames are dropped before mixing.
    #[cfg(feature = "media")]
    pub fn call_set_peer_audio_muted(&self, participant_id: String, muted: bool) {
        if let Some(worker) = self.call_service().get() {
            worker.set_peer_audio_muted(participant_id, muted);
        }
    }

    #[cfg(all(feature = "media", not(target_os = "android")))]
    pub async fn call_audio_device_snapshot(&self, output: bool) -> Result<(Vec<crate::calls::AudioDevice>, String), String> {
        match self.call_service().get().cloned() {
            Some(worker) => worker.audio_device_snapshot(output).await,
            None => Err("calls are not available".to_owned()),
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

    #[cfg(all(feature = "media", target_os = "android"))]
    pub async fn call_publish_screen_share(&self) -> Result<(), crate::error::CallError> {
        match self.call_service().get().cloned() {
            Some(worker) => worker.publish_screen_share().await,
            None => Err(crate::error::CallError::WorkerShutdown),
        }
    }

    #[cfg(all(feature = "media", not(target_os = "ios")))]
    pub fn call_stop_screen_share(&self) {
        if let Some(worker) = self.call_service().get() {
            worker.stop_screen_share();
        }
    }

    pub fn call_session_is_current(&self, revision: u64) -> bool {
        self.auth_state().session_is_current(revision)
    }

    /// Authoritative incoming modal/notification state. Never contains signaling keys.
    #[cfg(feature = "media")]
    pub fn incoming_call_presentation_state(
        &self,
    ) -> Option<crate::services::call_worker::IncomingCallPresentationState> {
        self.call_service()
            .get()
            .map(|w| w.incoming_presentation_state())
    }

    /// Subscribe to the orchestrator's broadcast stream. Returns `None`
    /// until the call service has been initialized (post-login).
    #[cfg(feature = "media")]
    pub fn subscribe_call_orchestrator_events(
        &self,
    ) -> Option<tokio::sync::broadcast::Receiver<crate::services::call_worker::CallOrchestratorEvent>>
    {
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
    /// `end_dm_call` for the full hang-up flow.
    #[cfg(feature = "media")]
    pub async fn call_leave_room(&self) -> Result<(), crate::error::CallError> {
        if let Some(worker) = self.call_service().get().cloned() {
            worker
                .leave_call(crate::services::call_worker::CallEndReason::LocalHangup)
                .await?;
        }
        Ok(())
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
}

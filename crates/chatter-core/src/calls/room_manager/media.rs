use super::*;

impl RoomManager {
    // =========================================================================
    // Device Enumeration
    // =========================================================================

    /// List available audio input devices.
    pub fn list_audio_devices() -> Result<Vec<AudioDevice>, CallError> {
        AudioCapture::list_devices()
    }

    /// List available video input devices.
    pub fn list_video_devices() -> Result<Vec<VideoDevice>, CallError> {
        VideoCapture::list_devices()
    }

    /// List available screen sources for sharing.
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    pub fn list_screen_sources() -> Result<Vec<ScreenSource>, CallError> {
        ScreenCapture::list_sources()
    }

    // =========================================================================
    // Track Publishing
    // =========================================================================

    /// Publish microphone audio to the room.
    ///
    /// # Arguments
    /// * `device_id` - Optional device ID (uses default if None)
    pub async fn publish_microphone(&self, device_id: Option<&str>) -> Result<(), CallError> {
        self.publish_microphone_restored(device_id, false).await
    }

    pub(crate) async fn publish_microphone_restored(
        &self,
        device_id: Option<&str>,
        muted: bool,
    ) -> Result<(), CallError> {
        let local_participant = {
            let room = self.room.read();
            room.as_ref()
                .ok_or(CallError::NotInCall)?
                .local_participant()
        };

        // Start audio capture
        let audio_source = {
            let mut capture = self.audio_capture.write();
            let source = capture.start(device_id)?;
            // Pause a restored muted capture before publishing the new track.
            if muted {
                capture.set_muted(true);
            }
            source
        };

        // Create and publish track
        let audio_track =
            LocalAudioTrack::create_audio_track("microphone", RtcAudioSource::Native(audio_source));

        let publish_options = TrackPublishOptions {
            source: TrackSource::Microphone,
            ..Default::default()
        };

        local_participant
            .publish_track(LocalTrack::Audio(audio_track), publish_options)
            .await
            .map_err(|e| CallError::TrackPublishFailed(e.to_string()))?;

        self.microphone_requested
            .store(true, std::sync::atomic::Ordering::SeqCst);
        tracing::info!("Published microphone track");
        Ok(())
    }

    /// Publish camera video to the room.
    ///
    /// # Arguments
    /// * `device_path` - Optional device path (uses default if None)
    /// * `config` - Optional capture configuration
    pub async fn publish_camera(
        &self,
        device_path: Option<&str>,
        config: Option<VideoCaptureConfig>,
    ) -> Result<(), CallError> {
        use std::sync::atomic::Ordering;

        // Already on — guard against double-publish from a rapid re-toggle.
        if self.camera_published.load(Ordering::SeqCst) {
            tracing::info!("Camera already published — ignoring publish");
            return Ok(());
        }

        let local_participant = {
            let room = self.room.read();
            room.as_ref()
                .ok_or(CallError::NotInCall)?
                .local_participant()
        };

        // Tee the local camera to the self-preview ("You") tile, keyed by the
        // local participant identity (== the "You" tile's user_id).
        let local_preview =
            self.shared_frames
                .as_ref()
                .map(|shared| super::super::video_capture::LocalPreview {
                    shared: shared.clone(),
                    local_id: local_participant.identity().to_string(),
                    screen_share: false,
                });

        // Start video capture
        let (video_source, ready_rx) = {
            let mut capture = self.video_capture.write();
            capture.start(device_path, config, local_preview)?
        };

        // The capture thread reports readiness only after a real camera frame.
        // Await without blocking the call worker's local executor.
        if !matches!(
            tokio::time::timeout(std::time::Duration::from_secs(60), ready_rx).await,
            Ok(Ok(()))
        ) {
            self.video_capture.write().stop();
            return Err(CallError::CameraUnavailable);
        }

        // Create and publish track
        let video_track =
            LocalVideoTrack::create_video_track("camera", RtcVideoSource::Native(video_source));

        let publish_options = TrackPublishOptions {
            source: TrackSource::Camera,
            ..Default::default()
        };

        let publication = local_participant
            .publish_track(LocalTrack::Video(video_track), publish_options)
            .await
            .map_err(|e| {
                self.video_capture.write().stop();
                CallError::TrackPublishFailed(e.to_string())
            })?;

        *self.camera_track_sid.write() = Some(publication.sid());
        self.camera_published.store(true, Ordering::SeqCst);
        tracing::info!("Published camera track");
        Ok(())
    }

    /// Publish screen share to the room.
    ///
    /// # Arguments
    /// * `source` - Screen source to capture
    /// * `config` - Optional capture configuration
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    pub async fn publish_screen_share(
        &self,
        source: ScreenSource,
        config: Option<ScreenCaptureConfig>,
    ) -> Result<(), CallError> {
        let local_participant = {
            let room = self.room.read();
            room.as_ref()
                .ok_or(CallError::NotInCall)?
                .local_participant()
        };

        // Two-phase startup so the `NativeVideoSource` is sized to the
        // real capture dims (e.g. 5120×1440). Previously we seeded at a
        // fixed 7680×4320 — libwebrtc then upscaled every capture frame
        // to that size before encode, and receivers decoded 33 MP frames
        // (~150 ms libyuv convert per frame, starving the tokio worker
        // and overflowing the audio relay queue).
        // Capture the negotiated frame rate before `config` is moved into
        // `begin`, so the LiveKit publish cap matches what the capturer
        // actually produces (otherwise raising one without the other is a
        // no-op — see the three coupled "60 fps" sites).
        let target_fps = config
            .as_ref()
            .map(|c| c.fps)
            .unwrap_or_else(|| ScreenCaptureConfig::default().fps);

        // Tee the local screen feed to its OWN tile (throttled inside the
        // capture loop), keyed by `screen_share_frame_id` — NOT the bare local
        // id — so it doesn't overwrite the camera in the shared "You" slot.
        let local_preview =
            self.shared_frames
                .as_ref()
                .map(|shared| super::super::video_capture::LocalPreview {
                    shared: shared.clone(),
                    local_id: super::super::screen_share_frame_id(
                        &local_participant.identity().to_string(),
                        true,
                    ),
                    screen_share: true,
                });

        let handshake = {
            let mut capture = self.screen_capture.write();
            capture.begin(source, config, local_preview)?
        };

        // Wait for the capture thread's probe frame to come back with
        // real dims. Portal selection can take a while; the timeout is
        // generous but bounded so a cancelled portal doesn't hang the
        // call worker forever.
        let dims_result =
            tokio::time::timeout(std::time::Duration::from_secs(120), handshake.dims_rx).await;

        let (width, height) = match dims_result {
            Ok(Ok(d)) => d,
            Ok(Err(_)) => {
                self.screen_capture.write().stop();
                return Err(CallError::TrackPublishFailed(
                    "Screen capture thread exited before sending probe dims".to_string(),
                ));
            }
            Err(_) => {
                self.screen_capture.write().stop();
                return Err(CallError::TrackPublishFailed(
                    "Timed out waiting for screen capture probe frame (portal dialog cancelled?)"
                        .to_string(),
                ));
            }
        };

        let video_source = NativeVideoSource::new(
            VideoResolution { width, height },
            true, // is_screencast
        );

        // Hand the source to the capture thread so it can start pushing
        // frames (including the buffered probe frame). If the channel is
        // dropped the thread has already given up.
        if handshake.source_tx.send(video_source.clone()).is_err() {
            self.screen_capture.write().stop();
            return Err(CallError::TrackPublishFailed(
                "Screen capture thread exited before source handoff".to_string(),
            ));
        }

        self.screen_capture.read().complete();

        let video_track =
            LocalVideoTrack::create_video_track("screen", RtcVideoSource::Native(video_source));

        // Screen-share track tuned for high-res content (e.g. 5120×1440
        // ultrawides). Settings validated by a live end-to-end wire sweep
        // (examples/screenshare_wire_bench.rs, since removed; see
        // `git log -- crates/chatter-core/examples`). The headline result at 5120×1440:
        //   default (MaintainResolution)      →  30 fps, 149 ms latency
        //   THIS (Disabled + H265 + NVENC)    → 104 fps,  30-45 ms, PSNR ~44
        // i.e. 3.5× the framerate at FULL native resolution, visually lossless.
        //
        // degradation_preference = MaintainFramerateAndResolution (== the old
        // "Disabled") is the single most important knob. The SDK defaults a
        // Screenshare source to MaintainResolution, which tells libwebrtc's
        // CPU-overuse detector to hold resolution and DROP FRAMERATE under load —
        // that was our hard ~30-35 fps wall, codec- and bitrate-independent
        // (measured flat from 60→800 Mbps). Disabled throttles neither axis.
        //
        // video_encoder = Nvenc forces the HW HEVC path (confirmed engaged via
        // NVENC session-init logs). H.265 is the winner ONLY once the throttle is
        // off: at the wall all codecs tied ~33 fps, but with Disabled+NVENC H.265
        // holds 104 fps steady while SW VP8/VP9 manage 45-75 and thrash. Our
        // earlier "H.265 = 36 fps regression" was MaintainResolution strangling
        // it, not the codec. On non-NVIDIA hosts the SDK falls back (VAAPI/SW);
        // per-GPU encoder selection is a follow-up.
        //
        // simulcast=false runs a single encoder and, with an explicit
        // `video_encoding`, keeps us off the hardcoded 1080p/30/3 Mbps
        // screenshare preset (which otherwise caps ~27 fps at any resolution).
        //
        // Bitrate CEILING scales with pixel count (~16 bits/px), clamped
        // [10, 150] Mbps. The sweep showed H.265 is near-lossless (PSNR 44) even
        // at 60 Mbps here, and fps is flat vs bitrate — this is just headroom, a
        // ceiling the congestion controller stays under on constrained links.
        let pixels = (width as u64) * (height as u64);
        let max_bitrate = (pixels * 16).clamp(10_000_000, 150_000_000);
        let publish_options = TrackPublishOptions {
            source: TrackSource::Screenshare,
            video_codec: VideoCodec::H265,
            video_encoder: VideoEncoderBackend::Nvenc,
            degradation_preference: Some(DegradationPreference::MaintainFramerateAndResolution),
            simulcast: false,
            video_encoding: Some(VideoEncoding {
                max_bitrate,
                max_framerate: target_fps as f64,
            }),
            ..Default::default()
        };

        let publication = local_participant
            .publish_track(LocalTrack::Video(video_track), publish_options)
            .await
            .map_err(|e| CallError::TrackPublishFailed(e.to_string()))?;

        let sid = publication.sid();
        *self.screen_share_track_sid.write() = Some(sid.clone());

        tracing::info!(
            "Published screen share track (H.265/NVENC, degradation=disabled, simulcast off, {} fps cap, {} Mbps, {}x{}, sid={})",
            target_fps,
            max_bitrate / 1_000_000,
            width,
            height,
            sid
        );
        Ok(())
    }

    #[cfg(target_os = "android")]
    pub async fn publish_screen_share(&self) -> Result<(), CallError> {
        use super::super::android_screen;
        let local_participant = {
            let room = self.room.read();
            room.as_ref()
                .ok_or(CallError::NotInCall)?
                .local_participant()
        };
        let preview =
            self.shared_frames
                .as_ref()
                .map(|shared| super::super::video_capture::LocalPreview {
                    shared: shared.clone(),
                    local_id: super::super::screen_share_frame_id(
                        &local_participant.identity().to_string(),
                        true,
                    ),
                    screen_share: true,
                });
        let (token, ready) =
            android_screen::begin(preview).map_err(CallError::TrackPublishFailed)?;
        let dims = tokio::time::timeout(std::time::Duration::from_secs(120), ready).await;
        let (width, height) = match dims {
            Ok(Ok(Ok(dims))) => dims,
            Ok(Ok(Err(error))) => {
                android_screen::stop();
                return Err(CallError::TrackPublishFailed(error));
            }
            _ => {
                android_screen::stop();
                return Err(CallError::TrackPublishFailed(
                    "Screen share unavailable".into(),
                ));
            }
        };
        let source = NativeVideoSource::new(VideoResolution { width, height }, true);
        if !android_screen::install_source(token, source.clone()) {
            return Err(CallError::TrackPublishFailed("Screen share stopped".into()));
        }
        let track = LocalVideoTrack::create_video_track("screen", RtcVideoSource::Native(source));
        let options = TrackPublishOptions {
            source: TrackSource::Screenshare,
            ..Default::default()
        };
        let publication = local_participant
            .publish_track(LocalTrack::Video(track), options)
            .await
            .map_err(|error| {
                android_screen::stop();
                CallError::TrackPublishFailed(error.to_string())
            })?;
        *self.screen_share_track_sid.write() = Some(publication.sid());
        if !android_screen::is_running() {
            self.stop_screen_share().await;
            return Err(CallError::TrackPublishFailed("Screen share stopped".into()));
        }
        Ok(())
    }

    /// Stop microphone capture and unpublish track.
    pub fn stop_microphone(&self) {
        self.microphone_requested
            .store(false, std::sync::atomic::Ordering::SeqCst);
        let mut capture = self.audio_capture.write();
        capture.stop();
        // Note: Track will be auto-unpublished when room disconnects
        // For explicit unpublish, we'd need to track the publication
    }

    /// Stop camera capture AND unpublish the LiveKit track.
    ///
    /// Without the unpublish step the SFU keeps the track alive after the local
    /// capture loop ends, so remote viewers freeze on the last frame instead of
    /// showing the avatar. Mirrors `stop_screen_share`.
    pub async fn stop_camera(&self) {
        {
            let mut capture = self.video_capture.write();
            capture.stop();
        }
        self.camera_published
            .store(false, std::sync::atomic::Ordering::SeqCst);
        let sid = self.camera_track_sid.write().take();
        let Some(sid) = sid else { return };
        let local_participant = {
            let room = self.room.read();
            match room.as_ref() {
                Some(r) => r.local_participant(),
                None => return,
            }
        };
        // Drop the self-preview frame so the "You" tile reverts to the avatar.
        if let Some(ref shared) = self.shared_frames {
            shared.remove_frame(&local_participant.identity().to_string());
        }
        match local_participant.unpublish_track(&sid).await {
            Ok(_) => tracing::info!("Unpublished camera track sid={}", sid),
            Err(e) => tracing::warn!("Failed to unpublish camera track {}: {}", sid, e),
        }
    }

    /// Stop screen share capture AND unpublish the LiveKit track.
    ///
    /// Without the unpublish step the SFU keeps the track alive after the
    /// local capture loop ends. Remote viewers stay subscribed to a
    /// frozen stream; restarting screen share creates a second track on
    /// top of the dead one and the receiver tile picks whichever
    /// subscription it had first.
    #[cfg(not(target_os = "ios"))]
    pub async fn stop_screen_share(&self) {
        #[cfg(not(target_os = "android"))]
        {
            let mut capture = self.screen_capture.write();
            capture.stop();
        }
        #[cfg(target_os = "android")]
        super::super::android_screen::stop();
        let sid = self.screen_share_track_sid.write().take();
        let Some(sid) = sid else { return };
        let local_participant = {
            let room = self.room.read();
            match room.as_ref() {
                Some(r) => r.local_participant(),
                None => return,
            }
        };
        match local_participant.unpublish_track(&sid).await {
            Ok(_) => tracing::info!("Unpublished screen share track sid={}", sid),
            Err(e) => tracing::warn!("Failed to unpublish screen share track {}: {}", sid, e),
        }
        // Drop the local screen frame (its own tile key) so the screen tile
        // clears. The camera tile (bare local id) is untouched.
        if let Some(ref shared) = self.shared_frames {
            shared.remove_frame(&super::super::screen_share_frame_id(
                &local_participant.identity().to_string(),
                true,
            ));
        }
    }

    /// Toggle microphone mute.
    pub fn set_microphone_muted(&self, muted: bool) {
        let mut capture = self.audio_capture.write();
        capture.set_muted(muted);
    }

    pub(crate) fn reconnect_microphone_device(&self) -> Option<String> {
        self.microphone_requested
            .load(std::sync::atomic::Ordering::SeqCst)
            .then(|| self.audio_capture.read().device_name().to_string())
    }

    /// Check if microphone is muted.
    pub fn is_microphone_muted(&self) -> bool {
        self.audio_capture.read().is_muted()
    }

    /// Snapshot the effective selection before disconnect clears publication.
    pub(crate) fn reconnect_camera_selection(&self) -> Option<(String, VideoCaptureConfig)> {
        self.is_camera_enabled()
            .then(|| self.video_capture.read().reconnect_selection())
    }

    /// Check if the camera is currently on (track published).
    pub fn is_camera_enabled(&self) -> bool {
        self.camera_published
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// A published track whose camera thread exited must be unpublished.
    pub fn camera_capture_failed(&self) -> bool {
        self.is_camera_enabled() && !self.video_capture.read().is_running()
    }

    /// Check if screen sharing is active.
    pub fn is_screen_sharing(&self) -> bool {
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        {
            self.screen_capture.read().is_running()
        }
        #[cfg(target_os = "android")]
        {
            super::super::android_screen::is_running()
        }
        #[cfg(target_os = "ios")]
        {
            false
        }
    }

    #[cfg(target_os = "android")]
    pub fn screen_share_capture_failed(&self) -> bool {
        self.screen_share_track_sid.read().is_some() && !super::super::android_screen::is_running()
    }

    /// Set playback volume for remote audio (0.0 to 1.0).
    pub fn set_playback_volume(&self, volume: f32) {
        if let Some(playback) = self.audio_playback.read().as_ref() {
            playback.set_volume(volume);
        }
    }

    /// Route remote audio to the named output device (see
    /// [`AudioPlayback::set_output_device`]).
    pub fn set_audio_output(&self, device: &str) -> Result<(), CallError> {
        match self.audio_playback.read().as_ref() {
            Some(playback) => playback.set_output_device(device),
            None => Err(CallError::DeviceError(
                "Audio playback unavailable".to_string(),
            )),
        }
    }

    /// Get current playback volume.
    pub fn playback_volume(&self) -> f32 {
        self.audio_playback
            .read()
            .as_ref()
            .map(|p| p.volume())
            .unwrap_or(1.0)
    }

    /// Locally mute/unmute a specific remote participant's audio. Frames
    /// keep decoding so the LiveKit stream stays healthy; muted frames are
    /// dropped before mixing.
    pub fn set_peer_audio_muted(&self, participant_id: &str, muted: bool) {
        if let Some(playback) = self.audio_playback.read().as_ref() {
            playback.set_peer_muted(participant_id, muted);
        }
    }

    /// Whether a remote participant is locally audio-muted.
    pub fn is_peer_audio_muted(&self, participant_id: &str) -> bool {
        self.audio_playback
            .read()
            .as_ref()
            .map(|p| p.is_peer_muted(participant_id))
            .unwrap_or(false)
    }

    /// Locally pause/resume rendering of a remote participant's video. The
    /// LiveKit subscription stays up; frames decode but are dropped before
    /// storage. The UI tile falls back to the avatar while paused.
    pub fn set_peer_video_paused(&self, participant_id: &str, paused: bool) {
        self.video_playback
            .read()
            .set_peer_paused(participant_id, paused);
    }

    /// Whether a remote participant's video is locally paused.
    pub fn is_peer_video_paused(&self, participant_id: &str) -> bool {
        self.video_playback.read().is_peer_paused(participant_id)
    }

    /// Get the latest video frame for a participant as raw data.
    ///
    /// Returns None if no frame is available for the participant.
    pub fn get_video_frame(
        &self,
        participant_id: &str,
    ) -> Option<super::super::shared_frames::VideoFrameData> {
        self.video_playback.read().get_frame(participant_id)
    }

    /// Get all remote video frames as raw data.
    pub fn get_all_video_frames(&self) -> Vec<super::super::shared_frames::VideoFrameData> {
        self.video_playback.read().get_all_frames()
    }

    /// Check if we have a video frame available for a participant.
    pub fn has_video_frame(&self, participant_id: &str) -> bool {
        self.video_playback.read().has_frame(participant_id)
    }

    /// Get the number of active video streams.
    pub fn video_stream_count(&self) -> usize {
        self.video_playback.read().stream_count()
    }
}

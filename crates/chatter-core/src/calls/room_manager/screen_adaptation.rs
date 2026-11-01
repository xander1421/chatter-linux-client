//! Single-layer screen-share targets driven by publisher BWE, with no frame queue.
use super::*;
use livekit::webrtc::stats::{IceCandidatePairState, QualityLimitationReason, RtcStats};

use super::super::screen_share_policy::Controller;

fn metered() -> bool {
    #[cfg(target_os = "android")]
    {
        super::super::android_screen::is_metered()
    }
    #[cfg(not(target_os = "android"))]
    {
        false
    }
}

impl RoomManager {
    pub(super) fn start_screen_adaptation(
        &self,
        track: LocalVideoTrack,
        width: u32,
        height: u32,
        fps: u32,
    ) {
        if let Some(task) = self.screen_adaptation.write().take() {
            task.abort();
        }
        let room = self.room.clone();
        let sid = self.screen_share_track_sid.clone();
        let expected_sid = sid.read().clone();
        let mut controller = Controller::new(width, height, fps, metered());
        let task = tokio::spawn(async move {
            let mut applied = None;
            let mut encoder = String::new();
            let mut ticks = tokio::time::interval(std::time::Duration::from_secs(1));
            ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                ticks.tick().await;
                if *sid.read() != expected_sid {
                    break;
                }
                let is_metered = metered();
                // Apply the initial metered target before waiting for the first stats.
                let target = controller.target(is_metered);
                if applied != Some(target) {
                    match track.set_screen_share_encoding(target.bitrate, target.fps, target.scale)
                    {
                        Ok(()) => {
                            #[cfg(target_os = "android")]
                            super::super::android_screen::set_target_fps(target.fps.ceil() as u32);
                            tracing::info!(
                                ?target,
                                metered = is_metered,
                                "Screen-share adaptive target"
                            );
                            applied = Some(target);
                        }
                        Err(error) => tracing::warn!(%error, "Screen-share adaptation failed"),
                    }
                }
                let room_stats = {
                    let guard = room.read();
                    let Some(room) = guard.as_ref() else {
                        break;
                    };
                    if room.connection_state() == livekit::ConnectionState::Disconnected {
                        break;
                    }
                    room.get_stats()
                };
                let stats = tokio::time::timeout(std::time::Duration::from_secs(2), async {
                    Ok::<_, livekit::RoomError>((
                        room_stats
                            .await
                            .map_err(|e| livekit::RoomError::Internal(e.to_string()))?,
                        track.get_stats().await?,
                    ))
                })
                .await;
                let Ok(Ok((session, track_stats))) = stats else {
                    continue;
                };
                let screen_ssrcs: Vec<u32> = track_stats
                    .iter()
                    .filter_map(|stat| {
                        if let RtcStats::OutboundRtp(out) = stat {
                            Some(out.stream.ssrc)
                        } else {
                            None
                        }
                    })
                    .collect();
                let mut other_media_bitrate = 0u64;
                let mut available = None;
                for stat in session.publisher_stats {
                    if let RtcStats::OutboundRtp(out) = &stat {
                        if !screen_ssrcs.is_empty()
                            && !screen_ssrcs.contains(&out.stream.ssrc)
                            && out.outbound.target_bitrate.is_finite()
                            && out.outbound.target_bitrate > 0.0
                        {
                            other_media_bitrate = other_media_bitrate
                                .saturating_add(out.outbound.target_bitrate as u64);
                        }
                    }
                    if let RtcStats::CandidatePair(pair) = stat {
                        let pair = pair.candidate_pair;
                        if pair.nominated
                            && pair.state == Some(IceCandidatePairState::Succeeded)
                            && pair.available_outgoing_bitrate.is_finite()
                            && pair.available_outgoing_bitrate > 0.0
                        {
                            available = Some(pair.available_outgoing_bitrate as u64);
                        }
                    }
                }
                // Publisher BWE is shared with microphone/camera senders.
                let available = available.map(|bps: u64| bps.saturating_sub(other_media_bitrate));
                let mut limited = false;
                for stat in track_stats {
                    if let RtcStats::OutboundRtp(out) = stat {
                        let out = out.outbound;
                        limited |=
                            out.quality_limitation_reason == QualityLimitationReason::Bandwidth;
                        if out.frames_encoded > 0
                            && !out.encoder_implementation.is_empty()
                            && encoder != out.encoder_implementation
                        {
                            encoder = out.encoder_implementation;
                            tracing::info!(implementation = %encoder, power_efficient = out.power_efficient_encoder,
                                fps = out.frames_per_second, width = out.frame_width, height = out.frame_height,
                                "Screen-share encoder engaged (SDK outbound stats)");
                            #[cfg(target_os = "android")]
                            if !out.power_efficient_encoder {
                                tracing::warn!(implementation = %encoder, "Android SDK did not report a power-efficient encoder; inspect implementation and verify MediaCodec hardware on device");
                            }
                        }
                    }
                }
                controller.observe(available, limited, is_metered);
            }
        });
        *self.screen_adaptation.write() = Some(task);
    }
}

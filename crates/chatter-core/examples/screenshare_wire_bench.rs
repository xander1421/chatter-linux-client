//! Live end-to-end screen-share WIRE benchmark.
//!
//! `screenshare_pipeline_bench` measures only the CPU convert stages we own and
//! explicitly punts on "wire/decode/encode owned by libwebrtc". This harness
//! closes that gap: it publishes a synthetic screen-content track to a real
//! LiveKit server and subscribes to it in the SAME process, then sweeps a
//! matrix of publish configs (codec × simulcast × explicit encoding) and
//! reports, per config, the metrics that actually matter for screen share:
//!
//!   - received fps   (frames/sec the subscriber decodes off the wire)
//!   - latency        (p50 / p95, glass-to-glass minus render — capture→decode)
//!   - quality        (PSNR of decoded luma vs the known source pattern)
//!
//! How the metrics are ground-truthed without clock sync or frame storage:
//!   - Publisher and subscriber share one process → one monotonic `Instant`
//!     epoch, so latency needs no NTP/RTP correlation.
//!   - Each frame's luma is a deterministic function of (x, y, frame_index):
//!     `Y = (x + y + index*8) & 0xFF` — moving diagonal high-frequency stripes
//!     that stress the codec like real screen content (text/motion).
//!   - The frame_index is stamped into a row of high-contrast 40px blocks at
//!     the top of the luma plane (survives compression); the subscriber decodes
//!     it, looks up the publish `Instant` for that index (latency), and
//!     regenerates the expected luma for that index (PSNR). No frame buffers
//!     are retained.
//!
//! E2EE is OFF here on purpose — we're isolating the encode/wire/decode path,
//! not the GCM frame-crypto overhead (which is codec-independent and constant).
//!
//! Needs a LiveKit server (the same `docker-compose` dev one the app uses).
//!   LIVEKIT_URL        default ws://127.0.0.1:7880
//!   LIVEKIT_API_KEY    default devkey
//!   LIVEKIT_API_SECRET default secret
//!   BENCH_WIDTHS       default "5120x1440,2560x720"  (comma list of WxH)
//!   BENCH_FPS          default 144   (push target; measures max achievable)
//!   BENCH_SECS         default 8     (measurement window per config, after 2s warmup)
//!
//! Run RELEASE (debug libvpx/nvenc glue skews every number):
//!   cargo run --release --example screenshare_wire_bench -p chatter-core

#[cfg(not(feature = "desktop-media"))]
fn main() {
    eprintln!("This example requires the `desktop-media` feature (enabled by default).");
}

#[cfg(feature = "desktop-media")]
fn main() {
    bench::run();
}

#[cfg(feature = "desktop-media")]
mod bench {
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use livekit::options::{
        DegradationPreference, TrackPublishOptions, VideoCodec, VideoEncoderBackend, VideoEncoding,
    };
    use livekit::prelude::*;
    use livekit::webrtc::video_frame::{I420Buffer, VideoBuffer, VideoFrame, VideoRotation};
    use livekit::webrtc::video_source::native::NativeVideoSource;
    use livekit::webrtc::video_source::{RtcVideoSource, VideoResolution};
    use livekit::webrtc::video_stream::native::NativeVideoStream;
    use parking_lot::Mutex;
    use tokio::sync::mpsc;

    // Index stamp layout: 20 blocks across the top, one bit each.
    const INDEX_BITS: u32 = 20;
    const BLOCK_W: u32 = 40;
    const BLOCK_PITCH: u32 = 48;
    const BLOCK_Y0: u32 = 16;
    const BLOCK_Y1: u32 = 56;
    // Rows below this are pure pattern (no stamp) → used for PSNR.
    const PSNR_Y0: u32 = 128;
    // Publish-time ring keyed by index & (RING-1).
    const RING: usize = 8192;
    // Distinct luma planes precomputed once and cycled — motion for the codec
    // without re-synthesising 7.4M pixels per frame on the push thread (that
    // scalar synth was itself capping send fps ~45 at 5120×1440 and masking the
    // real wire/encode ceiling). Content still scrolls every frame; the 16-frame
    // repeat is beyond any codec's inter-frame reference window here.
    const MOTION: u32 = 16;

    /// Expected luma for a pixel — the shared source-of-truth generator. Uses
    /// `index % MOTION` so a precomputed plane ring reproduces it exactly for
    /// PSNR. Consecutive indices differ by 8 everywhere → full-frame motion.
    #[inline]
    fn expected_y(x: u32, y: u32, index: u32) -> u8 {
        ((x.wrapping_add(y).wrapping_add((index % MOTION).wrapping_mul(8))) & 0xFF) as u8
    }

    /// Precompute the MOTION luma planes for a resolution (tight w*h, no stride).
    fn precompute_planes(w: u32, h: u32) -> Arc<Vec<Vec<u8>>> {
        let mut planes = Vec::with_capacity(MOTION as usize);
        for k in 0..MOTION {
            let mut p = vec![0u8; (w * h) as usize];
            for y in 0..h {
                let base = (y * w) as usize;
                for x in 0..w {
                    p[base + x as usize] = expected_y(x, y, k);
                }
            }
            planes.push(p);
        }
        Arc::new(planes)
    }

    struct ConfigCase {
        name: String,
        codec: VideoCodec,
        max_bitrate: u64,
        // Max source width this codec tolerates (H.264 NVENC ~4096). 0 = no cap.
        max_width: u32,
    }

    /// Codec × bitrate sweep, simulcast off + explicit encoding (the only shape
    /// that clears the ~27fps screenshare default). Bitrate climbs well past
    /// what a link needs so we find where *encoder compute* — not bandwidth —
    /// caps fps; that ceiling is codec-specific (HW NVENC vs SW libvpx) and is
    /// what decides whether 240Hz native is reachable at all.
    fn matrix() -> Vec<ConfigCase> {
        // (label, codec, max source width). H264 capped at 4096 → skipped at 5120.
        let codecs = [
            ("vp8", VideoCodec::VP8, 0u32),
            ("vp9", VideoCodec::VP9, 0u32),
            ("h264", VideoCodec::H264, 4096u32),
            ("h265", VideoCodec::H265, 0u32),
        ];
        let rates_mbps: &[u64] = &[60, 120, 240, 480, 800];
        let mut v = Vec::new();
        for (label, codec, max_width) in codecs {
            for &mbps in rates_mbps {
                v.push(ConfigCase {
                    name: format!("{label}@{mbps}M"),
                    codec,
                    max_bitrate: mbps * 1_000_000,
                    max_width,
                });
            }
        }
        v
    }

    /// Shared publish-time ring so the subscriber can turn a decoded frame index
    /// into a latency. `epoch` lets us discard frames left over from the prior
    /// config still draining through the SFU when a new run starts.
    struct Timing {
        publish_at: Mutex<Vec<Option<Instant>>>,
        epoch: AtomicU32,
    }

    /// Hand-mint a LiveKit access token (HS256 JWT) — avoids depending on the
    /// livekit-api crate, which version-skews against livekit 0.7.52's protocol.
    fn mint(api_key: &str, api_secret: &str, room: &str, identity: &str) -> String {
        use base64::Engine;
        use hmac::{Hmac, Mac};
        use sha2::Sha256;
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let header = b64.encode(br#"{"alg":"HS256","typ":"JWT"}"#);
        let payload = b64.encode(format!(
            r#"{{"iss":"{api_key}","sub":"{identity}","name":"{identity}","nbf":{now},"exp":{exp},"video":{{"room":"{room}","roomJoin":true,"canPublish":true,"canSubscribe":true}}}}"#,
            exp = now + 3600,
        ));
        let signing_input = format!("{header}.{payload}");
        let mut mac = Hmac::<Sha256>::new_from_slice(api_secret.as_bytes()).expect("hmac key");
        mac.update(signing_input.as_bytes());
        let sig = b64.encode(mac.finalize().into_bytes());
        format!("{signing_input}.{sig}")
    }

    pub fn run() {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn,screenshare_wire_bench=info")),
            )
            .init();

        let url = std::env::var("LIVEKIT_URL").unwrap_or_else(|_| "ws://127.0.0.1:7880".into());
        let api_key = std::env::var("LIVEKIT_API_KEY").unwrap_or_else(|_| "devkey".into());
        let api_secret = std::env::var("LIVEKIT_API_SECRET").unwrap_or_else(|_| "secret".into());
        let target_fps: f64 = std::env::var("BENCH_FPS").ok().and_then(|s| s.parse().ok()).unwrap_or(240.0);
        let secs: u64 = std::env::var("BENCH_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(8);
        let widths = std::env::var("BENCH_WIDTHS").unwrap_or_else(|_| "5120x1440".into());
        let resolutions: Vec<(u32, u32)> = widths
            .split(',')
            .filter_map(|s| {
                let (w, h) = s.trim().split_once('x')?;
                Some((w.parse().ok()?, h.parse().ok()?))
            })
            .collect();

        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("rt");
        rt.block_on(async move {
            let room_name = format!("wire-bench-{}", std::process::id());
            let pub_token = mint(&api_key, &api_secret, &room_name, "publisher");
            let sub_token = mint(&api_key, &api_secret, &room_name, "subscriber");

            let mut opts = RoomOptions::default();
            opts.auto_subscribe = true;
            let (pub_room, _pub_events) = match Room::connect(&url, &pub_token, opts.clone()).await {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("publisher connect failed: {e}\nIs LiveKit up at {url}? (docker-compose up -d)");
                    return;
                }
            };
            let (sub_room, sub_events) = match Room::connect(&url, &sub_token, opts).await {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("subscriber connect failed: {e}");
                    return;
                }
            };
            let pub_room = Arc::new(pub_room);
            let sub_room = Arc::new(sub_room);

            let timing = Arc::new(Timing {
                publish_at: Mutex::new(vec![None; RING]),
                epoch: AtomicU32::new(0),
            });

            // One subscriber reader task lives for the whole run. It hands each
            // freshly subscribed RemoteVideoTrack to the metrics loop over a
            // channel; the metrics loop owns measurement windowing.
            let (track_tx, mut track_rx) = mpsc::unbounded_channel::<RemoteVideoTrack>();
            {
                let mut events = sub_events;
                tokio::spawn(async move {
                    while let Some(ev) = events.recv().await {
                        if let RoomEvent::TrackSubscribed { track: RemoteTrack::Video(vt), .. } = ev {
                            let _ = track_tx.send(vt);
                        }
                    }
                });
            }

            println!(
                "\n=== screenshare wire bench ===  push_target={target_fps}fps  window={secs}s  E2EE=off\n\
                 server={url}\n"
            );
            println!(
                "{:<26} {:>11} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>7}",
                "config", "src_res", "send_fps", "recv_fps", "recv_res", "lat_p50", "lat_p95", "psnr_dB", "frames"
            );
            println!("{}", "-".repeat(104));

            for (w, h) in &resolutions {
                let (w, h) = (*w, *h);
                // Synthesise the motion planes ONCE per resolution (was per
                // frame — that scalar synth capped the push thread ~45fps at
                // 5120×1440 and masked the true pipeline ceiling).
                let planes = precompute_planes(w, h);
                for case in matrix() {
                    if case.max_width != 0 && w > case.max_width {
                        println!("{:<30} {:>7}x{:<3} {:>9}  (skipped: codec max width {})", case.name, w, h, "-", case.max_width);
                        continue;
                    }
                    let line = run_one(
                        &pub_room, &sub_room, &timing, &mut track_rx, &case, &planes, w, h, target_fps, secs,
                    )
                    .await;
                    println!("{line}");
                }
            }

            println!("\n(PSNR>=40 visually lossless, 30-40 good, <30 soft. lat = capture→decode, ms.)");
            pub_room.close().await.ok();
            sub_room.close().await.ok();
        });
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_one(
        pub_room: &Arc<Room>,
        _sub_room: &Arc<Room>,
        timing: &Arc<Timing>,
        track_rx: &mut mpsc::UnboundedReceiver<RemoteVideoTrack>,
        case: &ConfigCase,
        planes: &Arc<Vec<Vec<u8>>>,
        w: u32,
        h: u32,
        target_fps: f64,
        secs: u64,
    ) -> String {
        // Fresh epoch: subscriber ignores any in-flight frames from a prior run.
        let epoch = timing.epoch.fetch_add(1, Ordering::SeqCst) + 1;
        for slot in timing.publish_at.lock().iter_mut() {
            *slot = None;
        }

        // --- publisher: source + push thread ---
        // is_screencast=true sets the WebRTC content hint (Text/Detailed) which
        // enables the quality scaler + biases to maintain-resolution (drop fps).
        // BENCH_SCREENCAST=false flips it to test whether that content hint is
        // the ~35fps governor on this SDK (0.7.37 has no degradation_preference).
        let is_screencast = std::env::var("BENCH_SCREENCAST").map(|v| v != "false").unwrap_or(true);
        let source = NativeVideoSource::new(VideoResolution { width: w, height: h }, is_screencast);
        let track = LocalVideoTrack::create_video_track(
            "screen",
            RtcVideoSource::Native(source.clone()),
        );
        // BENCH_DEGRADATION: disabled|framerate|resolution|balanced (default =
        // SDK default, which is MaintainResolution for screenshare). "disabled"
        // = MaintainFramerateAndResolution = throttle NEITHER axis.
        let degradation = match std::env::var("BENCH_DEGRADATION").as_deref() {
            Ok("disabled") => Some(DegradationPreference::MaintainFramerateAndResolution),
            Ok("framerate") => Some(DegradationPreference::MaintainFramerate),
            Ok("resolution") => Some(DegradationPreference::MaintainResolution),
            Ok("balanced") => Some(DegradationPreference::Balanced),
            _ => None,
        };
        // BENCH_ENCODER: auto|nvenc|vaapi|software (default auto).
        let encoder = match std::env::var("BENCH_ENCODER").as_deref() {
            Ok("nvenc") => VideoEncoderBackend::Nvenc,
            Ok("vaapi") => VideoEncoderBackend::Vaapi,
            Ok("software") => VideoEncoderBackend::Software,
            _ => VideoEncoderBackend::Auto,
        };
        let opts = TrackPublishOptions {
            source: TrackSource::Screenshare,
            video_codec: case.codec,
            simulcast: false,
            video_encoding: Some(VideoEncoding { max_bitrate: case.max_bitrate, max_framerate: target_fps }),
            degradation_preference: degradation,
            video_encoder: encoder,
            ..Default::default()
        };
        let publication = match pub_room.local_participant().publish_track(LocalTrack::Video(track), opts).await {
            Ok(p) => p,
            Err(e) => return format!("{:<30} {:>7}x{:<3}  publish FAILED: {e}", case.name, w, h),
        };
        let sid = publication.sid();

        let stop = Arc::new(AtomicBool::new(false));
        let push = {
            let stop = stop.clone();
            let timing = timing.clone();
            let planes = planes.clone();
            std::thread::spawn(move || push_loop(source, planes, w, h, target_fps, timing, stop))
        };

        // Wait for the subscriber to actually subscribe to THIS track.
        let track = match tokio::time::timeout(Duration::from_secs(10), track_rx.recv()).await {
            Ok(Some(t)) => t,
            _ => {
                stop.store(true, Ordering::SeqCst);
                let _ = push.join();
                pub_room.local_participant().unpublish_track(&sid).await.ok();
                return format!("{:<30} {:>7}x{:<3}  no subscription (timeout)", case.name, w, h);
            }
        };

        // --- subscriber: read + measure ---
        let stats = measure(track, timing.clone(), epoch, w, h, secs).await;

        // Teardown, settle so the next run's subscription is unambiguous.
        stop.store(true, Ordering::SeqCst);
        let send_fps = push.join().unwrap_or(0.0);
        pub_room.local_participant().unpublish_track(&sid).await.ok();
        tokio::time::sleep(Duration::from_millis(800)).await;
        // Drain any late track events queued during teardown.
        while track_rx.try_recv().is_ok() {}

        let Stats { frames, secs_elapsed, mut latencies_ms, psnr_sum, psnr_n, recv_w, recv_h } = stats;
        if frames == 0 {
            return format!("{:<26} {:>7}x{:<3} {:>8.1} {:>8.1} {:>8} {:>8} {:>8} {:>8} {:>7}", case.name, w, h, send_fps, 0.0, "-", "-", "-", "-", 0);
        }
        let recv_fps = frames as f64 / secs_elapsed;
        // Received resolution — "=" if unchanged, else shows the downscale the
        // encoder applied to hold framerate (MAINTAIN_FRAMERATE trades res).
        let recv_res = if recv_w == w && recv_h == h { "=".to_string() } else { format!("{recv_w}x{recv_h}") };
        latencies_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let p = |q: f64| -> String {
            if latencies_ms.is_empty() { return "-".to_string(); }
            format!("{:.1}", latencies_ms[((latencies_ms.len() as f64 * q) as usize).min(latencies_ms.len() - 1)])
        };
        let psnr = if psnr_n > 0 { format!("{:.1}", psnr_sum / psnr_n as f64) } else { "-".to_string() };
        format!(
            "{:<26} {:>7}x{:<3} {:>8.1} {:>8.1} {:>8} {:>8} {:>8} {:>8} {:>7}",
            case.name, w, h, send_fps, recv_fps, recv_res, p(0.50), p(0.95), psnr, frames
        )
    }

    struct Stats {
        frames: u64,
        secs_elapsed: f64,
        latencies_ms: Vec<f64>,
        psnr_sum: f64,
        psnr_n: u64,
        // Last-seen received dims — reveals encoder downscaling (MAINTAIN_FRAMERATE
        // sheds resolution instead of fps, so recv res may be < source res).
        recv_w: u32,
        recv_h: u32,
    }

    /// Push frames at ~target fps until `stop`. Returns achieved send fps —
    /// if this equals recv fps, the ceiling is upstream of the wire (here or
    /// the encoder input), not the network/decoder.
    fn push_loop(
        source: NativeVideoSource,
        planes: Arc<Vec<Vec<u8>>>,
        w: u32,
        h: u32,
        target_fps: f64,
        timing: Arc<Timing>,
        stop: Arc<AtomicBool>,
    ) -> f64 {
        let frame_dur = Duration::from_secs_f64(1.0 / target_fps);
        // Reuse one I420 buffer; borrow it into the VideoFrame each push (the
        // C++ side takes its own ref, so overwrite-next-iteration is sound).
        let mut buf = I420Buffer::new(w, h);
        // Chroma is neutral and constant — set it ONCE, never per frame.
        {
            let (_ys, us, vs) = buf.strides();
            let (_y, u, v) = buf.data_mut();
            let cw = (w / 2) as usize;
            for row in 0..(h / 2) {
                u[(row * us) as usize..(row * us) as usize + cw].fill(128);
                v[(row * vs) as usize..(row * vs) as usize + cw].fill(128);
            }
        }
        let mut index: u32 = 0;
        let loop_start = Instant::now();
        while !stop.load(Ordering::Relaxed) {
            let t0 = Instant::now();
            fill_luma(&mut buf, &planes[(index % MOTION) as usize], w, h, index);
            timing.publish_at.lock()[index as usize & (RING - 1)] = Some(Instant::now());

            let frame = VideoFrame {
                rotation: VideoRotation::VideoRotation0,
                timestamp_us: index as i64 * (1_000_000 / target_fps as i64),
                frame_metadata: None,
                buffer: RefBuf(&buf),
            };
            source.capture_frame(&frame);
            drop(frame);

            index = index.wrapping_add(1);
            let el = t0.elapsed();
            if el < frame_dur {
                std::thread::sleep(frame_dur - el);
            }
        }
        index as f64 / loop_start.elapsed().as_secs_f64().max(1e-3)
    }

    /// Copy a precomputed luma plane into the buffer (respecting stride) and
    /// stamp the frame index. Chroma is untouched (set once by the caller).
    fn fill_luma(buf: &mut I420Buffer, plane: &[u8], w: u32, h: u32, index: u32) {
        let (ys, _us, _vs) = buf.strides();
        let (y, _u, _v) = buf.data_mut();
        for row in 0..h {
            let dst = (row * ys) as usize;
            let src = (row * w) as usize;
            y[dst..dst + w as usize].copy_from_slice(&plane[src..src + w as usize]);
        }
        // Stamp the index as high-contrast blocks over the pattern (top band).
        for bit in 0..INDEX_BITS {
            let on = (index >> bit) & 1 == 1;
            let val = if on { 235 } else { 16 };
            let x0 = bit * BLOCK_PITCH;
            let x1 = (x0 + BLOCK_W).min(w);
            for row in BLOCK_Y0..BLOCK_Y1.min(h) {
                let base = (row * ys) as usize;
                for col in x0..x1 {
                    y[base + col as usize] = val;
                }
            }
        }
    }

    async fn measure(track: RemoteVideoTrack, timing: Arc<Timing>, _epoch: u32, w: u32, h: u32, secs: u64) -> Stats {
        let mut stream = NativeVideoStream::new(track.rtc_track());
        use futures::StreamExt;

        let warmup = Duration::from_secs(2);
        let start = Instant::now();
        let mut counting_from: Option<Instant> = None;
        let mut frames: u64 = 0;
        let mut latencies_ms = Vec::new();
        let mut psnr_sum = 0.0;
        let mut psnr_n: u64 = 0;
        let mut recv_w = 0u32;
        let mut recv_h = 0u32;
        let deadline = warmup + Duration::from_secs(secs);

        while start.elapsed() < deadline {
            let next = tokio::time::timeout(Duration::from_millis(500), stream.next()).await;
            let frame = match next {
                Ok(Some(f)) => f,
                Ok(None) => break,
                Err(_) => continue,
            };
            let recv_at = Instant::now();
            // Warmup: let the encoder ramp before we count.
            if start.elapsed() < warmup {
                continue;
            }
            if counting_from.is_none() {
                counting_from = Some(recv_at);
            }
            frames += 1;
            recv_w = frame.buffer.width();
            recv_h = frame.buffer.height();

            // Latency/PSNR need the source pattern intact, so only score frames
            // still at full source res. Downscaled frames still count toward fps.
            if recv_w != w || recv_h != h {
                continue;
            }
            // to_i420() always yields I420 — HW decoders (NVDEC) hand back NV12,
            // where as_i420() would be None and drop every frame. We only read
            // the Y plane (chroma is neutral), and Y is identical across formats.
            let i420 = frame.buffer.to_i420();
            let (ys, _us, _vs) = i420.strides();
            let (yp, _u, _v) = i420.data();

            if let Some(index) = decode_index(yp, ys, w, h) {
                if let Some(pub_at) = timing.publish_at.lock()[index as usize & (RING - 1)] {
                    latencies_ms.push((recv_at - pub_at).as_secs_f64() * 1000.0);
                }
                let (mse, n) = luma_mse(yp, ys, w, h, index);
                if n > 0 {
                    let mse = (mse / n as f64).max(1e-6);
                    psnr_sum += 10.0 * (255.0f64 * 255.0 / mse).log10();
                    psnr_n += 1;
                }
            }
        }
        let secs_elapsed = counting_from.map(|c| c.elapsed().as_secs_f64()).unwrap_or(1.0).max(1e-3);
        Stats { frames, secs_elapsed, latencies_ms, psnr_sum, psnr_n, recv_w, recv_h }
    }

    /// Read the index stamp back from a decoded luma plane. Samples each block
    /// centre and thresholds — robust to compression blur.
    fn decode_index(y: &[u8], stride: u32, w: u32, h: u32) -> Option<u32> {
        let row = (BLOCK_Y0 + BLOCK_Y1) / 2;
        if row >= h {
            return None;
        }
        let base = (row * stride) as usize;
        let mut index = 0u32;
        for bit in 0..INDEX_BITS {
            let cx = bit * BLOCK_PITCH + BLOCK_W / 2;
            if cx >= w {
                return None;
            }
            if y[base + cx as usize] >= 128 {
                index |= 1 << bit;
            }
        }
        Some(index)
    }

    /// Sum of squared luma error vs the expected pattern, over the PSNR band
    /// (below the stamp). Returns (sum, count).
    fn luma_mse(y: &[u8], stride: u32, w: u32, h: u32, index: u32) -> (f64, u64) {
        if h <= PSNR_Y0 {
            return (0.0, 0);
        }
        let mut sum = 0.0;
        let mut n = 0u64;
        // Sample every 4th pixel — full-frame at 5120×1440 per frame is wasteful
        // and a 1/16 sample is statistically ample for a mean.
        let step = 4u32;
        let mut row = PSNR_Y0;
        while row < h {
            let base = (row * stride) as usize;
            let mut col = 0u32;
            while col < w {
                let got = y[base + col as usize] as f64;
                let want = expected_y(col, row, index) as f64;
                let d = got - want;
                sum += d * d;
                n += 1;
                col += step;
            }
            row += step;
        }
        (sum, n)
    }

    /// Borrow an `&I420Buffer` as a `VideoFrame` buffer without moving it, so the
    /// same buffer is reused across pushes (mirrors screen_capture.rs::RefBuf).
    struct RefBuf<'a>(&'a I420Buffer);
    impl AsRef<dyn VideoBuffer> for RefBuf<'_> {
        fn as_ref(&self) -> &(dyn VideoBuffer + 'static) {
            <I420Buffer as AsRef<dyn VideoBuffer>>::as_ref(self.0)
        }
    }
}

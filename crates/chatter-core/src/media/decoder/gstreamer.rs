//! GStreamer-based video decoder (Linux/Windows).
//!
//! Uses GStreamer's playbin for demuxing and decoding, with appsink
//! to receive decoded frames. This avoids FFmpeg symbol conflicts
//! with libwebrtc while providing full codec support.
//!
//! On Windows, GStreamer uses DirectShow/Media Foundation decoders.
//! On Linux, it uses VA-API/V4L2/software decoders.

use std::path::Path;
use std::sync::{Arc, Mutex, Once};

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;

use super::{DecodedAudio, DecodedFrame, VideoDecoder, VideoInfo};
use crate::error::MediaError;

/// Lazy GStreamer initialization - only called once when first needed.
static GST_INIT: Once = Once::new();
static mut GST_INIT_ERROR: Option<String> = None;

/// Initialize GStreamer lazily (only once, when first needed).
fn ensure_gst_init() -> Result<(), MediaError> {
    GST_INIT.call_once(|| {
        if let Err(e) = gst::init() {
            // SAFETY: Only written once during call_once, before any reads
            unsafe {
                GST_INIT_ERROR = Some(format!("GStreamer init failed: {}", e));
            }
        }
    });

    // SAFETY: Only read after call_once completes (which is guaranteed here)
    unsafe {
        if let Some(ref err) = GST_INIT_ERROR {
            return Err(MediaError::PlaybackError(err.clone()));
        }
    }

    Ok(())
}

/// GStreamer-based video decoder.
///
/// Decoder initialization is lazy - GStreamer is only initialized when
/// `open()` or `open_memory()` is called, not when the decoder is created.
///
/// Audio is handled by GStreamer's autoaudiosink, which outputs to the
/// system's default audio device (PulseAudio/PipeWire). This is simpler
/// and more reliable than pulling audio via AppSink and outputting via cpal.
pub struct GStreamerDecoder {
    pipeline: Option<gst::Pipeline>,
    video_sink: Option<gst_app::AppSink>,
    /// Playbin element (if using playbin3) for volume control
    playbin: Option<gst::Element>,
    /// Volume element in the dynamically linked memory-playback audio branch.
    memory_audio_volume: Arc<Mutex<Option<gst::Element>>>,
    info: Option<VideoInfo>,
    position_ms: Arc<Mutex<u64>>,
    /// AppSrc for memory-based playback (kept alive for the pipeline lifetime)
    _appsrc: Option<gst_app::AppSrc>,
}

impl GStreamerDecoder {
    /// Create a new GStreamer decoder.
    ///
    /// This is a lightweight operation - GStreamer is not initialized until
    /// `open()` or `open_memory()` is called.
    pub fn new() -> Result<Self, MediaError> {
        // Don't initialize GStreamer here - defer until open() is called
        Ok(Self {
            pipeline: None,
            video_sink: None,
            playbin: None,
            memory_audio_volume: Arc::new(Mutex::new(None)),
            info: None,
            position_ms: Arc::new(Mutex::new(0)),
            _appsrc: None,
        })
    }

    /// Build the GStreamer pipeline for a given URI.
    fn build_pipeline(&mut self, uri: &str) -> Result<VideoInfo, MediaError> {
        // Initialize GStreamer lazily (only once)
        ensure_gst_init()?;

        // Clean up any existing pipeline
        self.close();

        // Create pipeline elements
        let pipeline = gst::Pipeline::new();

        // Use playbin3 for automatic demuxing and decoding
        let playbin = gst::ElementFactory::make("playbin3")
            .property("uri", uri)
            .build()
            .map_err(|e| MediaError::PlaybackError(format!("Failed to create playbin: {}", e)))?;

        // Create video appsink with RGB conversion
        let video_sink = Self::create_video_sink()?;

        // Create bin to wrap video sink with converters
        let video_bin = Self::create_video_bin(&video_sink)?;

        // Set video sink on playbin - audio uses default autoaudiosink
        // which outputs to PulseAudio/PipeWire (simpler and more reliable)
        playbin.set_property("video-sink", &video_bin);
        // Note: We don't set audio-sink, so playbin3 uses autoaudiosink
        if crate::system::null_audio() {
            let fakesink = gst::ElementFactory::make("fakesink")
                .build()
                .map_err(|e| MediaError::PlaybackError(format!("Failed to create fakesink: {e}")))?;
            playbin.set_property("audio-sink", &fakesink);
        }

        pipeline.add(&playbin).map_err(|e| {
            MediaError::PlaybackError(format!("Failed to add playbin to pipeline: {}", e))
        })?;

        // Set to PAUSED to preroll and get stream info
        pipeline.set_state(gst::State::Paused).map_err(|e| {
            MediaError::PlaybackError(format!("Failed to set pipeline to paused: {}", e))
        })?;

        // Wait for state change with timeout
        let (result, _current, _pending) = pipeline.state(gst::ClockTime::from_seconds(10));
        if result.is_err() {
            pipeline.set_state(gst::State::Null).ok();
            return Err(MediaError::PlaybackError(
                "Pipeline failed to preroll".to_string(),
            ));
        }

        // Get video info from the sink caps
        let info = Self::extract_video_info(&pipeline, &video_sink)?;

        // Seek to position 0 to ensure video starts from the beginning.
        // Some containers (especially MP4 with moov atom at end) may have
        // incorrect initial position after preroll due to index/metadata issues.
        // SNAP_BEFORE ensures we land at or before the target position.
        if let Err(e) = pipeline.seek_simple(
            gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT | gst::SeekFlags::SNAP_BEFORE,
            gst::ClockTime::ZERO,
        ) {
            tracing::warn!("Initial seek to 0 failed: {:?}", e);
        }

        // Verify seek worked by querying actual position
        if let Some(pos) = pipeline.query_position::<gst::ClockTime>() {
            let pos_ms = pos.mseconds();
            // If position is still near the end, seek failed - retry with ACCURATE flag
            let threshold = info
                .duration_ms
                .saturating_sub(500)
                .max(info.duration_ms * 80 / 100);
            if pos_ms > threshold && info.duration_ms > 1000 {
                tracing::warn!(
                    "Initial seek to 0 failed, position is {}ms / {}ms - retrying with ACCURATE",
                    pos_ms,
                    info.duration_ms
                );
                // ACCURATE flag forces precise positioning (slower but reliable)
                if let Err(e) = pipeline.seek_simple(
                    gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE,
                    gst::ClockTime::ZERO,
                ) {
                    tracing::warn!("Accurate seek to 0 also failed: {:?}", e);
                }
            }
        }

        self.pipeline = Some(pipeline);
        self.video_sink = Some(video_sink);
        self.playbin = Some(playbin);
        self.info = Some(info.clone());
        self._appsrc = None; // Not used for URI-based playback

        Ok(info)
    }

    /// Build the GStreamer pipeline for in-memory data using appsrc.
    fn build_pipeline_from_memory(&mut self, data: Vec<u8>) -> Result<VideoInfo, MediaError> {
        // Initialize GStreamer lazily (only once)
        ensure_gst_init()?;

        // Clean up any existing pipeline
        self.close();

        let pipeline = gst::Pipeline::new();

        // Create appsrc for feeding data
        // Use RandomAccess for seekable playback - we keep data in memory
        // Don't set caps - let typefind detect the format automatically
        let data_len = data.len() as i64;
        let appsrc = gst_app::AppSrc::builder()
            .stream_type(gst_app::AppStreamType::RandomAccess)
            .size(data_len)
            .format(gst::Format::Bytes)
            .build();

        tracing::info!(
            "Memory pipeline: data_len={}, detected format: {:?}",
            data_len,
            Self::detect_video_caps(&data).map(|c| c.to_string())
        );

        // Store data for seeking - Arc allows sharing between callbacks
        let data_arc = Arc::new(data);
        let data_len = data_arc.len();

        // Track current read position for seek support
        let read_pos = Arc::new(Mutex::new(0usize));
        let read_pos_need = read_pos.clone();
        let read_pos_seek = read_pos.clone();

        let data_for_need = data_arc.clone();
        let data_for_seek = data_arc;

        // For RandomAccess mode with known size:
        // - GStreamer handles seeking via seek_data callback
        // - need_data provides data at current position
        // - DON'T send EOS after each push - GStreamer knows the size
        // - Only push requested amount (hint), not everything
        appsrc.set_callbacks(
            gst_app::AppSrcCallbacks::builder()
                .need_data(move |appsrc, hint| {
                    let mut pos = read_pos_need.lock().unwrap();
                    let data = data_for_need.as_ref();

                    if *pos >= data.len() {
                        // At end of data - signal EOS once
                        tracing::debug!("AppSrc: at end of data, signaling EOS");
                        let _ = appsrc.end_of_stream();
                        return;
                    }

                    // Push requested amount of data (or remaining, whichever is smaller)
                    let chunk_size = if hint > 0 {
                        std::cmp::min(hint as usize, data.len() - *pos)
                    } else {
                        // No hint - push a reasonable chunk (64KB)
                        std::cmp::min(65536, data.len() - *pos)
                    };

                    let slice = &data[*pos..*pos + chunk_size];
                    let offset = *pos as u64;

                    let mut buffer = gst::Buffer::from_slice(slice.to_vec());
                    {
                        let buffer_ref = buffer.get_mut().unwrap();
                        buffer_ref.set_offset(offset);
                        buffer_ref.set_offset_end(offset + chunk_size as u64);
                    }

                    tracing::trace!(
                        "AppSrc: pushing {} bytes from offset {}",
                        chunk_size,
                        offset
                    );

                    if let Err(e) = appsrc.push_buffer(buffer) {
                        tracing::error!("Failed to push buffer at offset {}: {:?}", *pos, e);
                        return;
                    }

                    *pos += chunk_size;
                })
                .seek_data(move |_appsrc, offset| {
                    let offset_usize = offset as usize;
                    let data = data_for_seek.as_ref();

                    if offset_usize > data.len() {
                        tracing::warn!(
                            "AppSrc: seek beyond data length {} > {}",
                            offset,
                            data.len()
                        );
                        return false;
                    }

                    tracing::trace!("AppSrc: seek to offset {}", offset);

                    // Update read position - next need_data will read from here
                    let mut pos = read_pos_seek.lock().unwrap();
                    *pos = offset_usize;

                    true
                })
                .build(),
        );

        tracing::info!("Created memory pipeline with {} bytes of data", data_len);

        // Create decodebin3 for automatic demuxing and decoding with hardware acceleration
        // decodebin3 is newer and better handles hardware decoders (VA-API, NVDEC, etc.)
        let decodebin = gst::ElementFactory::make("decodebin3")
            .build()
            .or_else(|_| {
                // Fall back to decodebin if decodebin3 not available
                tracing::warn!("decodebin3 not available, falling back to decodebin");
                gst::ElementFactory::make("decodebin").build()
            })
            .map_err(|e| MediaError::PlaybackError(format!("Failed to create decodebin: {}", e)))?;

        // Create video sink upfront - we need to store it for frame pulling
        // Audio uses autoaudiosink which GStreamer creates dynamically
        let video_sink = Self::create_video_sink()?;

        // IMPORTANT: Only add appsrc + decodebin upfront
        // Sink elements are added dynamically in pad-added to avoid preroll blocking
        pipeline
            .add_many([appsrc.upcast_ref(), &decodebin])
            .map_err(|e| MediaError::PlaybackError(format!("Failed to add elements: {}", e)))?;

        // Link appsrc -> decodebin
        appsrc.link(&decodebin).map_err(|e| {
            MediaError::PlaybackError(format!("Failed to link appsrc to decodebin: {}", e))
        })?;

        // Clone sink for use in pad-added callback
        let video_sink_clone = video_sink.clone();
        let pipeline_weak = pipeline.downgrade();
        let memory_audio_volume = self.memory_audio_volume.clone();

        // Track if video stream was found (for detecting audio-only files)
        let video_linked = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let video_linked_clone = video_linked.clone();

        // Connect decodebin pad-added for DYNAMIC element creation (per Context7 best practice)
        decodebin.connect_pad_added(move |_decodebin, src_pad| {
            let caps = src_pad
                .current_caps()
                .unwrap_or_else(|| src_pad.query_caps(None));
            let Some(structure) = caps.structure(0) else {
                tracing::warn!("Decodebin pad has no caps structure");
                return;
            };
            let name = structure.name();
            tracing::info!("Decodebin pad-added: {}", name);

            let Some(pipeline) = pipeline_weak.upgrade() else {
                tracing::warn!("Pipeline gone in pad-added callback");
                return;
            };

            // Create and add elements INSIDE pad-added (correct pattern)
            let insert_elements = || -> Result<(), String> {
                if name.starts_with("video/") {
                    // Create video processing elements dynamically
                    // Use larger queue (10 frames) to buffer decoded frames and absorb
                    // decode time variations, preventing stuttering
                    let queue = gst::ElementFactory::make("queue")
                        .property("max-size-buffers", 10u32)
                        .property("max-size-time", 500_000_000u64) // 500ms of video
                        .build()
                        .map_err(|e| format!("Failed to create video queue: {}", e))?;
                    let convert = gst::ElementFactory::make("videoconvert")
                        .build()
                        .map_err(|e| format!("Failed to create videoconvert: {}", e))?;
                    let scale = gst::ElementFactory::make("videoscale")
                        .build()
                        .map_err(|e| format!("Failed to create videoscale: {}", e))?;

                    // Add all elements to pipeline
                    let elements: &[&gst::Element] =
                        &[&queue, &convert, &scale, video_sink_clone.upcast_ref()];
                    pipeline
                        .add_many(elements)
                        .map_err(|e| format!("Failed to add video elements: {}", e))?;

                    // Link elements together
                    gst::Element::link_many(elements)
                        .map_err(|e| format!("Failed to link video elements: {}", e))?;

                    // Sync state with parent BEFORE linking to decodebin
                    for e in elements {
                        e.sync_state_with_parent()
                            .map_err(|e| format!("Failed to sync video element state: {}", e))?;
                    }

                    // Link decodebin's output to queue's input
                    let sink_pad = queue
                        .static_pad("sink")
                        .ok_or_else(|| "No sink pad on video queue".to_string())?;
                    src_pad
                        .link(&sink_pad)
                        .map_err(|e| format!("Failed to link video pad: {:?}", e))?;

                    // Mark that video track was found and linked
                    video_linked_clone.store(true, std::sync::atomic::Ordering::SeqCst);
                    tracing::info!("Video pipeline linked successfully");
                } else if name.starts_with("audio/") {
                    // Create audio processing elements dynamically
                    // Use autoaudiosink - GStreamer handles audio output to system default
                    let queue = gst::ElementFactory::make("queue")
                        .property("max-size-buffers", 200u32)
                        .property("max-size-time", 1_000_000_000u64) // 1 second of audio
                        .build()
                        .map_err(|e| format!("Failed to create audio queue: {}", e))?;
                    let convert = gst::ElementFactory::make("audioconvert")
                        .build()
                        .map_err(|e| format!("Failed to create audioconvert: {}", e))?;
                    let resample = gst::ElementFactory::make("audioresample")
                        .build()
                        .map_err(|e| format!("Failed to create audioresample: {}", e))?;
                    let volume = gst::ElementFactory::make("volume")
                        .build()
                        .map_err(|e| format!("Failed to create audio volume: {}", e))?;
                    // Use autoaudiosink - outputs to PulseAudio/PipeWire automatically
                    let sink = if crate::system::null_audio() {
                        "fakesink"
                    } else {
                        "autoaudiosink"
                    };
                    let audiosink = gst::ElementFactory::make(sink)
                        .build()
                        .map_err(|e| format!("Failed to create {sink}: {e}"))?;

                    // Add all elements to pipeline
                    let elements: &[&gst::Element] =
                        &[&queue, &convert, &resample, &volume, &audiosink];
                    pipeline
                        .add_many(elements)
                        .map_err(|e| format!("Failed to add audio elements: {}", e))?;

                    // Link elements together
                    gst::Element::link_many(elements)
                        .map_err(|e| format!("Failed to link audio elements: {}", e))?;

                    // Sync state with parent BEFORE linking to decodebin
                    for e in elements {
                        e.sync_state_with_parent()
                            .map_err(|e| format!("Failed to sync audio element state: {}", e))?;
                    }

                    // Link decodebin's output to queue's input
                    let sink_pad = queue
                        .static_pad("sink")
                        .ok_or_else(|| "No sink pad on audio queue".to_string())?;
                    src_pad
                        .link(&sink_pad)
                        .map_err(|e| format!("Failed to link audio pad: {:?}", e))?;

                    *memory_audio_volume.lock().unwrap() = Some(volume);

                    tracing::info!("Audio pipeline linked successfully (autoaudiosink)");
                }
                Ok(())
            };

            if let Err(e) = insert_elements() {
                tracing::error!("Failed to insert sink elements: {}", e);
            }
        });

        // Set to PAUSED to preroll - this triggers need-data callback
        tracing::debug!("Setting pipeline to PAUSED for preroll...");
        pipeline.set_state(gst::State::Paused).map_err(|e| {
            MediaError::PlaybackError(format!("Failed to set pipeline to paused: {}", e))
        })?;

        // Wait for state change with timeout - handle async completion
        let (result, current, pending) = pipeline.state(gst::ClockTime::from_seconds(10));
        tracing::info!(
            "Pipeline state change result: {:?}, current: {:?}, pending: {:?}",
            result,
            current,
            pending
        );

        // If state change is async, wait for it on the bus
        if result == Ok(gst::StateChangeSuccess::Async) {
            tracing::debug!("State change async, waiting for completion on bus...");
            if let Some(bus) = pipeline.bus() {
                // Wait for async-done or error, with longer timeout for slow decoding
                for msg in bus.iter_timed(gst::ClockTime::from_seconds(30)) {
                    match msg.view() {
                        gst::MessageView::AsyncDone(_) => {
                            tracing::info!("Pipeline async state change completed");
                            break;
                        }
                        gst::MessageView::Error(err) => {
                            let err_msg = format!(
                                "Pipeline error during preroll: {} (debug: {:?})",
                                err.error(),
                                err.debug()
                            );
                            tracing::error!("{}", err_msg);
                            pipeline.set_state(gst::State::Null).ok();
                            return Err(MediaError::PlaybackError(err_msg));
                        }
                        gst::MessageView::StateChanged(sc) => {
                            if sc
                                .src()
                                .map(|s| *s == pipeline.clone().upcast::<gst::Object>())
                                .unwrap_or(false)
                            {
                                tracing::debug!(
                                    "Pipeline state: {:?} -> {:?}",
                                    sc.old(),
                                    sc.current()
                                );
                                if sc.current() == gst::State::Paused {
                                    tracing::info!("Pipeline reached PAUSED state");
                                    break;
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        } else if result.is_err() {
            // Check for pipeline errors
            if let Some(bus) = pipeline.bus() {
                while let Some(msg) = bus.pop() {
                    match msg.view() {
                        gst::MessageView::Error(err) => {
                            let err_msg = format!(
                                "Pipeline error: {} (debug: {:?})",
                                err.error(),
                                err.debug()
                            );
                            tracing::error!("{}", err_msg);
                            pipeline.set_state(gst::State::Null).ok();
                            return Err(MediaError::PlaybackError(err_msg));
                        }
                        gst::MessageView::Warning(warn) => {
                            tracing::warn!(
                                "Pipeline warning: {} (debug: {:?})",
                                warn.error(),
                                warn.debug()
                            );
                        }
                        _ => {}
                    }
                }
            }
            pipeline.set_state(gst::State::Null).ok();
            return Err(MediaError::PlaybackError(
                "Pipeline failed to preroll".to_string(),
            ));
        }

        // Wait for video sink to receive caps - this indicates the video pipeline is connected.
        // The pad-added callback runs asynchronously, so we need to poll for caps.
        // This handles the race between state change completing and decodebin demuxing.
        let video_caps_timeout = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut has_video_caps = false;

        while std::time::Instant::now() < video_caps_timeout {
            if let Some(pad) = video_sink.static_pad("sink") {
                if pad.current_caps().is_some() {
                    has_video_caps = true;
                    tracing::info!("Video sink received caps - pipeline fully connected");
                    break;
                }
            }

            // Also check atomic flag in case pad-added already ran
            if video_linked.load(std::sync::atomic::Ordering::SeqCst) {
                // Pad-added ran, give it a moment for caps to propagate
                std::thread::sleep(std::time::Duration::from_millis(50));
                if let Some(pad) = video_sink.static_pad("sink") {
                    if pad.current_caps().is_some() {
                        has_video_caps = true;
                        break;
                    }
                }
            }

            // Check for pipeline errors while waiting
            if let Some(bus) = pipeline.bus() {
                while let Some(msg) = bus.pop() {
                    if let gst::MessageView::Error(err) = msg.view() {
                        let err_msg = format!(
                            "Pipeline error waiting for video: {} (debug: {:?})",
                            err.error(),
                            err.debug()
                        );
                        tracing::error!("{}", err_msg);
                        pipeline.set_state(gst::State::Null).ok();
                        return Err(MediaError::PlaybackError(err_msg));
                    }
                }
            }

            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        if !has_video_caps {
            pipeline.set_state(gst::State::Null).ok();
            return Err(MediaError::InvalidFormat(
                "No video track found - file may be audio-only or codec not supported".to_string(),
            ));
        }

        // Get video info from the sink caps
        tracing::debug!("Extracting video info from sink caps...");
        let info = Self::extract_video_info(&pipeline, &video_sink)?;
        tracing::info!(
            "Extracted video info: {}x{}, {}ms, {}fps",
            info.width,
            info.height,
            info.duration_ms,
            info.fps
        );

        // Seek to position 0 to ensure video starts from the beginning.
        // Some containers (especially MP4 with moov atom at end) may have
        // incorrect initial position after preroll due to index/metadata issues.
        // SNAP_BEFORE ensures we land at or before the target position.
        if let Err(e) = pipeline.seek_simple(
            gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT | gst::SeekFlags::SNAP_BEFORE,
            gst::ClockTime::ZERO,
        ) {
            tracing::warn!("Initial seek to 0 failed: {:?}", e);
        } else {
            tracing::debug!("Seeked to start position 0");
        }

        // Verify seek worked by querying actual position
        if let Some(pos) = pipeline.query_position::<gst::ClockTime>() {
            let pos_ms = pos.mseconds();
            // If position is still near the end, seek failed - retry with ACCURATE flag
            let threshold = info
                .duration_ms
                .saturating_sub(500)
                .max(info.duration_ms * 80 / 100);
            if pos_ms > threshold && info.duration_ms > 1000 {
                tracing::warn!(
                    "Initial seek to 0 failed, position is {}ms / {}ms - retrying with ACCURATE",
                    pos_ms,
                    info.duration_ms
                );
                // ACCURATE flag forces precise positioning (slower but reliable)
                if let Err(e) = pipeline.seek_simple(
                    gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE,
                    gst::ClockTime::ZERO,
                ) {
                    tracing::warn!("Accurate seek to 0 also failed: {:?}", e);
                }
            }
        }

        self.pipeline = Some(pipeline);
        self.video_sink = Some(video_sink);
        self.playbin = None; // Memory pipeline uses decodebin, not playbin
        self.info = Some(info.clone());
        self._appsrc = Some(appsrc); // Keep appsrc alive

        Ok(info)
    }

    /// Create video appsink configured for RGB output.
    fn create_video_sink() -> Result<gst_app::AppSink, MediaError> {
        let caps = gst::Caps::builder("video/x-raw")
            .field("format", "RGB")
            .build();

        // sync(true) - Let GStreamer handle frame timing via its internal clock.
        // This is the proper approach used by real video players like Glide.
        // With sync(true):
        // - try_pull_sample() blocks until the frame should be displayed
        // - No manual PTS timing or sleep needed in playback loop
        // - Audio and video stay synchronized automatically
        //
        // max_buffers(3) - small buffer, frames are pulled at display time
        // drop(false) - don't drop frames, GStreamer handles timing
        let sink = gst_app::AppSink::builder()
            .caps(&caps)
            .max_buffers(3)
            .drop(false)
            .sync(true)
            .build();

        Ok(sink)
    }

    /// Create a bin with videoconvert -> videoscale -> appsink.
    fn create_video_bin(sink: &gst_app::AppSink) -> Result<gst::Bin, MediaError> {
        let bin = gst::Bin::new();

        let convert = gst::ElementFactory::make("videoconvert")
            .build()
            .map_err(|e| {
                MediaError::PlaybackError(format!("Failed to create videoconvert: {}", e))
            })?;

        let scale = gst::ElementFactory::make("videoscale")
            .build()
            .map_err(|e| {
                MediaError::PlaybackError(format!("Failed to create videoscale: {}", e))
            })?;

        bin.add_many([&convert, &scale, sink.upcast_ref()])
            .map_err(|e| {
                MediaError::PlaybackError(format!("Failed to add elements to video bin: {}", e))
            })?;

        gst::Element::link_many([&convert, &scale, sink.upcast_ref()]).map_err(|e| {
            MediaError::PlaybackError(format!("Failed to link video elements: {}", e))
        })?;

        // Create ghost pad for the bin
        let pad = convert
            .static_pad("sink")
            .ok_or_else(|| MediaError::PlaybackError("No sink pad on videoconvert".to_string()))?;
        let ghost_pad = gst::GhostPad::with_target(&pad)
            .map_err(|e| MediaError::PlaybackError(format!("Failed to create ghost pad: {}", e)))?;
        ghost_pad.set_active(true).ok();
        bin.add_pad(&ghost_pad)
            .map_err(|e| MediaError::PlaybackError(format!("Failed to add ghost pad: {}", e)))?;

        Ok(bin)
    }

    /// Detect video container format from magic bytes.
    ///
    /// Returns GStreamer caps for the detected format to help decodebin.
    fn detect_video_caps(data: &[u8]) -> Option<gst::Caps> {
        if data.len() < 12 {
            return None;
        }

        // MP4/MOV: "ftyp" at offset 4
        if data.len() >= 8 && &data[4..8] == b"ftyp" {
            tracing::debug!("Detected MP4/QuickTime format");
            return Some(gst::Caps::builder("video/quicktime").build());
        }

        // MKV/WebM: EBML header (0x1A 0x45 0xDF 0xA3)
        if data.len() >= 4 && data[0..4] == [0x1A, 0x45, 0xDF, 0xA3] {
            // Check if it's WebM (has "webm" doctype) or MKV
            // For simplicity, use matroska which handles both
            tracing::debug!("Detected Matroska/WebM format");
            return Some(gst::Caps::builder("video/x-matroska").build());
        }

        // AVI: "RIFF" at start, "AVI " at offset 8
        if data.len() >= 12 && &data[0..4] == b"RIFF" && &data[8..12] == b"AVI " {
            tracing::debug!("Detected AVI format");
            return Some(gst::Caps::builder("video/x-msvideo").build());
        }

        // FLV: "FLV" at start
        if data.len() >= 3 && &data[0..3] == b"FLV" {
            tracing::debug!("Detected FLV format");
            return Some(gst::Caps::builder("video/x-flv").build());
        }

        // MPEG-TS: sync byte 0x47 repeated
        if data.len() >= 188 && data[0] == 0x47 && data[188 % data.len()] == 0x47 {
            tracing::debug!("Detected MPEG-TS format");
            return Some(gst::Caps::builder("video/mpegts").build());
        }

        // OGG: "OggS" at start
        if data.len() >= 4 && &data[0..4] == b"OggS" {
            tracing::debug!("Detected OGG format");
            return Some(gst::Caps::builder("application/ogg").build());
        }

        tracing::debug!("Unknown video format, letting decodebin auto-detect");
        None
    }

    /// Extract video information from the pipeline.
    fn extract_video_info(
        pipeline: &gst::Pipeline,
        video_sink: &gst_app::AppSink,
    ) -> Result<VideoInfo, MediaError> {
        // Query duration
        let duration_ms = pipeline
            .query_duration::<gst::ClockTime>()
            .map(|d| d.mseconds())
            .unwrap_or(0);
        tracing::info!("Queried duration: {}ms", duration_ms);

        // Get video caps from sink pad
        let pad = video_sink
            .static_pad("sink")
            .ok_or_else(|| MediaError::PlaybackError("No sink pad".to_string()))?;

        tracing::info!(
            "Sink pad linked: {}, has current caps: {}",
            pad.is_linked(),
            pad.current_caps().is_some()
        );

        let caps = pad.current_caps().ok_or_else(|| {
            // No caps means no video data reached the sink - likely audio-only file
            if pad.is_linked() {
                tracing::error!("Video sink linked but no caps - codec may not be supported");
                MediaError::InvalidFormat(
                    "Video codec not supported or file is corrupted".to_string(),
                )
            } else {
                tracing::error!("Video sink not linked - file may be audio-only");
                MediaError::InvalidFormat(
                    "No video track found - file may be audio-only".to_string(),
                )
            }
        })?;

        tracing::info!("Got caps: {}", caps);

        let structure = caps
            .structure(0)
            .ok_or_else(|| MediaError::PlaybackError("No structure in caps".to_string()))?;

        let width = structure
            .get::<i32>("width")
            .map_err(|_| MediaError::PlaybackError("No width in caps".to_string()))?
            as u32;

        let height = structure
            .get::<i32>("height")
            .map_err(|_| MediaError::PlaybackError("No height in caps".to_string()))?
            as u32;

        // Get framerate
        let fps = structure
            .get::<gst::Fraction>("framerate")
            .map(|f| f.numer() as f64 / f.denom() as f64)
            .unwrap_or(30.0);

        // Check for audio - query n-audio property from playbin
        // We'll assume has_audio is true for now, will be updated when we try to pull audio
        let has_audio = true;

        // Get codec info (best effort)
        let codec = "unknown".to_string();

        // Query if seeking is supported (important for live streams)
        let is_seekable = {
            let mut query = gst::query::Seeking::new(gst::Format::Time);
            if pipeline.query(&mut query) {
                let (seekable, _start, _end) = query.result();
                seekable
            } else {
                // If query fails, assume seekable if we have duration
                duration_ms > 0
            }
        };
        tracing::info!("Video is_seekable: {}", is_seekable);

        Ok(VideoInfo {
            width,
            height,
            duration_ms,
            fps,
            codec,
            has_audio,
            is_seekable,
        })
    }
}

impl VideoDecoder for GStreamerDecoder {
    fn open(&mut self, path: &Path) -> Result<VideoInfo, MediaError> {
        let uri = format!("file://{}", path.display());
        self.build_pipeline(&uri)
    }

    fn open_memory(&mut self, data: Vec<u8>) -> Result<VideoInfo, MediaError> {
        // Use appsrc-based pipeline for true memory-only playback
        self.build_pipeline_from_memory(data)
    }

    fn decode_video_frame(&mut self) -> Result<Option<DecodedFrame>, MediaError> {
        let video_sink = self
            .video_sink
            .as_ref()
            .ok_or_else(|| MediaError::PlaybackError("No video sink".to_string()))?;

        let pipeline = self
            .pipeline
            .as_ref()
            .ok_or_else(|| MediaError::PlaybackError("No pipeline".to_string()))?;

        // Check pipeline state to determine how to get frames:
        // - PAUSED: Use try_pull_preroll_sample() - gets the prerolled frame without clock
        // - PLAYING: Use try_pull_sample() - waits for clock timing (sync=true)
        let current_state = pipeline.current_state();

        let sample = if current_state == gst::State::Paused {
            // In PAUSED state, use preroll sample (for thumbnail extraction, seeking preview, etc.)
            // The preroll sample is the first frame decoded when entering PAUSED state.
            match video_sink.try_pull_preroll(gst::ClockTime::from_mseconds(500)) {
                Some(s) => s,
                None => {
                    if video_sink.is_eos() {
                        tracing::debug!("Video sink at EOS (paused)");
                        return Ok(None);
                    }
                    tracing::trace!("No preroll sample yet, will retry");
                    return Err(MediaError::PlaybackError("no_sample".to_string()));
                }
            }
        } else {
            // In PLAYING state, use try_pull_sample with sync(true) timing.
            // GStreamer's clock controls when frames are delivered.
            match video_sink.try_pull_sample(gst::ClockTime::from_mseconds(500)) {
                Some(s) => s,
                None => {
                    // Check if we're at EOS
                    if video_sink.is_eos() {
                        tracing::debug!("Video sink at EOS");
                        return Ok(None);
                    }
                    // No sample yet - return empty frame to signal "try again"
                    // Don't return Ok(None) as that signals end of stream!
                    tracing::trace!("No video sample yet, will retry");
                    return Err(MediaError::PlaybackError("no_sample".to_string()));
                }
            }
        };

        let buffer = sample
            .buffer()
            .ok_or_else(|| MediaError::PlaybackError("No buffer in sample".to_string()))?;

        let caps = sample
            .caps()
            .ok_or_else(|| MediaError::PlaybackError("No caps in sample".to_string()))?;

        let video_info = gst_video::VideoInfo::from_caps(caps)
            .map_err(|e| MediaError::PlaybackError(format!("Invalid video caps: {}", e)))?;

        let width = video_info.width();
        let height = video_info.height();

        // Map buffer for reading
        let map = buffer
            .map_readable()
            .map_err(|e| MediaError::PlaybackError(format!("Failed to map buffer: {}", e)))?;

        let data = map.as_slice().to_vec();

        // Use buffer PTS (Presentation Time Stamp) for the frame's actual timestamp.
        // This is the correct timestamp for this specific frame.
        // Pipeline position can lag behind or jump ahead during seeks.
        let timestamp_ms = buffer.pts().map(|pts| pts.mseconds()).unwrap_or_else(|| {
            // Fall back to pipeline position if buffer has no PTS
            pipeline
                .query_position::<gst::ClockTime>()
                .map(|pos| pos.mseconds())
                .unwrap_or(0)
        });

        // Update cached position
        *self.position_ms.lock().unwrap() = timestamp_ms;

        Ok(Some(DecodedFrame {
            data,
            width,
            height,
            timestamp_ms,
        }))
    }

    fn decode_audio(&mut self) -> Result<Option<DecodedAudio>, MediaError> {
        // Audio is handled by GStreamer's autoaudiosink directly.
        // We don't need to pull audio samples - GStreamer outputs to system audio.
        Ok(None)
    }

    fn seek(&mut self, timestamp_ms: u64) -> Result<(), MediaError> {
        let pipeline = self
            .pipeline
            .as_ref()
            .ok_or_else(|| MediaError::PlaybackError("No pipeline".to_string()))?;

        // Ensure pipeline is at least PAUSED before seeking - FLUSH seek requires this
        let current_state = pipeline.current_state();
        if current_state < gst::State::Paused {
            pipeline.set_state(gst::State::Paused).map_err(|e| {
                MediaError::PlaybackError(format!("Failed to pause for seek: {}", e))
            })?;
            // Wait for pause state to complete
            let (result, _, _) = pipeline.state(gst::ClockTime::from_seconds(5));
            if result.is_err() {
                tracing::warn!("Pipeline state change to PAUSED timed out before seek");
            }
        }

        let seek_pos = gst::ClockTime::from_mseconds(timestamp_ms);

        // Use FLUSH to clear buffers, KEY_UNIT to find nearest keyframe
        // For seeking to start (0), also use SNAP_BEFORE to ensure we go to the beginning
        let flags = if timestamp_ms == 0 {
            gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT | gst::SeekFlags::SNAP_BEFORE
        } else {
            gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT
        };

        pipeline
            .seek_simple(flags, seek_pos)
            .map_err(|e| MediaError::PlaybackError(format!("Seek failed: {}", e)))?;

        // Wait for seek to complete by waiting for ASYNC_DONE or state to settle
        // The seek causes an async state change internally
        let bus = pipeline
            .bus()
            .ok_or_else(|| MediaError::PlaybackError("No bus on pipeline".to_string()))?;

        // Wait for ASYNC_DONE message which indicates seek completed
        // Timeout after 2 seconds
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut seek_completed = false;
        while std::time::Instant::now() < deadline {
            if let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(100)) {
                use gst::MessageView;
                match msg.view() {
                    MessageView::AsyncDone(_) => {
                        tracing::trace!("Seek completed (ASYNC_DONE)");
                        seek_completed = true;
                        break;
                    }
                    MessageView::Error(err) => {
                        return Err(MediaError::PlaybackError(format!(
                            "Pipeline error during seek: {}",
                            err.error()
                        )));
                    }
                    MessageView::Eos(_) => {
                        tracing::debug!("EOS during seek - video may be very short");
                        seek_completed = true;
                        break;
                    }
                    _ => {}
                }
            }
        }

        // Give extra time for preroll to be generated after seek
        // Some codecs need additional time to decode the frame at the seek position
        if seek_completed && current_state == gst::State::Paused {
            // Brief wait for preroll sample to be ready
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        *self.position_ms.lock().unwrap() = timestamp_ms;

        Ok(())
    }

    fn position_ms(&self) -> u64 {
        *self.position_ms.lock().unwrap()
    }

    fn query_actual_position_ms(&self) -> Result<u64, MediaError> {
        let pipeline = self
            .pipeline
            .as_ref()
            .ok_or_else(|| MediaError::PlaybackError("No pipeline".to_string()))?;

        pipeline
            .query_position::<gst::ClockTime>()
            .map(|pos| pos.mseconds())
            .ok_or_else(|| MediaError::PlaybackError("Position query failed".to_string()))
    }

    fn duration_ms(&self) -> u64 {
        self.info.as_ref().map(|i| i.duration_ms).unwrap_or(0)
    }

    fn has_audio(&self) -> bool {
        self.info.as_ref().map(|i| i.has_audio).unwrap_or(false)
    }

    fn dimensions(&self) -> (u32, u32) {
        self.info
            .as_ref()
            .map(|i| (i.width, i.height))
            .unwrap_or((0, 0))
    }

    fn fps(&self) -> f64 {
        self.info.as_ref().map(|i| i.fps).unwrap_or(30.0)
    }

    fn is_seekable(&self) -> bool {
        self.info.as_ref().map(|i| i.is_seekable).unwrap_or(false)
    }

    fn play(&mut self) -> Result<(), MediaError> {
        let pipeline = self
            .pipeline
            .as_ref()
            .ok_or_else(|| MediaError::PlaybackError("No pipeline".to_string()))?;

        pipeline.set_state(gst::State::Playing).map_err(|e| {
            MediaError::PlaybackError(format!("Failed to set pipeline to playing: {}", e))
        })?;

        // Wait for state change
        let (result, _, _) = pipeline.state(gst::ClockTime::from_seconds(2));
        if result.is_err() {
            tracing::warn!("Pipeline state change to PLAYING timed out");
        }

        Ok(())
    }

    fn pause(&mut self) -> Result<(), MediaError> {
        let pipeline = self
            .pipeline
            .as_ref()
            .ok_or_else(|| MediaError::PlaybackError("No pipeline".to_string()))?;

        pipeline.set_state(gst::State::Paused).map_err(|e| {
            MediaError::PlaybackError(format!("Failed to set pipeline to paused: {}", e))
        })?;

        // Wait for state change
        let (result, _, _) = pipeline.state(gst::ClockTime::from_seconds(2));
        if result.is_err() {
            tracing::warn!("Pipeline state change to PAUSED timed out");
        }

        Ok(())
    }

    fn set_volume(&mut self, volume: f64) -> Result<(), MediaError> {
        // For playbin3, volume is a property on the playbin element
        if let Some(ref playbin) = self.playbin {
            let clamped = volume.clamp(0.0, 1.0);
            playbin.set_property("volume", clamped);
            tracing::debug!("Set volume to {:.2}", clamped);
            return Ok(());
        }

        let volume_element = self.memory_audio_volume.lock().unwrap();
        let volume_element = volume_element.as_ref().ok_or_else(|| {
            MediaError::PlaybackError("No audio volume element in memory pipeline".to_string())
        })?;
        volume_element.set_property("volume", volume.clamp(0.0, 1.0));
        Ok(())
    }

    fn close(&mut self) {
        if let Some(pipeline) = self.pipeline.take() {
            pipeline.set_state(gst::State::Null).ok();
        }
        self.video_sink = None;
        self.playbin = None;
        *self.memory_audio_volume.lock().unwrap() = None;
        self.info = None;
        self._appsrc = None;
    }
}

impl Drop for GStreamerDecoder {
    fn drop(&mut self) {
        self.close();
    }
}

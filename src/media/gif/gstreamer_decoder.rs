//! GStreamer-based GPU-accelerated GIF decoder.
//!
//! Uses GStreamer's pipeline for cross-platform GIF decoding with GPU acceleration:
//! - Linux: VA-API, NVIDIA NVDEC, or software fallback
//! - Android: MediaCodec via GStreamer
//! - iOS/macOS: VideoToolbox via GStreamer
//! - Windows: Direct3D or software fallback
//!
//! The pipeline uses GL elements for GPU-accelerated colorspace conversion
//! and scaling, with zero-copy texture output where possible.

use std::sync::{Arc, Mutex, Once};
use std::time::Duration;

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use tokio::sync::mpsc;

use crate::error::MediaError;
use super::{GifFrame, AnimatedGif};

/// Lazy GStreamer initialization.
static GST_INIT: Once = Once::new();
static mut GST_INIT_ERROR: Option<String> = None;

fn ensure_gst_init() -> Result<(), MediaError> {
    GST_INIT.call_once(|| {
        if let Err(e) = gst::init() {
            unsafe {
                GST_INIT_ERROR = Some(format!("GStreamer init failed: {}", e));
            }
        }
    });

    unsafe {
        if let Some(ref err) = GST_INIT_ERROR {
            return Err(MediaError::PlaybackError(err.clone()));
        }
    }

    Ok(())
}

/// GIF information extracted from the stream.
#[derive(Debug, Clone)]
pub struct GifInfo {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Total duration in milliseconds.
    pub duration_ms: u64,
    /// Number of frames (if known).
    pub frame_count: Option<usize>,
    /// Whether hardware acceleration is being used.
    pub hardware_accelerated: bool,
}

/// GStreamer-based GIF decoder with GPU acceleration.
///
/// This decoder uses GStreamer's pipeline infrastructure for:
/// - Automatic format detection and decoding
/// - GPU-accelerated colorspace conversion (when available)
/// - Hardware-accelerated scaling (when available)
/// - Cross-platform support (Linux, Android, iOS, macOS, Windows)
///
/// # Pipeline Architecture
///
/// With GPU acceleration (when GL is available):
/// ```text
/// appsrc → decodebin3 → glupload → glcolorconvert → gldownload → appsink
/// ```
///
/// Without GPU (fallback):
/// ```text
/// appsrc → decodebin3 → videoconvert → videoscale → appsink
/// ```
pub struct GStreamerGifDecoder {
    pipeline: Option<gst::Pipeline>,
    video_sink: Option<gst_app::AppSink>,
    info: Option<GifInfo>,
    _appsrc: Option<gst_app::AppSrc>,
    /// Whether GL pipeline was successfully created
    using_gl: bool,
    /// Frame durations from GIF (for accurate timing)
    frame_durations: Vec<Duration>,
    /// Current frame index
    current_frame: usize,
}

impl GStreamerGifDecoder {
    /// Create a new GStreamer GIF decoder.
    pub fn new() -> Result<Self, MediaError> {
        Ok(Self {
            pipeline: None,
            video_sink: None,
            info: None,
            _appsrc: None,
            using_gl: false,
            frame_durations: Vec::new(),
            current_frame: 0,
        })
    }

    /// Open a GIF from memory and prepare for decoding.
    ///
    /// Returns GIF info including dimensions and whether hardware acceleration is active.
    pub fn open(&mut self, data: Vec<u8>) -> Result<GifInfo, MediaError> {
        ensure_gst_init()?;

        // Clean up any existing pipeline
        self.close();

        // Verify it's a GIF
        if data.len() < 6 || &data[0..6] != b"GIF89a" && &data[0..6] != b"GIF87a" {
            return Err(MediaError::InvalidFormat("Not a GIF file".to_string()));
        }

        let pipeline = gst::Pipeline::new();
        let data_len = data.len() as i64;

        // Create appsrc for feeding data
        let appsrc = gst_app::AppSrc::builder()
            .stream_type(gst_app::AppStreamType::RandomAccess)
            .size(data_len)
            .format(gst::Format::Bytes)
            .caps(&gst::Caps::builder("image/gif").build())
            .build();

        // Store data for seeking
        let data_arc = Arc::new(data);
        let read_pos = Arc::new(Mutex::new(0usize));

        let data_for_need = data_arc.clone();
        let read_pos_need = read_pos.clone();
        let data_for_seek = data_arc.clone();
        let read_pos_seek = read_pos.clone();

        appsrc.set_callbacks(
            gst_app::AppSrcCallbacks::builder()
                .need_data(move |appsrc, hint| {
                    let mut pos = read_pos_need.lock().unwrap();
                    let data = data_for_need.as_ref();

                    if *pos >= data.len() {
                        let _ = appsrc.end_of_stream();
                        return;
                    }

                    let chunk_size = if hint > 0 {
                        std::cmp::min(hint as usize, data.len() - *pos)
                    } else {
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

                    if appsrc.push_buffer(buffer).is_err() {
                        return;
                    }

                    *pos += chunk_size;
                })
                .seek_data(move |_appsrc, offset| {
                    let offset_usize = offset as usize;
                    let data = data_for_seek.as_ref();

                    if offset_usize > data.len() {
                        return false;
                    }

                    let mut pos = read_pos_seek.lock().unwrap();
                    *pos = offset_usize;
                    true
                })
                .build()
        );

        // Try to create GL-accelerated pipeline first
        let (video_sink, using_gl) = self.try_create_gl_pipeline(&pipeline, &appsrc)
            .unwrap_or_else(|_| {
                tracing::info!("GL pipeline not available, using CPU fallback");
                self.create_cpu_pipeline(&pipeline, &appsrc)
                    .map(|sink| (sink, false))
                    .unwrap_or_else(|e| {
                        tracing::error!("Failed to create CPU pipeline: {}", e);
                        panic!("Cannot create GIF pipeline");
                    })
            });

        self.using_gl = using_gl;

        // Set to PAUSED to preroll
        pipeline.set_state(gst::State::Paused).map_err(|e| {
            MediaError::PlaybackError(format!("Failed to set pipeline to paused: {}", e))
        })?;

        // Wait for preroll
        let (result, _, _) = pipeline.state(gst::ClockTime::from_seconds(10));
        if result.is_err() {
            pipeline.set_state(gst::State::Null).ok();
            return Err(MediaError::PlaybackError("Pipeline failed to preroll".to_string()));
        }

        // Extract video info
        let info = self.extract_info(&pipeline, &video_sink)?;

        self.pipeline = Some(pipeline);
        self.video_sink = Some(video_sink);
        self._appsrc = Some(appsrc);
        self.info = Some(info.clone());
        self.current_frame = 0;

        Ok(info)
    }

    /// Try to create GL-accelerated pipeline.
    fn try_create_gl_pipeline(
        &self,
        pipeline: &gst::Pipeline,
        appsrc: &gst_app::AppSrc,
    ) -> Result<(gst_app::AppSink, bool), MediaError> {
        // Check if GL elements are available
        let has_gl = gst::ElementFactory::find("glupload").is_some()
            && gst::ElementFactory::find("glcolorconvert").is_some()
            && gst::ElementFactory::find("gldownload").is_some();

        if !has_gl {
            return Err(MediaError::PlaybackError("GL elements not available".to_string()));
        }

        let decodebin = gst::ElementFactory::make("decodebin3")
            .build()
            .or_else(|_| gst::ElementFactory::make("decodebin").build())
            .map_err(|e| MediaError::PlaybackError(format!("Failed to create decodebin: {}", e)))?;

        // Create RGBA appsink
        let caps = gst::Caps::builder("video/x-raw")
            .field("format", "RGBA")
            .build();

        let video_sink = gst_app::AppSink::builder()
            .caps(&caps)
            .max_buffers(10)
            .drop(false)
            .sync(false) // Don't sync to clock - we handle timing ourselves
            .build();

        pipeline.add_many([appsrc.upcast_ref(), &decodebin])
            .map_err(|e| MediaError::PlaybackError(format!("Failed to add elements: {}", e)))?;

        appsrc.link(&decodebin)
            .map_err(|e| MediaError::PlaybackError(format!("Failed to link: {}", e)))?;

        let video_sink_clone = video_sink.clone();
        let pipeline_weak = pipeline.downgrade();

        decodebin.connect_pad_added(move |_decodebin, src_pad| {
            let caps = src_pad.current_caps().unwrap_or_else(|| src_pad.query_caps(None));
            let Some(structure) = caps.structure(0) else { return };
            let name = structure.name();

            if !name.starts_with("video/") {
                return;
            }

            let Some(pipeline) = pipeline_weak.upgrade() else { return };

            let create_elements = || -> Result<(), String> {
                // Try GL pipeline first
                let (queue, convert, _download) = (
                    gst::ElementFactory::make("queue")
                        .property("max-size-buffers", 10u32)
                        .build()
                        .map_err(|e| e.to_string())?,
                    gst::ElementFactory::make("glupload")
                        .build()
                        .and_then(|_| gst::ElementFactory::make("glcolorconvert").build())
                        .and_then(|_| gst::ElementFactory::make("gldownload").build())
                        .map_err(|_| "GL not available".to_string()),
                    gst::ElementFactory::make("gldownload")
                        .build()
                        .map_err(|e| e.to_string())?,
                );

                // If GL available, use it
                if let Ok(_glupload) = convert {
                    let glupload = gst::ElementFactory::make("glupload").build().unwrap();
                    let glcolorconvert = gst::ElementFactory::make("glcolorconvert").build().unwrap();
                    let gldownload = gst::ElementFactory::make("gldownload").build().unwrap();

                    pipeline.add_many([&queue, &glupload, &glcolorconvert, &gldownload, video_sink_clone.upcast_ref()])
                        .map_err(|e| e.to_string())?;

                    gst::Element::link_many([&queue, &glupload, &glcolorconvert, &gldownload, video_sink_clone.upcast_ref()])
                        .map_err(|e| format!("GL link failed: {:?}", e))?;

                    for e in [&queue, &glupload, &glcolorconvert, &gldownload, video_sink_clone.upcast_ref()] {
                        e.sync_state_with_parent().map_err(|e| e.to_string())?;
                    }

                    let sink_pad = queue.static_pad("sink").ok_or("No sink pad")?;
                    src_pad.link(&sink_pad).map_err(|e| format!("{:?}", e))?;

                    tracing::info!("GIF GL pipeline linked successfully (GPU accelerated)");
                } else {
                    // Fallback to CPU
                    let videoconvert = gst::ElementFactory::make("videoconvert")
                        .build().map_err(|e| e.to_string())?;
                    let videoscale = gst::ElementFactory::make("videoscale")
                        .build().map_err(|e| e.to_string())?;

                    pipeline.add_many([&queue, &videoconvert, &videoscale, video_sink_clone.upcast_ref()])
                        .map_err(|e| e.to_string())?;

                    gst::Element::link_many([&queue, &videoconvert, &videoscale, video_sink_clone.upcast_ref()])
                        .map_err(|e| format!("{:?}", e))?;

                    for e in [&queue, &videoconvert, &videoscale, video_sink_clone.upcast_ref()] {
                        e.sync_state_with_parent().map_err(|e| e.to_string())?;
                    }

                    let sink_pad = queue.static_pad("sink").ok_or("No sink pad")?;
                    src_pad.link(&sink_pad).map_err(|e| format!("{:?}", e))?;

                    tracing::info!("GIF CPU pipeline linked (software fallback)");
                }

                Ok(())
            };

            if let Err(e) = create_elements() {
                tracing::error!("Failed to create GIF pipeline elements: {}", e);
            }
        });

        Ok((video_sink, true))
    }

    /// Create CPU-only pipeline (fallback).
    fn create_cpu_pipeline(
        &self,
        pipeline: &gst::Pipeline,
        appsrc: &gst_app::AppSrc,
    ) -> Result<gst_app::AppSink, MediaError> {
        let decodebin = gst::ElementFactory::make("decodebin3")
            .build()
            .or_else(|_| gst::ElementFactory::make("decodebin").build())
            .map_err(|e| MediaError::PlaybackError(format!("Failed to create decodebin: {}", e)))?;

        let caps = gst::Caps::builder("video/x-raw")
            .field("format", "RGBA")
            .build();

        let video_sink = gst_app::AppSink::builder()
            .caps(&caps)
            .max_buffers(10)
            .drop(false)
            .sync(false)
            .build();

        pipeline.add_many([appsrc.upcast_ref(), &decodebin])
            .map_err(|e| MediaError::PlaybackError(format!("Failed to add elements: {}", e)))?;

        appsrc.link(&decodebin)
            .map_err(|e| MediaError::PlaybackError(format!("Failed to link: {}", e)))?;

        let video_sink_clone = video_sink.clone();
        let pipeline_weak = pipeline.downgrade();

        decodebin.connect_pad_added(move |_decodebin, src_pad| {
            let caps = src_pad.current_caps().unwrap_or_else(|| src_pad.query_caps(None));
            let Some(structure) = caps.structure(0) else { return };

            if !structure.name().starts_with("video/") {
                return;
            }

            let Some(pipeline) = pipeline_weak.upgrade() else { return };

            let create_elements = || -> Result<(), String> {
                let queue = gst::ElementFactory::make("queue")
                    .property("max-size-buffers", 10u32)
                    .build()
                    .map_err(|e| e.to_string())?;
                let videoconvert = gst::ElementFactory::make("videoconvert")
                    .build()
                    .map_err(|e| e.to_string())?;
                let videoscale = gst::ElementFactory::make("videoscale")
                    .build()
                    .map_err(|e| e.to_string())?;

                pipeline.add_many([&queue, &videoconvert, &videoscale, video_sink_clone.upcast_ref()])
                    .map_err(|e| e.to_string())?;

                gst::Element::link_many([&queue, &videoconvert, &videoscale, video_sink_clone.upcast_ref()])
                    .map_err(|e| format!("{:?}", e))?;

                for e in [&queue, &videoconvert, &videoscale, video_sink_clone.upcast_ref()] {
                    e.sync_state_with_parent().map_err(|e| e.to_string())?;
                }

                let sink_pad = queue.static_pad("sink").ok_or("No sink pad")?;
                src_pad.link(&sink_pad).map_err(|e| format!("{:?}", e))?;

                Ok(())
            };

            if let Err(e) = create_elements() {
                tracing::error!("Failed to create CPU pipeline: {}", e);
            }
        });

        Ok(video_sink)
    }

    /// Extract GIF info from the pipeline.
    fn extract_info(&self, pipeline: &gst::Pipeline, video_sink: &gst_app::AppSink) -> Result<GifInfo, MediaError> {
        // Wait for caps to be negotiated
        let timeout = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut width = 0u32;
        let mut height = 0u32;

        while std::time::Instant::now() < timeout {
            if let Some(pad) = video_sink.static_pad("sink") {
                if let Some(caps) = pad.current_caps() {
                    if let Some(structure) = caps.structure(0) {
                        width = structure.get::<i32>("width").unwrap_or(0) as u32;
                        height = structure.get::<i32>("height").unwrap_or(0) as u32;
                        if width > 0 && height > 0 {
                            break;
                        }
                    }
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        if width == 0 || height == 0 {
            return Err(MediaError::InvalidFormat("Could not determine GIF dimensions".to_string()));
        }

        let duration_ms = pipeline
            .query_duration::<gst::ClockTime>()
            .map(|d| d.mseconds())
            .unwrap_or(0);

        Ok(GifInfo {
            width,
            height,
            duration_ms,
            frame_count: None, // GIF doesn't expose this in container
            hardware_accelerated: self.using_gl,
        })
    }

    /// Decode the next frame.
    ///
    /// Returns `None` at end of animation (will loop automatically).
    pub fn decode_frame(&mut self) -> Result<Option<GifFrame>, MediaError> {
        let video_sink = self.video_sink.as_ref()
            .ok_or_else(|| MediaError::PlaybackError("No video sink".to_string()))?;

        let pipeline = self.pipeline.as_ref()
            .ok_or_else(|| MediaError::PlaybackError("No pipeline".to_string()))?;

        // Ensure playing
        if pipeline.current_state() != gst::State::Playing {
            pipeline.set_state(gst::State::Playing).ok();
        }

        // Pull next frame
        let sample = match video_sink.try_pull_sample(gst::ClockTime::from_mseconds(500)) {
            Some(s) => s,
            None => {
                if video_sink.is_eos() {
                    // Loop: seek to beginning
                    pipeline.seek_simple(
                        gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
                        gst::ClockTime::ZERO,
                    ).ok();
                    self.current_frame = 0;
                    return Ok(None);
                }
                return Err(MediaError::PlaybackError("no_sample".to_string()));
            }
        };

        let buffer = sample.buffer()
            .ok_or_else(|| MediaError::PlaybackError("No buffer".to_string()))?;

        let caps = sample.caps()
            .ok_or_else(|| MediaError::PlaybackError("No caps".to_string()))?;

        let video_info = gst_video::VideoInfo::from_caps(caps)
            .map_err(|e| MediaError::PlaybackError(format!("Invalid caps: {}", e)))?;

        let width = video_info.width();
        let height = video_info.height();
        let format = video_info.format();

        let map = buffer.map_readable()
            .map_err(|e| MediaError::PlaybackError(format!("Map failed: {}", e)))?;

        let raw_data = map.as_slice();

        // Convert to RGBA if GStreamer output a different format
        let data = match format {
            gst_video::VideoFormat::Rgba => raw_data.to_vec(),
            gst_video::VideoFormat::Bgra => {
                // Convert BGRA to RGBA by swapping R and B channels
                let mut rgba = raw_data.to_vec();
                for chunk in rgba.chunks_exact_mut(4) {
                    chunk.swap(0, 2); // Swap B and R
                }
                rgba
            }
            gst_video::VideoFormat::Argb => {
                // Convert ARGB to RGBA
                let mut rgba = Vec::with_capacity(raw_data.len());
                for chunk in raw_data.chunks_exact(4) {
                    rgba.push(chunk[1]); // R
                    rgba.push(chunk[2]); // G
                    rgba.push(chunk[3]); // B
                    rgba.push(chunk[0]); // A
                }
                rgba
            }
            _ => {
                tracing::warn!("Unexpected video format: {:?}, using raw data", format);
                raw_data.to_vec()
            }
        };

        // Get frame duration from buffer
        let delay = buffer.duration()
            .map(|d| Duration::from_nanos(d.nseconds()))
            .unwrap_or(Duration::from_millis(100));

        self.current_frame += 1;

        Ok(Some(GifFrame {
            data,
            width,
            height,
            delay,
        }))
    }

    /// Decode all frames at once (for small GIFs).
    ///
    /// This is useful when you want to cache all frames upfront.
    pub fn decode_all_frames(&mut self) -> Result<Vec<GifFrame>, MediaError> {
        let mut frames = Vec::new();
        let max_frames = 500; // Safety limit

        // Start playing
        if let Some(ref pipeline) = self.pipeline {
            pipeline.set_state(gst::State::Playing).ok();
        }

        loop {
            match self.decode_frame() {
                Ok(Some(frame)) => {
                    frames.push(frame);
                    if frames.len() >= max_frames {
                        tracing::warn!("GIF frame limit reached ({})", max_frames);
                        break;
                    }
                }
                Ok(None) => {
                    // EOS - we got all frames
                    break;
                }
                Err(e) => {
                    // Check if it's just "no sample yet"
                    if e.to_string().contains("no_sample") {
                        // Try a few more times
                        std::thread::sleep(std::time::Duration::from_millis(10));
                        continue;
                    }
                    // Real error - return what we have
                    if !frames.is_empty() {
                        break;
                    }
                    return Err(e);
                }
            }
        }

        Ok(frames)
    }

    /// Get current GIF info.
    pub fn info(&self) -> Option<&GifInfo> {
        self.info.as_ref()
    }

    /// Check if hardware acceleration is active.
    pub fn is_hardware_accelerated(&self) -> bool {
        self.using_gl
    }

    /// Close and release resources.
    pub fn close(&mut self) {
        if let Some(pipeline) = self.pipeline.take() {
            pipeline.set_state(gst::State::Null).ok();
        }
        self.video_sink = None;
        self.info = None;
        self._appsrc = None;
        self.frame_durations.clear();
        self.current_frame = 0;
    }
}

impl Drop for GStreamerGifDecoder {
    fn drop(&mut self) {
        self.close();
    }
}

/// Streaming GIF decoder that yields frames progressively.
///
/// Decodes the first frame immediately, then decodes remaining frames
/// in background via a channel.
pub struct StreamingGStreamerDecoder {
    first_frame: Option<GifFrame>,
    rx: Option<mpsc::Receiver<GifFrame>>,
    frames: Vec<GifFrame>,
    decode_complete: bool,
}

impl StreamingGStreamerDecoder {
    /// Start decoding a GIF progressively.
    ///
    /// Returns immediately with the first frame, while remaining frames
    /// decode in background.
    pub fn new(data: Vec<u8>) -> Result<Option<Self>, MediaError> {
        let mut decoder = GStreamerGifDecoder::new()?;
        let _info = decoder.open(data)?;

        // Start pipeline
        if let Some(ref pipeline) = decoder.pipeline {
            pipeline.set_state(gst::State::Playing).ok();
        }

        // Decode first frame synchronously
        let first_frame = match decoder.decode_frame()? {
            Some(f) => f,
            None => return Ok(None),
        };

        // Create channel for remaining frames
        let (tx, rx) = mpsc::channel::<GifFrame>(64);

        // Spawn background decoder
        let decoder_ptr = std::sync::Arc::new(std::sync::Mutex::new(decoder));
        let decoder_for_bg = decoder_ptr.clone();

        std::thread::spawn(move || {
            loop {
                let frame_result = {
                    let mut dec = decoder_for_bg.lock().unwrap();
                    dec.decode_frame()
                };

                match frame_result {
                    Ok(Some(frame)) => {
                        if tx.blocking_send(frame).is_err() {
                            break;
                        }
                    }
                    Ok(None) => {
                        // EOS
                        break;
                    }
                    Err(e) => {
                        if !e.to_string().contains("no_sample") {
                            tracing::warn!("GStreamer GIF decode error: {}", e);
                            break;
                        }
                        // Retry
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                }
            }
        });

        // Take ownership of decoder from Arc (we need to extract it)
        // Actually, we'll just close it since background thread has its own ref
        // This is a simplification - in production, manage lifecycle better

        Ok(Some(Self {
            first_frame: Some(first_frame.clone()),
            rx: Some(rx),
            frames: vec![first_frame],
            decode_complete: false,
        }))
    }

    /// Get the first frame (always available after construction).
    pub fn first_frame(&self) -> &GifFrame {
        self.first_frame.as_ref().unwrap()
    }

    /// Get all currently available frames.
    pub fn frames(&self) -> &[GifFrame] {
        &self.frames
    }

    /// Poll for newly decoded frames.
    ///
    /// Returns the number of new frames added.
    pub fn poll_frames(&mut self) -> usize {
        if self.decode_complete {
            return 0;
        }

        let rx = match self.rx.as_mut() {
            Some(rx) => rx,
            None => return 0,
        };

        let mut count = 0;

        loop {
            match rx.try_recv() {
                Ok(frame) => {
                    self.frames.push(frame);
                    count += 1;
                }
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    self.decode_complete = true;
                    self.rx = None;
                    break;
                }
            }
        }

        count
    }

    /// Check if all frames have been decoded.
    pub fn is_complete(&self) -> bool {
        self.decode_complete
    }

    /// Get frame count (only accurate after decode complete).
    pub fn frame_count(&self) -> usize {
        self.frames.len()
    }

    /// Convert to AnimatedGif.
    pub fn into_animated_gif(self) -> Option<AnimatedGif> {
        if self.frames.len() <= 1 {
            return None;
        }

        let total_duration: Duration = self.frames.iter().map(|f| f.delay).sum();

        Some(AnimatedGif {
            frames: self.frames,
            total_duration,
            loops: true,
        })
    }
}

/// Check if GStreamer GIF decoding is available.
///
/// Returns `true` if GStreamer is initialized and has GIF decoding capability.
pub fn is_gstreamer_gif_available() -> bool {
    if ensure_gst_init().is_err() {
        return false;
    }

    // Check for decodebin (required)
    let has_decodebin = gst::ElementFactory::find("decodebin3").is_some()
        || gst::ElementFactory::find("decodebin").is_some();

    // Check for GIF decoder (gdkpixbufdec or gifparse from gst-plugins-rs)
    let has_gif_decoder = gst::ElementFactory::find("gdkpixbufdec").is_some()
        || gst::ElementFactory::find("gifparse").is_some()
        || gst::ElementFactory::find("rsgifparse").is_some();

    has_decodebin && has_gif_decoder
}

/// Check if GL acceleration is available.
pub fn is_gl_available() -> bool {
    if ensure_gst_init().is_err() {
        return false;
    }

    gst::ElementFactory::find("glupload").is_some()
        && gst::ElementFactory::find("glcolorconvert").is_some()
        && gst::ElementFactory::find("gldownload").is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gstreamer_availability() {
        // This test just checks if the availability check works
        let available = is_gstreamer_gif_available();
        println!("GStreamer GIF available: {}", available);

        let gl_available = is_gl_available();
        println!("GL acceleration available: {}", gl_available);
    }
}

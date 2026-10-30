//! Video processor for thumbnail extraction.
//!
//! Uses GStreamer with hardware acceleration (VA-API/NVDEC) when available.
//! Based on the official gstreamer-rs thumbnail example.

use crate::error::MediaError;
use image::ImageEncoder;
use std::path::Path;
use std::sync::{Arc, Mutex, Once};

use gst_video::prelude::*;
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;

/// Maximum thumbnail dimension (width or height).
const THUMBNAIL_MAX_SIZE: u32 = 400;

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

/// Extract a thumbnail from a video file.
///
/// Uses hardware-accelerated decoding (VA-API/NVDEC) when available.
/// Seeks to 1 second (or first frame if video is shorter).
pub fn extract_video_thumbnail(source: &Path) -> Result<(Vec<u8>, u32, u32, u64), MediaError> {
    ensure_gst_init()?;

    let uri = format!(
        "file://{}",
        source
            .canonicalize()
            .map_err(MediaError::FileRead)?
            .display()
    );

    extract_thumbnail_from_uri(&uri, 1) // Seek to 1 second
}

/// Extract a thumbnail from video data in memory.
pub fn extract_video_thumbnail_from_memory(
    data: &[u8],
) -> Result<(Vec<u8>, u32, u32, u64), MediaError> {
    if data.is_empty() {
        return Err(MediaError::InvalidFormat("empty video data".to_string()));
    }

    ensure_gst_init()?;
    extract_thumbnail_from_memory_impl(data, 1)
}

/// Extract thumbnail using uridecodebin (auto-selects HW decoder).
///
/// Pipeline: uridecodebin ! videoconvert ! appsink
/// - uridecodebin auto-plugs VA-API/NVDEC decoders when available
/// - videoconvert handles GPU→CPU memory transfer automatically
#[allow(clippy::type_complexity)]
fn extract_thumbnail_from_uri(
    uri: &str,
    seek_seconds: u64,
) -> Result<(Vec<u8>, u32, u32, u64), MediaError> {
    // Build pipeline string - uridecodebin handles HW decoder selection
    // videoconvert will use VA-API postproc or gldownload automatically when needed
    let pipeline_str = format!(
        "uridecodebin uri=\"{}\" ! videoconvert ! appsink name=sink",
        uri.replace('"', "\\\"")
    );

    let pipeline = gst::parse::launch(&pipeline_str)
        .map_err(|e| MediaError::PlaybackError(format!("Pipeline parse error: {}", e)))?
        .downcast::<gst::Pipeline>()
        .map_err(|_| MediaError::PlaybackError("Not a pipeline".to_string()))?;

    // Get appsink and configure it
    let appsink = pipeline
        .by_name("sink")
        .ok_or_else(|| MediaError::PlaybackError("No sink element".to_string()))?
        .downcast::<gst_app::AppSink>()
        .map_err(|_| MediaError::PlaybackError("Not an appsink".to_string()))?;

    // KEY: sync=false for immediate frame delivery (no clock waiting)
    appsink.set_property("sync", false);

    // Request RGBx format (4 bytes per pixel, handles stride properly)
    appsink.set_caps(Some(
        &gst_video::VideoCapsBuilder::new()
            .format(gst_video::VideoFormat::Rgbx)
            .build(),
    ));

    // Storage for captured frame
    let frame_data: Arc<Mutex<Option<(Vec<u8>, u32, u32)>>> = Arc::new(Mutex::new(None));
    let frame_data_clone = frame_data.clone();
    let got_frame = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let got_frame_clone = got_frame.clone();

    // Callback to capture frame
    appsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |appsink| {
                // Only capture one frame
                if got_frame_clone.swap(true, std::sync::atomic::Ordering::SeqCst) {
                    return Err(gst::FlowError::Eos);
                }

                let sample = appsink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                let caps = sample.caps().ok_or(gst::FlowError::Error)?;
                let info =
                    gst_video::VideoInfo::from_caps(caps).map_err(|_| gst::FlowError::Error)?;

                // Use VideoFrameRef for proper stride handling
                let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info)
                    .map_err(|_| gst::FlowError::Error)?;

                let width = frame.width();
                let height = frame.height();

                // Use image crate's FlatSamples to handle stride correctly
                // This is the same approach as the official example
                let plane_data = frame.plane_data(0).map_err(|_| gst::FlowError::Error)?;
                let stride = frame.plane_stride()[0] as usize;

                // Convert RGBx (4 bytes, with stride) to packed RGB (3 bytes)
                let mut rgb_data = Vec::with_capacity((width * height * 3) as usize);
                for y in 0..height as usize {
                    for x in 0..width as usize {
                        let offset = y * stride + x * 4;
                        rgb_data.push(plane_data[offset]); // R
                        rgb_data.push(plane_data[offset + 1]); // G
                        rgb_data.push(plane_data[offset + 2]); // B
                    }
                }

                *frame_data_clone.lock().unwrap() = Some((rgb_data, width, height));

                Err(gst::FlowError::Eos)
            })
            .build(),
    );

    // Start pipeline in PAUSED for preroll
    pipeline
        .set_state(gst::State::Paused)
        .map_err(|e| MediaError::PlaybackError(format!("Failed to pause: {}", e)))?;

    let bus = pipeline
        .bus()
        .ok_or_else(|| MediaError::PlaybackError("No bus".to_string()))?;

    let mut state = 0u8; // 0=preroll, 1=seeked, 2=playing
    let mut duration_ms = 0u64;
    let timeout = std::time::Instant::now() + std::time::Duration::from_secs(15);

    // Event loop: preroll -> seek -> wait for seek complete -> play -> capture
    while std::time::Instant::now() < timeout {
        if let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(100)) {
            match msg.view() {
                gst::MessageView::AsyncDone(_) => {
                    match state {
                        0 => {
                            // First AsyncDone = pipeline prerolled, now seek
                            duration_ms = pipeline
                                .query_duration::<gst::ClockTime>()
                                .map(|d| d.mseconds())
                                .unwrap_or(0);

                            // Seek to 10% of video or seek_seconds, whichever is larger
                            let seek_pos = if duration_ms > 0 {
                                let ten_percent = duration_ms / 10;
                                let target = (seek_seconds * 1000).max(ten_percent);
                                target.min(duration_ms - 1000) // Don't seek past end
                            } else {
                                0
                            };

                            tracing::debug!(
                                "Seeking to {}ms (duration {}ms)",
                                seek_pos,
                                duration_ms
                            );

                            if seek_pos > 0 {
                                // Seek with FLUSH + KEY_UNIT for faster seeking
                                if let Err(e) = pipeline.seek_simple(
                                    gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
                                    gst::ClockTime::from_mseconds(seek_pos),
                                ) {
                                    tracing::warn!("Pipeline seek failed: {}", e);
                                }
                                state = 1;
                            } else {
                                // No seek needed, start playing immediately
                                if let Err(e) = pipeline.set_state(gst::State::Playing) {
                                    tracing::warn!("Pipeline set_state(Playing) failed: {:?}", e);
                                }
                                state = 2;
                            }
                        }
                        1 => {
                            // Second AsyncDone = seek completed, now play
                            tracing::debug!("Seek completed, starting playback");
                            if let Err(e) = pipeline.set_state(gst::State::Playing) {
                                tracing::warn!("Pipeline set_state(Playing) failed: {:?}", e);
                            }
                            state = 2;
                        }
                        _ => {}
                    }
                }
                gst::MessageView::Eos(_) => {
                    break;
                }
                gst::MessageView::Error(err) => {
                    let _ = pipeline.set_state(gst::State::Null);
                    return Err(MediaError::PlaybackError(format!(
                        "Pipeline error: {} ({:?})",
                        err.error(),
                        err.debug()
                    )));
                }
                _ => {}
            }
        }

        // Check if we got a frame
        if got_frame.load(std::sync::atomic::Ordering::SeqCst) {
            break;
        }
    }

    let _ = pipeline.set_state(gst::State::Null);

    // Get captured frame
    let (rgb_data, width, height) = frame_data
        .lock()
        .unwrap()
        .take()
        .ok_or_else(|| MediaError::ThumbnailGeneration("No frame captured".to_string()))?;

    encode_thumbnail(&rgb_data, width, height, duration_ms)
}

/// Extract thumbnail from memory using appsrc + decodebin.
#[allow(clippy::type_complexity)]
fn extract_thumbnail_from_memory_impl(
    data: &[u8],
    seek_seconds: u64,
) -> Result<(Vec<u8>, u32, u32, u64), MediaError> {
    let pipeline = gst::Pipeline::new();

    // Create appsrc for memory data
    let appsrc = gst_app::AppSrc::builder()
        .stream_type(gst_app::AppStreamType::RandomAccess)
        .size(data.len() as i64)
        .format(gst::Format::Bytes)
        .build();

    // Set up data feeding
    let data_arc = Arc::new(data.to_vec());
    let data_for_need = data_arc.clone();
    let read_pos = Arc::new(Mutex::new(0usize));
    let read_pos_need = read_pos.clone();
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

                let chunk_size =
                    (if hint > 0 { hint as usize } else { 65536 }).min(data.len() - *pos);
                let slice = &data[*pos..*pos + chunk_size];

                let mut buffer = gst::Buffer::from_slice(slice.to_vec());
                if let Some(buf) = buffer.get_mut() {
                    buf.set_offset(*pos as u64);
                }

                let _ = appsrc.push_buffer(buffer);
                *pos += chunk_size;
            })
            .seek_data(move |_appsrc, offset| {
                *read_pos_seek.lock().unwrap() = offset as usize;
                true
            })
            .build(),
    );

    // Create decodebin (auto-selects HW decoder)
    let decodebin = gst::ElementFactory::make("decodebin3")
        .build()
        .or_else(|_| gst::ElementFactory::make("decodebin").build())
        .map_err(|e| MediaError::PlaybackError(format!("Failed to create decodebin: {}", e)))?;

    // Create appsink
    let appsink = gst_app::AppSink::builder()
        .caps(
            &gst_video::VideoCapsBuilder::new()
                .format(gst_video::VideoFormat::Rgbx)
                .build(),
        )
        .sync(false)
        .build();

    pipeline
        .add_many([appsrc.upcast_ref(), &decodebin])
        .map_err(|e| MediaError::PlaybackError(format!("Failed to add elements: {}", e)))?;
    appsrc
        .link(&decodebin)
        .map_err(|e| MediaError::PlaybackError(format!("Failed to link: {}", e)))?;

    // Frame storage
    let frame_data: Arc<Mutex<Option<(Vec<u8>, u32, u32)>>> = Arc::new(Mutex::new(None));
    let frame_data_clone = frame_data.clone();
    let got_frame = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let got_frame_clone = got_frame.clone();

    appsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |appsink| {
                if got_frame_clone.swap(true, std::sync::atomic::Ordering::SeqCst) {
                    return Err(gst::FlowError::Eos);
                }

                let sample = appsink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                let caps = sample.caps().ok_or(gst::FlowError::Error)?;
                let info =
                    gst_video::VideoInfo::from_caps(caps).map_err(|_| gst::FlowError::Error)?;

                let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info)
                    .map_err(|_| gst::FlowError::Error)?;

                let width = frame.width();
                let height = frame.height();
                let plane_data = frame.plane_data(0).map_err(|_| gst::FlowError::Error)?;
                let stride = frame.plane_stride()[0] as usize;

                let mut rgb_data = Vec::with_capacity((width * height * 3) as usize);
                for y in 0..height as usize {
                    for x in 0..width as usize {
                        let offset = y * stride + x * 4;
                        rgb_data.push(plane_data[offset]);
                        rgb_data.push(plane_data[offset + 1]);
                        rgb_data.push(plane_data[offset + 2]);
                    }
                }

                *frame_data_clone.lock().unwrap() = Some((rgb_data, width, height));
                Err(gst::FlowError::Eos)
            })
            .build(),
    );

    // Handle dynamic pad linking from decodebin
    let pipeline_weak = pipeline.downgrade();
    let appsink_clone = appsink.clone();
    let video_linked = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let video_linked_clone = video_linked.clone();

    decodebin.connect_pad_added(move |_decodebin, src_pad| {
        if let Some(caps) = src_pad
            .current_caps()
            .or_else(|| Some(src_pad.query_caps(None)))
        {
            if let Some(s) = caps.structure(0) {
                if s.name().as_str().starts_with("video/") {
                    if let Some(pipeline) = pipeline_weak.upgrade() {
                        let convert = gst::ElementFactory::make("videoconvert").build().ok();

                        if let Some(convert) = convert {
                            let elements: &[&gst::Element] =
                                &[&convert, appsink_clone.upcast_ref()];
                            if pipeline.add_many(elements).is_ok()
                                && gst::Element::link_many(elements).is_ok()
                            {
                                for e in elements {
                                    let _ = e.sync_state_with_parent();
                                }
                                if let Some(sink_pad) = convert.static_pad("sink") {
                                    if src_pad.link(&sink_pad).is_ok() {
                                        video_linked_clone
                                            .store(true, std::sync::atomic::Ordering::SeqCst);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    });

    pipeline
        .set_state(gst::State::Paused)
        .map_err(|e| MediaError::PlaybackError(format!("Failed to pause: {}", e)))?;

    let bus = pipeline
        .bus()
        .ok_or_else(|| MediaError::PlaybackError("No bus".to_string()))?;

    let mut state = 0u8; // 0=preroll, 1=seeked, 2=playing
    let mut duration_ms = 0u64;
    let timeout = std::time::Instant::now() + std::time::Duration::from_secs(15);

    while std::time::Instant::now() < timeout {
        if let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(100)) {
            match msg.view() {
                gst::MessageView::AsyncDone(_) => {
                    // Only proceed once video is linked
                    if !video_linked.load(std::sync::atomic::Ordering::SeqCst) {
                        continue;
                    }

                    match state {
                        0 => {
                            duration_ms = pipeline
                                .query_duration::<gst::ClockTime>()
                                .map(|d| d.mseconds())
                                .unwrap_or(0);

                            let seek_pos = if duration_ms > 0 {
                                let ten_percent = duration_ms / 10;
                                let target = (seek_seconds * 1000).max(ten_percent);
                                target.min(duration_ms - 1000)
                            } else {
                                0
                            };

                            if seek_pos > 0 {
                                let _ = pipeline.seek_simple(
                                    gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
                                    gst::ClockTime::from_mseconds(seek_pos),
                                );
                                state = 1;
                            } else {
                                let _ = pipeline.set_state(gst::State::Playing);
                                state = 2;
                            }
                        }
                        1 => {
                            let _ = pipeline.set_state(gst::State::Playing);
                            state = 2;
                        }
                        _ => {}
                    }
                }
                gst::MessageView::Eos(_) => break,
                gst::MessageView::Error(err) => {
                    let _ = pipeline.set_state(gst::State::Null);
                    return Err(MediaError::PlaybackError(format!(
                        "Pipeline error: {} ({:?})",
                        err.error(),
                        err.debug()
                    )));
                }
                _ => {}
            }
        }

        if got_frame.load(std::sync::atomic::Ordering::SeqCst) {
            break;
        }
    }

    let _ = pipeline.set_state(gst::State::Null);

    let (rgb_data, width, height) = frame_data
        .lock()
        .unwrap()
        .take()
        .ok_or_else(|| MediaError::ThumbnailGeneration("No frame captured".to_string()))?;

    encode_thumbnail(&rgb_data, width, height, duration_ms)
}

/// Encode RGB frame data to WebP thumbnail.
fn encode_thumbnail(
    rgb_data: &[u8],
    width: u32,
    height: u32,
    duration_ms: u64,
) -> Result<(Vec<u8>, u32, u32, u64), MediaError> {
    // Calculate thumbnail dimensions (maintain aspect ratio)
    let (thumb_width, thumb_height) = if width > height {
        let w = THUMBNAIL_MAX_SIZE.min(width);
        let h = (height as f64 * w as f64 / width as f64) as u32;
        (w, h.max(1))
    } else {
        let h = THUMBNAIL_MAX_SIZE.min(height);
        let w = (width as f64 * h as f64 / height as f64) as u32;
        (w.max(1), h)
    };

    let img = image::RgbImage::from_raw(width, height, rgb_data.to_vec())
        .ok_or_else(|| MediaError::ThumbnailGeneration("Invalid frame data".to_string()))?;

    let resized = image::imageops::resize(
        &img,
        thumb_width,
        thumb_height,
        image::imageops::FilterType::Triangle,
    );

    let dyn_img = image::DynamicImage::ImageRgb8(resized);

    let mut webp_bytes: Vec<u8> = Vec::new();
    let encoder = image::codecs::webp::WebPEncoder::new_lossless(&mut webp_bytes);
    encoder
        .write_image(
            dyn_img.as_bytes(),
            thumb_width,
            thumb_height,
            image::ExtendedColorType::Rgb8,
        )
        .map_err(|e| MediaError::ThumbnailGeneration(format!("WebP encoding failed: {}", e)))?;

    tracing::debug!(
        "Extracted video thumbnail: {}x{} -> {}x{}, {} bytes",
        width,
        height,
        thumb_width,
        thumb_height,
        webp_bytes.len()
    );

    Ok((webp_bytes, thumb_width, thumb_height, duration_ms))
}

/// Get video duration in milliseconds.
pub fn get_video_duration(source: &Path) -> Result<u64, MediaError> {
    ensure_gst_init()?;

    let uri = format!(
        "file://{}",
        source
            .canonicalize()
            .map_err(MediaError::FileRead)?
            .display()
    );

    let pipeline_str = format!(
        "uridecodebin uri=\"{}\" ! fakesink",
        uri.replace('"', "\\\"")
    );

    let pipeline = gst::parse::launch(&pipeline_str)
        .map_err(|e| MediaError::PlaybackError(format!("Pipeline error: {}", e)))?
        .downcast::<gst::Pipeline>()
        .map_err(|_| MediaError::PlaybackError("Not a pipeline".to_string()))?;

    pipeline.set_state(gst::State::Paused).ok();
    let _ = pipeline.state(gst::ClockTime::from_seconds(5));

    let duration = pipeline
        .query_duration::<gst::ClockTime>()
        .map(|d| d.mseconds())
        .unwrap_or(0);

    let _ = pipeline.set_state(gst::State::Null);

    Ok(duration)
}

/// Get video dimensions.
pub fn get_video_dimensions(source: &Path) -> Result<(u32, u32), MediaError> {
    // Extract thumbnail and get dimensions from that
    let (_, width, height, _) = extract_video_thumbnail(source)?;
    Ok((width, height))
}

/// Get video duration from memory.
pub fn get_video_duration_from_memory(data: &[u8]) -> Result<u64, MediaError> {
    let (_, _, _, duration_ms) = extract_video_thumbnail_from_memory(data)?;
    Ok(duration_ms)
}

/// Get video dimensions from memory.
pub fn get_video_dimensions_from_memory(data: &[u8]) -> Result<(u32, u32), MediaError> {
    let (_, width, height, _) = extract_video_thumbnail_from_memory(data)?;
    Ok((width, height))
}

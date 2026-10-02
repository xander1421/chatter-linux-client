//! iOS Metal-accelerated video rendering.
//!
//! Uses VideoToolbox for hardware decoding and Metal for display.
//! GStreamer's applemedia plugins provide hardware acceleration on iOS.
//!
//! Note: Full Metal texture sharing requires Slint Metal API support.
//! Currently uses optimized software path with hardware decoding.

use std::cell::RefCell;
use std::sync::{Arc, Mutex};

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;

use slint::ComponentHandle;

use crate::MainWindow;

/// Global Metal video state (must be thread_local for rendering notifier).
thread_local! {
    static METAL_VIDEO_STATE: RefCell<Option<MetalVideoState>> = const { RefCell::new(None) };
}

/// State for Metal video playback on iOS.
struct MetalVideoState {
    /// AppSink for receiving video frames.
    appsink: gst_app::AppSink,
    /// Pipeline for playback.
    pipeline: Option<gst::Pipeline>,
    /// Next frame ready for rendering (RGBA bytes).
    next_frame: Arc<Mutex<Option<FrameData>>>,
    /// Whether connected.
    connected: bool,
    /// Video duration in ms.
    duration_ms: u64,
    /// Whether playing.
    is_playing: bool,
    /// Video dimensions.
    width: u32,
    height: u32,
}

/// Frame data for software rendering fallback.
struct FrameData {
    width: u32,
    height: u32,
    rgba_data: Vec<u8>,
}

/// Check if Metal video is available on iOS.
pub fn is_metal_available() -> bool {
    // Check for VideoToolbox decoder (hardware decoding on iOS)
    gst::ElementFactory::find("vtdec").is_some()
        || gst::ElementFactory::find("vtdec_hw").is_some()
}

/// Check if Metal context is connected (ready to play video).
pub fn is_metal_connected() -> bool {
    METAL_VIDEO_STATE.with(|cell| {
        cell.borrow()
            .as_ref()
            .map(|s| s.connected)
            .unwrap_or(false)
    })
}

/// Initialize Metal video rendering for the window on iOS.
pub fn init_metal_video(window: &MainWindow) -> bool {
    // Create video appsink for RGBA output
    // iOS doesn't have direct GL texture sharing via GStreamer like Linux
    // We use VideoToolbox for HW decode and software RGB conversion
    let appsink = gst_app::AppSink::builder()
        .caps(
            &gst_video::VideoCapsBuilder::new()
                .format(gst_video::VideoFormat::Rgba)
                .build(),
        )
        .enable_last_sample(false)
        .max_buffers(1)
        .sync(true) // Sync to clock for proper playback timing
        .build();

    let state = MetalVideoState {
        appsink,
        pipeline: None,
        next_frame: Arc::new(Mutex::new(None)),
        connected: true, // No context setup needed for software path
        duration_ms: 0,
        is_playing: false,
        width: 0,
        height: 0,
    };

    METAL_VIDEO_STATE.with(|cell| {
        *cell.borrow_mut() = Some(state);
    });

    // Set up rendering notifier for frame updates
    let weak = window.as_weak();
    if let Err(e) = window.window().set_rendering_notifier(move |render_state, _graphics_api| {
        crate::record_startup_frame(&render_state);
        match render_state {
            slint::RenderingState::BeforeRendering => {
                METAL_VIDEO_STATE.with(|cell| {
                    if let Some(ref mut video_state) = *cell.borrow_mut() {
                        if video_state.is_playing {
                            if let Some(image) = fetch_next_frame_ios(video_state) {
                                if let Some(w) = weak.upgrade() {
                                    w.set_video_player_frame(image);
                                }
                            }
                        }
                    }
                });
            }
            slint::RenderingState::RenderingTeardown => {
                METAL_VIDEO_STATE.with(|cell| {
                    if let Some(ref mut video_state) = *cell.borrow_mut() {
                        if let Ok(mut next) = video_state.next_frame.lock() {
                            *next = None;
                        }
                    }
                });
            }
            _ => {}
        }
    }) {
        tracing::warn!("Failed to set rendering notifier on iOS: {:?}", e);
        return false;
    }

    tracing::info!("iOS Metal video rendering initialized (VideoToolbox HW decode)");
    true
}

/// Fetch the next frame as a Slint image on iOS.
fn fetch_next_frame_ios(state: &mut MetalVideoState) -> Option<slint::Image> {
    if let Some(frame) = state.next_frame.lock().ok()?.take() {
        state.width = frame.width;
        state.height = frame.height;

        // Convert RGBA bytes to Slint image
        let buffer = slint::SharedPixelBuffer::clone_from_slice(
            &frame.rgba_data,
            frame.width,
            frame.height,
        );
        return Some(slint::Image::from_rgba8(buffer));
    }
    None
}

/// Start playing video from memory using Metal/VideoToolbox on iOS.
pub fn start_metal_video(data: Vec<u8>) -> Option<f64> {
    METAL_VIDEO_STATE.with(|cell| {
        let mut state_ref = cell.borrow_mut();
        let state = state_ref.as_mut()?;

        // Stop any existing pipeline
        if let Some(ref pipeline) = state.pipeline {
            let _ = pipeline.set_state(gst::State::Null);
        }

        // Create new pipeline
        let pipeline = gst::Pipeline::new();

        // Create appsrc for memory data
        let appsrc = gst_app::AppSrc::builder()
            .stream_type(gst_app::AppStreamType::RandomAccess)
            .size(data.len() as i64)
            .format(gst::Format::Bytes)
            .build();

        // Set up data feeding
        let data_arc = Arc::new(data);
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
                    if offset_usize > data_arc.len() {
                        return false;
                    }
                    *read_pos_seek.lock().unwrap() = offset_usize;
                    true
                })
                .build()
        );

        // Create decodebin - on iOS, this will use VideoToolbox (vtdec) for HW decode
        let decodebin = gst::ElementFactory::make("decodebin3")
            .build()
            .ok()?;

        // Add elements to pipeline
        pipeline.add_many([appsrc.upcast_ref(), &decodebin]).ok()?;
        appsrc.link(&decodebin).ok()?;

        // Get our appsink for video output
        let video_appsink = state.appsink.clone();
        let next_frame = state.next_frame.clone();

        // Set up appsink callback for frame delivery
        video_appsink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |appsink| {
                    let sample = appsink.pull_sample().map_err(|_| gst::FlowError::Flushing)?;
                    let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                    let caps = sample.caps().ok_or(gst::FlowError::NotNegotiated)?;
                    let info = gst_video::VideoInfo::from_caps(caps)
                        .map_err(|_| gst::FlowError::NotNegotiated)?;

                    let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                    let rgba_data = map.as_slice().to_vec();

                    *next_frame.lock().unwrap() = Some(FrameData {
                        width: info.width(),
                        height: info.height(),
                        rgba_data,
                    });

                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );

        // Handle decodebin pad-added (video and audio)
        let appsink_for_link = video_appsink.clone();
        let pipeline_weak = pipeline.downgrade();
        decodebin.connect_pad_added(move |_decodebin, src_pad| {
            let caps = src_pad.current_caps().or_else(|| Some(src_pad.query_caps(None)));
            if let Some(caps) = caps {
                let structure = caps.structure(0);
                if let Some(s) = structure {
                    let name = s.name().as_str();
                    if name.starts_with("video/") {
                        if let Some(pipeline) = pipeline_weak.upgrade() {
                            // Create video conversion chain for RGBA output
                            if let Ok(videoconvert) = gst::ElementFactory::make("videoconvert").build() {
                                pipeline.add_many([&videoconvert, appsink_for_link.upcast_ref()]).ok();
                                gst::Element::link_many([&videoconvert, appsink_for_link.upcast_ref()]).ok();
                                videoconvert.sync_state_with_parent().ok();
                                appsink_for_link.sync_state_with_parent().ok();

                                if let Some(sink_pad) = videoconvert.static_pad("sink") {
                                    src_pad.link(&sink_pad).ok();
                                    tracing::info!("iOS video sink linked (VideoToolbox HW decode -> RGBA)");
                                }
                            }
                        }
                    } else if name.starts_with("audio/") {
                        // Create and link audio sink (uses AudioToolbox on iOS)
                        if let Some(pipeline) = pipeline_weak.upgrade() {
                            if let Ok(audioconvert) = gst::ElementFactory::make("audioconvert").build() {
                                if let Ok(audioresample) = gst::ElementFactory::make("audioresample").build() {
                                    // autoaudiosink will use osxaudiosink on iOS
                                    if let Ok(audiosink) = gst::ElementFactory::make("autoaudiosink")
                                        .name("autoaudiosink0")
                                        .build()
                                    {
                                        pipeline.add_many([&audioconvert, &audioresample, &audiosink]).ok();
                                        gst::Element::link_many([&audioconvert, &audioresample, &audiosink]).ok();
                                        audioconvert.sync_state_with_parent().ok();
                                        audioresample.sync_state_with_parent().ok();
                                        audiosink.sync_state_with_parent().ok();

                                        if let Some(sink_pad) = audioconvert.static_pad("sink") {
                                            src_pad.link(&sink_pad).ok();
                                            tracing::info!("iOS video audio sink linked");
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        });

        // Start pipeline
        pipeline.set_state(gst::State::Playing).ok()?;

        // Wait for preroll and get duration
        let _ = pipeline.state(gst::ClockTime::from_seconds(5));
        let duration = pipeline
            .query_duration::<gst::ClockTime>()
            .map(|d| d.mseconds() as f64 / 1000.0)
            .unwrap_or(0.0);

        state.pipeline = Some(pipeline);
        state.duration_ms = (duration * 1000.0) as u64;
        state.is_playing = true;

        tracing::info!("iOS Metal video started (VideoToolbox), duration: {}s", duration);
        Some(duration)
    })
}

/// Stop Metal video playback on iOS.
pub fn stop_metal_video() {
    METAL_VIDEO_STATE.with(|cell| {
        if let Some(ref mut state) = *cell.borrow_mut() {
            if let Some(ref pipeline) = state.pipeline {
                let _ = pipeline.set_state(gst::State::Null);
            }
            state.pipeline = None;
            state.is_playing = false;
            if let Ok(mut next) = state.next_frame.lock() {
                *next = None;
            }
        }
    });
}

/// Pause/resume Metal video on iOS.
pub fn set_metal_video_playing(playing: bool) {
    METAL_VIDEO_STATE.with(|cell| {
        if let Some(ref mut state) = *cell.borrow_mut() {
            state.is_playing = playing;
            if let Some(ref pipeline) = state.pipeline {
                if playing {
                    let _ = pipeline.set_state(gst::State::Playing);
                } else {
                    let _ = pipeline.set_state(gst::State::Paused);
                }
            }
        }
    });
}

/// Get current Metal video position in seconds on iOS.
pub fn get_metal_video_position() -> Option<f64> {
    METAL_VIDEO_STATE.with(|cell| {
        let state_ref = cell.borrow();
        let state = state_ref.as_ref()?;
        let pipeline = state.pipeline.as_ref()?;
        pipeline
            .query_position::<gst::ClockTime>()
            .map(|p| p.mseconds() as f64 / 1000.0)
    })
}

/// Seek Metal video to position in seconds on iOS.
pub fn seek_metal_video(position: f64) {
    METAL_VIDEO_STATE.with(|cell| {
        if let Some(ref state) = *cell.borrow() {
            if let Some(ref pipeline) = state.pipeline {
                let pos_ns = (position * 1_000_000_000.0) as u64;
                let _ = pipeline.seek_simple(
                    gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
                    gst::ClockTime::from_nseconds(pos_ns),
                );
            }
        }
    });
}

/// Set Metal video volume on iOS.
pub fn set_metal_video_volume(volume: f64) {
    METAL_VIDEO_STATE.with(|cell| {
        if let Some(ref state) = *cell.borrow() {
            if let Some(ref pipeline) = state.pipeline {
                if let Some(audio_sink) = pipeline.by_name("autoaudiosink0") {
                    audio_sink.set_property("volume", volume.clamp(0.0, 1.0));
                }
            }
        }
    });
}

/// Check if Metal video is currently playing on iOS.
pub fn is_metal_video_active() -> bool {
    METAL_VIDEO_STATE.with(|cell| {
        cell.borrow()
            .as_ref()
            .map(|s| s.pipeline.is_some() && s.is_playing)
            .unwrap_or(false)
    })
}

/// Get Metal video duration in seconds on iOS.
pub fn get_metal_video_duration() -> Option<f64> {
    METAL_VIDEO_STATE.with(|cell| {
        cell.borrow()
            .as_ref()
            .filter(|s| s.duration_ms > 0)
            .map(|s| s.duration_ms as f64 / 1000.0)
    })
}

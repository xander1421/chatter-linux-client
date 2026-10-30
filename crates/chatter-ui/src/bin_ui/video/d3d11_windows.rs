//! Windows D3D11-accelerated video rendering.
//!
//! Uses GStreamer's D3D11 plugins for hardware video decoding (DXVA2/D3D11VA).
//! Currently outputs to software RGB frames since Slint doesn't expose D3D11
//! texture sharing APIs yet. Hardware decoding still provides significant
//! performance benefits over pure software decoding.
//!
//! Note: Full D3D11 texture sharing can be added when Slint adds D3D11 API support.

use std::cell::RefCell;
use std::sync::{Arc, Mutex};

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;

use slint::ComponentHandle;

use crate::MainWindow;

/// Global D3D11 video state (must be thread_local for rendering notifier).
thread_local! {
    static D3D11_VIDEO_STATE: RefCell<Option<D3D11VideoState>> = const { RefCell::new(None) };
}

/// State for D3D11 video playback on Windows.
struct D3D11VideoState {
    /// AppSink for receiving video frames (RGBA).
    appsink: gst_app::AppSink,
    /// Pipeline for playback.
    pipeline: Option<gst::Pipeline>,
    /// Next frame ready for rendering (RGBA bytes).
    next_frame: Arc<Mutex<Option<FrameData>>>,
    /// Whether connected (ready for playback).
    connected: bool,
    /// Video duration in ms.
    duration_ms: u64,
    /// Whether playing.
    is_playing: bool,
    /// Video dimensions.
    width: u32,
    height: u32,
}

/// Frame data for software rendering path.
struct FrameData {
    width: u32,
    height: u32,
    rgba_data: Vec<u8>,
}

/// Check if D3D11 video is available on Windows.
pub fn is_d3d11_available() -> bool {
    // Check for D3D11 video decoder (hardware decoding on Windows)
    gst::ElementFactory::find("d3d11h264dec").is_some()
        || gst::ElementFactory::find("d3d11h265dec").is_some()
        || gst::ElementFactory::find("d3d11vp9dec").is_some()
        // Fallback to checking for d3d11 plugin presence
        || gst::ElementFactory::find("d3d11videosink").is_some()
}

/// Check if D3D11 context is connected (ready to play video).
pub fn is_d3d11_connected() -> bool {
    D3D11_VIDEO_STATE.with(|cell| cell.borrow().as_ref().map(|s| s.connected).unwrap_or(false))
}

/// Initialize D3D11 video rendering for the window on Windows.
pub fn init_d3d11_video(window: &MainWindow) -> bool {
    if !is_d3d11_available() {
        tracing::info!("D3D11 video not available (no D3D11 decoders found)");
        return false;
    }

    // Create video appsink for RGBA output
    // We use hardware decoding (D3D11) but output to software RGBA
    // since Slint doesn't expose D3D11 texture APIs yet
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

    let state = D3D11VideoState {
        appsink,
        pipeline: None,
        next_frame: Arc::new(Mutex::new(None)),
        connected: true, // No special context setup needed for software output path
        duration_ms: 0,
        is_playing: false,
        width: 0,
        height: 0,
    };

    D3D11_VIDEO_STATE.with(|cell| {
        *cell.borrow_mut() = Some(state);
    });

    // Set up rendering notifier for frame updates
    let weak = window.as_weak();
    if let Err(e) = window
        .window()
        .set_rendering_notifier(move |render_state, _graphics_api| {
            crate::record_startup_frame(&render_state);
            match render_state {
                slint::RenderingState::BeforeRendering => {
                    D3D11_VIDEO_STATE.with(|cell| {
                        if let Some(ref mut video_state) = *cell.borrow_mut() {
                            if video_state.is_playing {
                                if let Some(image) = fetch_next_frame_d3d11(video_state) {
                                    if let Some(w) = weak.upgrade() {
                                        w.global::<crate::ChatPane>().set_video_player_frame(image);
                                    }
                                }
                            }
                        }
                    });
                }
                slint::RenderingState::RenderingTeardown => {
                    D3D11_VIDEO_STATE.with(|cell| {
                        if let Some(ref mut video_state) = *cell.borrow_mut() {
                            if let Ok(mut next) = video_state.next_frame.lock() {
                                *next = None;
                            }
                        }
                    });
                }
                _ => {}
            }
        })
    {
        tracing::warn!("Failed to set rendering notifier on Windows: {:?}", e);
        return false;
    }

    tracing::info!("Windows D3D11 video rendering initialized (HW decode -> software output)");
    true
}

/// Fetch the next frame as a Slint image on Windows.
fn fetch_next_frame_d3d11(state: &mut D3D11VideoState) -> Option<slint::Image> {
    if let Some(frame) = state.next_frame.lock().ok()?.take() {
        state.width = frame.width;
        state.height = frame.height;

        return Some(crate::bin_ui::slint_conversions::rgba_to_slint_image(
            &frame.rgba_data,
            frame.width,
            frame.height,
        ));
    }
    None
}

/// Start playing video from memory using D3D11 hardware decoding on Windows.
pub fn start_d3d11_video(data: Vec<u8>) -> Option<f64> {
    D3D11_VIDEO_STATE.with(|cell| {
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

        // Create decodebin3 - on Windows this will prefer D3D11 hardware decoders
        // (d3d11h264dec, d3d11h265dec, d3d11vp9dec, etc.)
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
                            // Create video conversion chain
                            // Use d3d11download if available for GPU->CPU transfer,
                            // then videoconvert for format conversion to RGBA
                            let d3d11download = gst::ElementFactory::make("d3d11download").build().ok();

                            if let Ok(videoconvert) = gst::ElementFactory::make("videoconvert").build() {
                                if let Some(download) = d3d11download {
                                    // D3D11 path: download -> convert -> appsink
                                    pipeline.add_many([&download, &videoconvert, appsink_for_link.upcast_ref()]).ok();
                                    gst::Element::link_many([&download, &videoconvert, appsink_for_link.upcast_ref()]).ok();
                                    download.sync_state_with_parent().ok();
                                    videoconvert.sync_state_with_parent().ok();
                                    appsink_for_link.sync_state_with_parent().ok();

                                    if let Some(sink_pad) = download.static_pad("sink") {
                                        src_pad.link(&sink_pad).ok();
                                        tracing::info!("Windows video sink linked (D3D11 HW decode -> d3d11download -> RGBA)");
                                    }
                                } else {
                                    // Fallback: videoconvert -> appsink
                                    pipeline.add_many([&videoconvert, appsink_for_link.upcast_ref()]).ok();
                                    gst::Element::link_many([&videoconvert, appsink_for_link.upcast_ref()]).ok();
                                    videoconvert.sync_state_with_parent().ok();
                                    appsink_for_link.sync_state_with_parent().ok();

                                    if let Some(sink_pad) = videoconvert.static_pad("sink") {
                                        src_pad.link(&sink_pad).ok();
                                        tracing::info!("Windows video sink linked (software decode -> RGBA)");
                                    }
                                }
                            }
                        }
                    } else if name.starts_with("audio/") {
                        // Create and link audio sink (uses Windows Audio Session API)
                        if let Some(pipeline) = pipeline_weak.upgrade() {
                            if let Ok(audioconvert) = gst::ElementFactory::make("audioconvert").build() {
                                if let Ok(audioresample) = gst::ElementFactory::make("audioresample").build() {
                                    // autoaudiosink will use wasapisink or directsoundsink on Windows
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
                                            tracing::info!("Windows video audio sink linked (WASAPI)");
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

        tracing::info!("Windows D3D11 video started, duration: {}s", duration);
        Some(duration)
    })
}

/// Stop D3D11 video playback on Windows.
pub fn stop_d3d11_video() {
    D3D11_VIDEO_STATE.with(|cell| {
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

/// Pause/resume D3D11 video on Windows.
pub fn set_d3d11_video_playing(playing: bool) {
    D3D11_VIDEO_STATE.with(|cell| {
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

/// Get current D3D11 video position in seconds on Windows.
pub fn get_d3d11_video_position() -> Option<f64> {
    D3D11_VIDEO_STATE.with(|cell| {
        let state_ref = cell.borrow();
        let state = state_ref.as_ref()?;
        let pipeline = state.pipeline.as_ref()?;
        pipeline
            .query_position::<gst::ClockTime>()
            .map(|p| p.mseconds() as f64 / 1000.0)
    })
}

/// Seek D3D11 video to position in seconds on Windows.
pub fn seek_d3d11_video(position: f64) {
    D3D11_VIDEO_STATE.with(|cell| {
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

/// Set D3D11 video volume on Windows.
pub fn set_d3d11_video_volume(volume: f64) {
    D3D11_VIDEO_STATE.with(|cell| {
        if let Some(ref state) = *cell.borrow() {
            if let Some(ref pipeline) = state.pipeline {
                if let Some(audio_sink) = pipeline.by_name("autoaudiosink0") {
                    audio_sink.set_property("volume", volume.clamp(0.0, 1.0));
                }
            }
        }
    });
}

/// Check if D3D11 video is currently playing on Windows.
pub fn is_d3d11_video_active() -> bool {
    D3D11_VIDEO_STATE.with(|cell| {
        cell.borrow()
            .as_ref()
            .map(|s| s.pipeline.is_some() && s.is_playing)
            .unwrap_or(false)
    })
}

/// Get D3D11 video duration in seconds on Windows.
pub fn get_d3d11_video_duration() -> Option<f64> {
    D3D11_VIDEO_STATE.with(|cell| {
        cell.borrow()
            .as_ref()
            .filter(|s| s.duration_ms > 0)
            .map(|s| s.duration_ms as f64 / 1000.0)
    })
}

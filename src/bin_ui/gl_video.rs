//! GL-accelerated video rendering integration.
//!
//! Sets up Slint's rendering notifier to fetch GL textures from GStreamer
//! and display them with zero-copy GPU rendering.

use std::cell::RefCell;
use std::sync::{Arc, Mutex};

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_gl as gst_gl;
use gstreamer_gl::prelude::*;
use gstreamer_gl_egl as gst_gl_egl;
use gstreamer_video as gst_video;

use slint::ComponentHandle;

use crate::MainWindow;

/// Global GL video state (must be thread_local for rendering notifier).
thread_local! {
    static GL_VIDEO_STATE: RefCell<Option<GLVideoState>> = const { RefCell::new(None) };
}

/// State for GL video playback.
struct GLVideoState {
    /// AppSink for receiving GL memory samples.
    appsink: gst_app::AppSink,
    /// GL sink bin wrapper.
    glsinkbin: gst::Element,
    /// Pipeline for playback.
    pipeline: Option<gst::Pipeline>,
    /// Next frame ready for rendering.
    next_frame: Arc<Mutex<Option<(gst_video::VideoInfo, gst::Buffer)>>>,
    /// Current frame being displayed.
    current_frame: Option<gst_gl::GLVideoFrame<gst_gl::gl_video_frame::Readable>>,
    /// GStreamer GL context (wrapped from Slint's EGL context).
    gst_gl_context: Option<gst_gl::GLContext>,
    /// Whether GL is connected.
    connected: bool,
    /// Video duration in ms.
    duration_ms: u64,
    /// Whether playing.
    is_playing: bool,
}

/// Check if GL video is available.
pub fn is_gl_available() -> bool {
    gst::ElementFactory::find("glsinkbin").is_some()
}

/// Check if GL context is connected (ready to play video).
pub fn is_gl_connected() -> bool {
    GL_VIDEO_STATE.with(|cell| {
        cell.borrow()
            .as_ref()
            .map(|s| s.connected)
            .unwrap_or(false)
    })
}

/// Initialize GL video rendering for the window.
///
/// Sets up the rendering notifier to fetch GL textures during render.
/// Returns true if GL video was successfully initialized.
pub fn init_gl_video(window: &MainWindow) -> bool {
    if !is_gl_available() {
        tracing::info!("GL video not available (glsinkbin not found)");
        return false;
    }

    // Create GL video state
    let appsink = gst_app::AppSink::builder()
        .caps(
            &gst_video::VideoCapsBuilder::new()
                .features([gst_gl::CAPS_FEATURE_MEMORY_GL_MEMORY])
                .format(gst_video::VideoFormat::Rgba)
                .field("texture-target", "2D")
                .field("pixel-aspect-ratio", gst::Fraction::new(1, 1))
                .build(),
        )
        .enable_last_sample(false)
        .max_buffers(1)
        .build();

    let glsinkbin = match gst::ElementFactory::make("glsinkbin")
        .property("sink", &appsink)
        .build()
    {
        Ok(element) => element,
        Err(e) => {
            tracing::error!("Failed to create glsinkbin: {}", e);
            return false;
        }
    };

    let state = GLVideoState {
        appsink,
        glsinkbin,
        pipeline: None,
        next_frame: Arc::new(Mutex::new(None)),
        current_frame: None,
        gst_gl_context: None,
        connected: false,
        duration_ms: 0,
        is_playing: false,
    };

    GL_VIDEO_STATE.with(|cell| {
        *cell.borrow_mut() = Some(state);
    });

    // Set up rendering notifier
    let weak = window.as_weak();
    if let Err(e) = window.window().set_rendering_notifier(move |state, graphics_api| {
        match state {
            slint::RenderingState::RenderingSetup => {
                // Connect GL context when rendering is set up
                GL_VIDEO_STATE.with(|cell| {
                    if let Some(ref mut video_state) = *cell.borrow_mut() {
                        if let Err(e) = connect_gl_context(video_state, graphics_api) {
                            tracing::warn!("Failed to connect GL context: {}", e);
                        }
                    }
                });
            }
            slint::RenderingState::BeforeRendering => {
                // Fetch next frame and update video player
                GL_VIDEO_STATE.with(|cell| {
                    if let Some(ref mut video_state) = *cell.borrow_mut() {
                        if video_state.is_playing {
                            if let Some(image) = fetch_next_frame(video_state) {
                                if let Some(w) = weak.upgrade() {
                                    w.set_video_player_frame(image);
                                }
                            }
                        }
                    }
                });
            }
            slint::RenderingState::RenderingTeardown => {
                // Clean up GL resources
                GL_VIDEO_STATE.with(|cell| {
                    if let Some(ref mut video_state) = *cell.borrow_mut() {
                        video_state.current_frame = None;
                        if let Ok(mut next) = video_state.next_frame.lock() {
                            *next = None;
                        }
                        if let Some(ref ctx) = video_state.gst_gl_context {
                            let _ = ctx.activate(false);
                        }
                    }
                });
            }
            _ => {}
        }
    }) {
        tracing::warn!("Failed to set rendering notifier: {:?}", e);
        return false;
    }

    tracing::info!("GL video rendering initialized");
    true
}

/// Connect the GL context from Slint to GStreamer.
fn connect_gl_context(
    state: &mut GLVideoState,
    graphics_api: &slint::GraphicsAPI<'_>,
) -> Result<(), String> {
    if state.connected {
        return Ok(());
    }

    // Load EGL functions
    let egl = match graphics_api {
        slint::GraphicsAPI::NativeOpenGL { get_proc_address } => {
            glutin_egl_sys::egl::Egl::load_with(|symbol| {
                get_proc_address(&std::ffi::CString::new(symbol).unwrap())
            })
        }
        _ => return Err("Unsupported graphics API - requires OpenGL".to_string()),
    };

    // Wrap Slint's EGL context for GStreamer
    let (gst_gl_context, _gst_gl_display) = unsafe {
        let platform = gst_gl::GLPlatform::EGL;
        let egl_display = egl.GetCurrentDisplay();
        let display = gst_gl_egl::GLDisplayEGL::with_egl_display(egl_display as usize)
            .map_err(|e| format!("Failed to wrap EGL display: {}", e))?;
        let native_context = egl.GetCurrentContext();

        let context = gst_gl::GLContext::new_wrapped(
            &display,
            native_context as _,
            platform,
            gst_gl::GLContext::current_gl_api(platform).0,
        )
        .ok_or_else(|| "Failed to wrap GL context".to_string())?;

        (context, display)
    };

    // Activate and fill context info
    gst_gl_context
        .activate(true)
        .map_err(|e| format!("Failed to activate GL context: {}", e))?;
    gst_gl_context
        .fill_info()
        .map_err(|e| format!("Failed to fill GL info: {}", e))?;

    state.gst_gl_context = Some(gst_gl_context.clone());
    state.connected = true;

    // Set up frame arrival callback
    let next_frame = state.next_frame.clone();
    state.appsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |appsink| {
                let sample = appsink.pull_sample().map_err(|_| gst::FlowError::Flushing)?;
                let mut buffer = sample.buffer_owned().unwrap();

                // Get GL context and set sync point
                {
                    let context = match (buffer.n_memory() > 0)
                        .then(|| buffer.peek_memory(0))
                        .and_then(|m| m.downcast_memory_ref::<gst_gl::GLBaseMemory>())
                        .map(|m| m.context())
                    {
                        Some(context) => context.clone(),
                        None => {
                            return Err(gst::FlowError::Error);
                        }
                    };

                    // Set sync point for GPU synchronization
                    if let Some(meta) = buffer.meta::<gst_gl::GLSyncMeta>() {
                        meta.set_sync_point(&context);
                    } else {
                        let buffer = buffer.make_mut();
                        let meta = gst_gl::GLSyncMeta::add(buffer, &context);
                        meta.set_sync_point(&context);
                    }
                }

                let info = sample
                    .caps()
                    .and_then(|caps| gst_video::VideoInfo::from_caps(caps).ok())
                    .ok_or(gst::FlowError::NotNegotiated)?;

                *next_frame.lock().unwrap() = Some((info, buffer));

                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );

    tracing::info!("GL context connected for video rendering");
    Ok(())
}

/// Fetch the next frame as a Slint image.
fn fetch_next_frame(state: &mut GLVideoState) -> Option<slint::Image> {
    if let Some((info, buffer)) = state.next_frame.lock().ok()?.take() {
        // Wait for GPU sync
        if let Some(sync_meta) = buffer.meta::<gst_gl::GLSyncMeta>() {
            if let Some(ref ctx) = state.gst_gl_context {
                sync_meta.wait(ctx);
            }
        }

        // Convert to GL video frame
        if let Ok(frame) = gst_gl::GLVideoFrame::from_buffer_readable(buffer, &info) {
            state.current_frame = Some(frame);
        }
    }

    // Get texture from current frame
    state.current_frame.as_ref().and_then(|frame| {
        let texture_id = frame.texture_id(0).ok()?;
        let texture: std::num::NonZeroU32 = texture_id.try_into().ok()?;
        let size = [frame.width(), frame.height()];

        Some(unsafe {
            slint::BorrowedOpenGLTextureBuilder::new_gl_2d_rgba_texture(texture, size.into())
                .build()
        })
    })
}

/// Start playing video from memory using GL rendering.
///
/// Returns the duration in seconds, or None if GL video is not available.
pub fn start_gl_video(data: Vec<u8>) -> Option<f64> {
    GL_VIDEO_STATE.with(|cell| {
        let mut state_ref = cell.borrow_mut();
        let state = state_ref.as_mut()?;

        if !state.connected {
            tracing::warn!("GL context not connected yet");
            return None;
        }

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

        // Create decodebin
        let decodebin = gst::ElementFactory::make("decodebin3")
            .build()
            .ok()?;

        // Add elements to pipeline
        pipeline.add_many([appsrc.upcast_ref(), &decodebin]).ok()?;
        appsrc.link(&decodebin).ok()?;

        // Handle decodebin pad-added
        let glsinkbin = state.glsinkbin.clone();
        let pipeline_weak = pipeline.downgrade();
        decodebin.connect_pad_added(move |_decodebin, src_pad| {
            let caps = src_pad.current_caps().or_else(|| Some(src_pad.query_caps(None)));
            if let Some(caps) = caps {
                let structure = caps.structure(0);
                if let Some(s) = structure {
                    let name = s.name().as_str();
                    if name.starts_with("video/") {
                        if let Some(pipeline) = pipeline_weak.upgrade() {
                            pipeline.add(&glsinkbin).ok();
                            glsinkbin.sync_state_with_parent().ok();
                            if let Some(sink_pad) = glsinkbin.static_pad("sink") {
                                src_pad.link(&sink_pad).ok();
                                tracing::info!("GL video sink linked");
                            }
                        }
                    }
                }
            }
        });

        // Set up bus handler for GL context messages
        if let Some(bus) = pipeline.bus() {
            let gst_gl_context = state.gst_gl_context.clone();
            bus.set_sync_handler(move |_, msg| {
                if let gst::MessageView::NeedContext(ctx) = msg.view() {
                    let ctx_type = ctx.context_type();

                    if ctx_type == "gst.gl.app_context" {
                        if let Some(element) = msg.src().and_then(|s| s.downcast_ref::<gst::Element>()) {
                            if let Some(ref gl_ctx) = gst_gl_context {
                                let mut gst_context = gst::Context::new(ctx_type, true);
                                {
                                    let gst_context = gst_context.get_mut().unwrap();
                                    let structure = gst_context.structure_mut();
                                    structure.set("context", gl_ctx);
                                }
                                element.set_context(&gst_context);
                            }
                        }
                    }
                }
                gst::BusSyncReply::Drop
            });
        }

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

        tracing::info!("GL video started, duration: {}s", duration);
        Some(duration)
    })
}

/// Stop GL video playback.
pub fn stop_gl_video() {
    GL_VIDEO_STATE.with(|cell| {
        if let Some(ref mut state) = *cell.borrow_mut() {
            if let Some(ref pipeline) = state.pipeline {
                let _ = pipeline.set_state(gst::State::Null);
            }
            state.pipeline = None;
            state.is_playing = false;
            state.current_frame = None;
            if let Ok(mut next) = state.next_frame.lock() {
                *next = None;
            }
        }
    });
}

/// Pause/resume GL video.
pub fn set_gl_video_playing(playing: bool) {
    GL_VIDEO_STATE.with(|cell| {
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

/// Get current GL video position in seconds.
pub fn get_gl_video_position() -> Option<f64> {
    GL_VIDEO_STATE.with(|cell| {
        let state_ref = cell.borrow();
        let state = state_ref.as_ref()?;
        let pipeline = state.pipeline.as_ref()?;
        pipeline
            .query_position::<gst::ClockTime>()
            .map(|p| p.mseconds() as f64 / 1000.0)
    })
}

/// Seek GL video to position in seconds.
pub fn seek_gl_video(position: f64) {
    GL_VIDEO_STATE.with(|cell| {
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

/// Set GL video volume (0.0 - 1.0).
pub fn set_gl_video_volume(volume: f64) {
    GL_VIDEO_STATE.with(|cell| {
        if let Some(ref state) = *cell.borrow() {
            if let Some(ref pipeline) = state.pipeline {
                // Find the audio sink and set volume
                if let Some(audio_sink) = pipeline.by_name("autoaudiosink0") {
                    audio_sink.set_property("volume", volume.clamp(0.0, 1.0));
                }
            }
        }
    });
}

/// Check if GL video is currently playing.
pub fn is_gl_video_active() -> bool {
    GL_VIDEO_STATE.with(|cell| {
        cell.borrow()
            .as_ref()
            .map(|s| s.pipeline.is_some() && s.is_playing)
            .unwrap_or(false)
    })
}

/// Get GL video duration in seconds.
pub fn get_gl_video_duration() -> Option<f64> {
    GL_VIDEO_STATE.with(|cell| {
        cell.borrow()
            .as_ref()
            .filter(|s| s.duration_ms > 0)
            .map(|s| s.duration_ms as f64 / 1000.0)
    })
}

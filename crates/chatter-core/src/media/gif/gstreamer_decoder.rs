//! GStreamer-based GIF decoder.
//!
//! Decodes every frame up front through a plain CPU pipeline
//! (`decodebin3 → videoconvert → RGBA appsink`), honoring the buffer stride.
//! Fast (~0.15s for a 93-frame GIF) and deterministic — the previous GL
//! pipeline produced black/garbage frames and has been removed.

use std::sync::Once;
use std::time::Duration;

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;

use super::GifFrame;
use crate::error::MediaError;

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

/// Decode ALL frames of a GIF up front via a plain CPU pipeline.
///
/// `appsrc → decodebin3 → videoconvert → RGBA appsink (sync=false)`. Pulls every
/// frame as fast as the CPU decodes (no real-time playback, no GL), and copies
/// each frame **honoring the buffer stride** — the GL path's stride-blind copy
/// is what produced black/garbage frames.
///
/// Measured ~0.15s for a 93-frame 1040×600 GIF (vs ~2.3s pure-Rust `image`,
/// ~0.85s real-time GL playback). Blocking — call off the UI thread.
pub fn decode_all_frames_cpu(gif_bytes: &[u8]) -> Result<Vec<GifFrame>, MediaError> {
    ensure_gst_init()?;

    if gif_bytes.len() < 6 || (&gif_bytes[0..6] != b"GIF89a" && &gif_bytes[0..6] != b"GIF87a") {
        return Err(MediaError::InvalidFormat("Not a GIF file".to_string()));
    }

    let pipeline = gst::Pipeline::new();
    let appsrc = gst_app::AppSrc::builder()
        .caps(&gst::Caps::builder("image/gif").build())
        .format(gst::Format::Bytes)
        .build();
    let decodebin = gst::ElementFactory::make("decodebin3")
        .build()
        .or_else(|_| gst::ElementFactory::make("decodebin").build())
        .map_err(|e| MediaError::PlaybackError(format!("decodebin: {e}")))?;
    let convert = gst::ElementFactory::make("videoconvert")
        .build()
        .map_err(|e| MediaError::PlaybackError(format!("videoconvert: {e}")))?;
    let caps = gst::Caps::builder("video/x-raw")
        .field("format", "RGBA")
        .build();
    let appsink = gst_app::AppSink::builder().caps(&caps).sync(false).build();

    pipeline
        .add_many([
            appsrc.upcast_ref(),
            &decodebin,
            &convert,
            appsink.upcast_ref(),
        ])
        .map_err(|e| MediaError::PlaybackError(format!("add: {e}")))?;
    appsrc
        .link(&decodebin)
        .map_err(|e| MediaError::PlaybackError(format!("link appsrc: {e}")))?;
    gst::Element::link_many([&convert, appsink.upcast_ref()])
        .map_err(|e| MediaError::PlaybackError(format!("link sink: {e}")))?;

    // decodebin pads appear dynamically — link to videoconvert.
    let convert_weak = convert.downgrade();
    decodebin.connect_pad_added(move |_, pad| {
        if let Some(c) = convert_weak.upgrade() {
            if let Some(sinkpad) = c.static_pad("sink") {
                if !sinkpad.is_linked() {
                    let _ = pad.link(&sinkpad);
                }
            }
        }
    });

    pipeline
        .set_state(gst::State::Playing)
        .map_err(|e| MediaError::PlaybackError(format!("play: {e}")))?;

    // Feed the whole GIF, then signal end of stream.
    let buffer = gst::Buffer::from_slice(gif_bytes.to_vec());
    appsrc
        .push_buffer(buffer)
        .map_err(|e| MediaError::PlaybackError(format!("push: {e:?}")))?;
    let _ = appsrc.end_of_stream();

    let mut frames = Vec::new();
    loop {
        match appsink.try_pull_sample(gst::ClockTime::from_seconds(5)) {
            Some(sample) => {
                let Some(buffer) = sample.buffer() else {
                    continue;
                };
                let Some(caps) = sample.caps() else { continue };
                let info = gst_video::VideoInfo::from_caps(caps)
                    .map_err(|e| MediaError::PlaybackError(format!("caps: {e}")))?;
                let map = buffer
                    .map_readable()
                    .map_err(|e| MediaError::PlaybackError(format!("map: {e}")))?;
                let raw = map.as_slice();

                let w = info.width() as usize;
                let h = info.height() as usize;
                let stride = info.stride()[0] as usize;
                let row_bytes = w * 4;

                // Copy row-by-row to strip stride padding into tight RGBA.
                let mut data = vec![0u8; row_bytes * h];
                for y in 0..h {
                    let src = y * stride;
                    let dst = y * row_bytes;
                    if src + row_bytes <= raw.len() {
                        data[dst..dst + row_bytes].copy_from_slice(&raw[src..src + row_bytes]);
                    }
                }

                let delay = buffer
                    .duration()
                    .map(|d| Duration::from_nanos(d.nseconds()))
                    .filter(|d| !d.is_zero())
                    .unwrap_or(Duration::from_millis(100));

                frames.push(GifFrame {
                    data,
                    width: w as u32,
                    height: h as u32,
                    delay,
                });
            }
            None => {
                if appsink.is_eos() {
                    break;
                }
                pipeline.set_state(gst::State::Null).ok();
                return Err(MediaError::PlaybackError(
                    "timed out pulling frames".to_string(),
                ));
            }
        }
    }

    pipeline.set_state(gst::State::Null).ok();
    Ok(frames)
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

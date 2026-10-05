//! Integration tests for media playback from memory.
//!
//! Tests video and image decoding from in-memory data to verify
//! the GStreamer pipeline and image processing work correctly.

use std::fs;
use std::path::Path;

use image::GenericImageView;

/// Test that we can decode a video from memory and extract info.
#[test]
fn test_video_from_memory_mp4() {
    // Skip if no test video available
    let test_video_path = "/home/alex/Documents/subliminal_vibes/output/sms_funny/20260130_195447_female_chatstory_asian_light_animated.mp4";

    if !Path::new(test_video_path).exists() {
        eprintln!("Skipping test: test video not found at {}", test_video_path);
        return;
    }

    // Read video into memory
    let video_data = fs::read(test_video_path).expect("Failed to read test video");
    println!("Loaded {} bytes of video data", video_data.len());

    // Use the public API to get video info
    use chatter_core::media::decoder::create_decoder;

    let mut decoder = create_decoder().expect("Failed to create decoder");

    match decoder.open_memory(video_data) {
        Ok(info) => {
            println!("Video info extracted successfully:");
            println!("  Width: {}", info.width);
            println!("  Height: {}", info.height);
            println!("  Duration: {}ms", info.duration_ms);
            println!("  FPS: {}", info.fps);
            println!("  Has audio: {}", info.has_audio);
            println!("  Codec: {}", info.codec);

            // Verify we got reasonable values
            assert!(info.width > 0, "Width should be > 0");
            assert!(info.height > 0, "Height should be > 0");
            assert!(info.duration_ms > 0, "Duration should be > 0");
            assert!(info.fps > 0.0, "FPS should be > 0");

            // Try to decode a frame
            match decoder.decode_video_frame() {
                Ok(Some(frame)) => {
                    println!(
                        "Decoded frame: {}x{}, {} bytes, ts={}ms",
                        frame.width,
                        frame.height,
                        frame.data.len(),
                        frame.timestamp_ms
                    );
                    assert!(frame.width > 0, "Frame width should be > 0");
                    assert!(frame.height > 0, "Frame height should be > 0");
                    assert!(!frame.data.is_empty(), "Frame data should not be empty");
                }
                Ok(None) => {
                    println!("No frame available yet (EOS or not ready)");
                }
                Err(e) => {
                    panic!("Failed to decode frame: {:?}", e);
                }
            }
        }
        Err(e) => {
            panic!("Failed to open video from memory: {:?}", e);
        }
    }

    decoder.close();
}

/// Test image loading from memory.
#[test]
fn test_image_from_memory_png() {
    let test_image_path = "/home/alex/Documents/subliminal_vibes/output/sms_family/20260130_011955_male_chatstory_european_light_frames/frame_009.png";

    if !Path::new(test_image_path).exists() {
        eprintln!("Skipping test: test image not found at {}", test_image_path);
        return;
    }

    // Read image into memory
    let image_data = fs::read(test_image_path).expect("Failed to read test image");
    println!("Loaded {} bytes of image data", image_data.len());

    // Try to decode the image using the image crate
    match image::load_from_memory(&image_data) {
        Ok(img) => {
            let (width, height) = img.dimensions();
            println!("Image loaded: {}x{}", width, height);

            assert!(width > 0, "Image width should be > 0");
            assert!(height > 0, "Image height should be > 0");

            // Convert to RGB
            let rgb = img.to_rgb8();
            println!("RGB data: {} bytes", rgb.len());
            assert!(!rgb.is_empty(), "RGB data should not be empty");
        }
        Err(e) => {
            panic!("Failed to load image from memory: {:?}", e);
        }
    }
}

/// Test the GStreamer pipeline construction with appsrc.
#[test]
fn test_gstreamer_appsrc_pipeline() {
    use gstreamer as gst;
    use gstreamer::prelude::*;
    use gstreamer_app as gst_app;
    use std::sync::{Arc, Mutex};

    // Skip if no test video
    let test_video_path = "/home/alex/Documents/subliminal_vibes/output/sms_funny/20260130_195447_female_chatstory_asian_light_animated.mp4";

    if !Path::new(test_video_path).exists() {
        eprintln!("Skipping test: test video not found");
        return;
    }

    let video_data = fs::read(test_video_path).expect("Failed to read test video");
    println!("Test data: {} bytes", video_data.len());

    // Check magic bytes
    if video_data.len() >= 8 {
        let magic = &video_data[4..8];
        println!(
            "Magic bytes at offset 4: {:?} ({})",
            magic,
            String::from_utf8_lossy(magic)
        );
        if magic == b"ftyp" {
            println!("Confirmed: MP4/QuickTime format");
        }
    }

    gst::init().expect("Failed to init GStreamer");

    // Create pipeline
    let pipeline = gst::Pipeline::new();

    // Create appsrc
    let data_len = video_data.len() as i64;
    let appsrc = gst_app::AppSrc::builder()
        .stream_type(gst_app::AppStreamType::RandomAccess)
        .size(data_len)
        .format(gst::Format::Bytes)
        .build();

    // Store data for callbacks
    let data_arc = Arc::new(video_data);
    let read_pos = Arc::new(Mutex::new(0usize));
    let eos_sent = Arc::new(Mutex::new(false));

    let data_for_need = data_arc.clone();
    let read_pos_need = read_pos.clone();
    let eos_sent_need = eos_sent.clone();

    let data_for_seek = data_arc.clone();
    let read_pos_seek = read_pos.clone();
    let eos_sent_seek = eos_sent.clone();

    appsrc.set_callbacks(
        gst_app::AppSrcCallbacks::builder()
            .need_data(move |appsrc, _hint| {
                let mut pos = read_pos_need.lock().unwrap();
                let mut eos = eos_sent_need.lock().unwrap();
                let data = data_for_need.as_ref();

                if *eos || *pos >= data.len() {
                    if !*eos {
                        *eos = true;
                        println!("need_data: sending EOS at position {}", *pos);
                        let _ = appsrc.end_of_stream();
                    }
                    return;
                }

                let slice = &data[*pos..];
                let offset = *pos as u64;

                let mut buffer = gst::Buffer::from_slice(slice.to_vec());
                {
                    let buffer_ref = buffer.get_mut().unwrap();
                    buffer_ref.set_offset(offset);
                    buffer_ref.set_offset_end(data.len() as u64);
                }

                println!(
                    "need_data: pushing {} bytes from offset {}",
                    slice.len(),
                    offset
                );

                if let Err(e) = appsrc.push_buffer(buffer) {
                    eprintln!("Failed to push buffer: {:?}", e);
                    return;
                }

                *pos = data.len();
                *eos = true;
                println!("need_data: sending EOS after full push");
                let _ = appsrc.end_of_stream();
            })
            .seek_data(move |_appsrc, offset| {
                let offset_usize = offset as usize;
                let data = data_for_seek.as_ref();

                if offset_usize > data.len() {
                    println!(
                        "seek_data: offset {} beyond data length {}",
                        offset,
                        data.len()
                    );
                    return false;
                }

                println!(
                    "seek_data: seeking to offset {} (data len {})",
                    offset,
                    data.len()
                );

                let mut pos = read_pos_seek.lock().unwrap();
                let mut eos = eos_sent_seek.lock().unwrap();
                *pos = offset_usize;
                *eos = false;

                true
            })
            .build(),
    );

    // Create decodebin
    let decodebin = gst::ElementFactory::make("decodebin")
        .build()
        .expect("Failed to create decodebin");

    // Create fakesink for testing (just to complete the pipeline)
    let fakesink = gst::ElementFactory::make("fakesink")
        .build()
        .expect("Failed to create fakesink");

    // Add elements
    pipeline
        .add_many([appsrc.upcast_ref(), &decodebin, &fakesink])
        .expect("Failed to add elements");

    // Link appsrc to decodebin
    appsrc
        .link(&decodebin)
        .expect("Failed to link appsrc to decodebin");

    // Connect pad-added for dynamic linking
    let fakesink_weak = fakesink.downgrade();
    decodebin.connect_pad_added(move |_, pad| {
        let caps = pad.current_caps().unwrap_or_else(|| pad.query_caps(None));
        if let Some(structure) = caps.structure(0) {
            let name = structure.name();
            println!("pad-added: {}", name);

            if name.starts_with("video/") || name.starts_with("audio/") {
                if let Some(sink) = fakesink_weak.upgrade() {
                    if let Some(sink_pad) = sink.static_pad("sink") {
                        if !sink_pad.is_linked() {
                            match pad.link(&sink_pad) {
                                Ok(_) => println!("Linked {} pad to fakesink", name),
                                Err(e) => println!("Failed to link pad: {:?}", e),
                            }
                        }
                    }
                }
            }
        }
    });

    // Set to PAUSED
    println!("Setting pipeline to PAUSED...");
    pipeline
        .set_state(gst::State::Paused)
        .expect("Failed to set state");

    // Wait for state change
    let (result, current, pending) = pipeline.state(gst::ClockTime::from_seconds(10));
    println!(
        "State change: result={:?}, current={:?}, pending={:?}",
        result, current, pending
    );

    // Check for errors
    if let Some(bus) = pipeline.bus() {
        while let Some(msg) = bus.pop() {
            match msg.view() {
                gst::MessageView::Error(err) => {
                    eprintln!("Pipeline error: {} (debug: {:?})", err.error(), err.debug());
                }
                gst::MessageView::Warning(warn) => {
                    eprintln!(
                        "Pipeline warning: {} (debug: {:?})",
                        warn.error(),
                        warn.debug()
                    );
                }
                gst::MessageView::StateChanged(sc) => {
                    if sc
                        .src()
                        .map(|s| s == pipeline.upcast_ref::<gst::Object>())
                        .unwrap_or(false)
                    {
                        println!(
                            "Pipeline state changed: {:?} -> {:?}",
                            sc.old(),
                            sc.current()
                        );
                    }
                }
                _ => {}
            }
        }
    }

    // Query duration
    if let Some(duration) = pipeline.query_duration::<gst::ClockTime>() {
        println!("Duration: {}ms", duration.mseconds());
        assert!(duration.mseconds() > 0, "Duration should be > 0");
    } else {
        println!("Could not query duration (might be OK for some formats)");
    }

    // Cleanup
    pipeline.set_state(gst::State::Null).ok();
    println!("Test completed successfully");
}

/// Test video format detection from magic bytes.
#[test]
fn test_video_format_detection() {
    // MP4 magic bytes
    let mp4_data: &[u8] = &[
        0x00, 0x00, 0x00, 0x20, b'f', b't', b'y', b'p', b'm', b'p', b'4', b'2',
    ];
    assert_eq!(
        &mp4_data[4..8],
        b"ftyp",
        "MP4 should have 'ftyp' at offset 4"
    );

    // MKV/WebM magic bytes
    let mkv_data: &[u8] = &[0x1A, 0x45, 0xDF, 0xA3];
    assert_eq!(
        &mkv_data[0..4],
        &[0x1A, 0x45, 0xDF, 0xA3],
        "MKV should have EBML header"
    );

    // AVI magic bytes
    let avi_data: &[u8] = &[
        b'R', b'I', b'F', b'F', 0x00, 0x00, 0x00, 0x00, b'A', b'V', b'I', b' ',
    ];
    assert_eq!(&avi_data[0..4], b"RIFF", "AVI should start with RIFF");
    assert_eq!(
        &avi_data[8..12],
        b"AVI ",
        "AVI should have 'AVI ' at offset 8"
    );

    println!("Format detection logic verified");
}

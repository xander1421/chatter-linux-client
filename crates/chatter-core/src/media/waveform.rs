//! Audio waveform extraction.
//!
//! Extracts audio energy from audio files for visualization.
//! Decodes voice-note WAV directly on all platforms and other desktop formats
//! with GStreamer to avoid FFmpeg symbol conflicts.

use std::path::Path;

#[cfg(feature = "desktop-gstreamer")]
use gstreamer as gst;
#[cfg(feature = "desktop-gstreamer")]
use gstreamer::prelude::*;
#[cfg(feature = "desktop-gstreamer")]
use gstreamer_app as gst_app;
#[cfg(feature = "desktop-gstreamer")]
use gstreamer_pbutils as gst_pbutils;

use crate::error::MediaError;

/// Number of bars in waveform visualization.
pub const WAVEFORM_BAR_COUNT: usize = 32;

/// Missing, constant, or floor-level bars cannot represent the note's dynamics.
/// Re-extract from authenticated audio instead of trusting old absolute-scale metadata.
pub(crate) fn needs_waveform_repair(samples: Option<&[u8]>) -> bool {
    let Some(samples) = samples.filter(|samples| !samples.is_empty()) else {
        return true;
    };
    samples.iter().all(|sample| *sample == samples[0]) || samples.iter().all(|sample| *sample <= 13)
}

/// Extract waveform bars from an audio file.
///
/// Returns a vector of amplitude values (0-255) representing the waveform.
/// The number of values equals `WAVEFORM_BAR_COUNT` (32).
///
/// # Arguments
///
/// * `path` - Path to the audio file
///
/// # Errors
///
/// Returns an error if the file cannot be read or decoded.
pub fn extract_waveform(path: &Path) -> Result<Vec<u8>, MediaError> {
    extract_waveform_with_bars(path, WAVEFORM_BAR_COUNT)
}

/// Extract waveform bars with a custom number of bars.
pub fn extract_waveform_with_bars(path: &Path, num_bars: usize) -> Result<Vec<u8>, MediaError> {
    if num_bars == 0 {
        return Ok(Vec::new());
    }

    #[cfg(feature = "desktop-gstreamer")]
    {
        gst::init()
            .map_err(|e| MediaError::InvalidFormat(format!("GStreamer init failed: {}", e)))?;
        let uri = format!("file://{}", path.display());
        extract_waveform_from_uri(&uri, num_bars)
    }
    #[cfg(not(feature = "desktop-gstreamer"))]
    {
        let data = std::fs::read(path).map_err(|e| MediaError::InvalidFormat(e.to_string()))?;
        extract_waveform_from_memory_with_bars(&data, num_bars)
    }
}

/// Extract waveform peaks from in-memory audio data.
///
/// # Arguments
///
/// * `data` - Audio data bytes (MP3, WAV, OGG, etc.)
///
/// # Errors
///
/// Returns an error if audio cannot be decoded.
pub fn extract_waveform_from_memory(data: &[u8]) -> Result<Vec<u8>, MediaError> {
    extract_waveform_from_memory_with_bars(data, WAVEFORM_BAR_COUNT)
}

/// Extract waveform peaks from memory with custom bar count.
pub fn extract_waveform_from_memory_with_bars(
    data: &[u8],
    num_bars: usize,
) -> Result<Vec<u8>, MediaError> {
    if num_bars == 0 {
        return Ok(Vec::new());
    }

    // Voice notes are PCM WAV on every platform. Decode them without a
    // platform media backend, including Android builds without GStreamer.
    if data.starts_with(b"RIFF") && data.get(8..12) == Some(b"WAVE") {
        let mut reader = hound::WavReader::new(std::io::Cursor::new(data))
            .map_err(|e| MediaError::InvalidFormat(e.to_string()))?;
        let spec = reader.spec();
        let samples = match spec.sample_format {
            hound::SampleFormat::Float => reader.samples::<f32>().collect::<Result<Vec<_>, _>>(),
            hound::SampleFormat::Int => {
                let scale = 2_f32.powi(i32::from(spec.bits_per_sample) - 1);
                reader
                    .samples::<i32>()
                    .map(|s| s.map(|s| s as f32 / scale))
                    .collect::<Result<Vec<_>, _>>()
            }
        }
        .map_err(|e| MediaError::InvalidFormat(e.to_string()))?;
        return process_samples_to_bars(
            samples,
            num_bars,
            spec.sample_rate as usize * spec.channels as usize,
        );
    }
    #[cfg(feature = "desktop-gstreamer")]
    {
        gst::init()
            .map_err(|e| MediaError::InvalidFormat(format!("GStreamer init failed: {}", e)))?;
        extract_waveform_from_data(data, num_bars)
    }
    #[cfg(not(feature = "desktop-gstreamer"))]
    Err(MediaError::InvalidFormat(
        "Waveform extraction requires PCM WAV on this platform".into(),
    ))
}

/// Extract waveform from in-memory data using appsrc.
#[cfg(feature = "desktop-gstreamer")]
fn extract_waveform_from_data(data: &[u8], num_bars: usize) -> Result<Vec<u8>, MediaError> {
    // Create pipeline: appsrc -> decodebin -> audioconvert -> audioresample -> appsink
    let pipeline = gst::Pipeline::new();

    // Create appsrc for feeding data
    let appsrc = gst_app::AppSrc::builder()
        .stream_type(gst_app::AppStreamType::RandomAccess)
        .size(data.len() as i64)
        .build();

    // Create decodebin for automatic demuxing and decoding
    let decodebin = gst::ElementFactory::make("decodebin")
        .build()
        .map_err(|e| MediaError::InvalidFormat(format!("Failed to create decodebin: {}", e)))?;

    let convert = gst::ElementFactory::make("audioconvert")
        .build()
        .map_err(|e| MediaError::InvalidFormat(format!("Failed to create audioconvert: {}", e)))?;

    let resample = gst::ElementFactory::make("audioresample")
        .build()
        .map_err(|e| MediaError::InvalidFormat(format!("Failed to create audioresample: {}", e)))?;

    // Configure appsink for mono float samples
    let caps = gst::Caps::builder("audio/x-raw")
        .field("format", "F32LE")
        .field("channels", 1i32)
        .field("rate", 8000i32) // Low sample rate is fine for waveform
        .build();

    let sink = gst_app::AppSink::builder().caps(&caps).build();

    pipeline
        .add_many([
            appsrc.upcast_ref(),
            &decodebin,
            &convert,
            &resample,
            sink.upcast_ref(),
        ])
        .map_err(|e| MediaError::InvalidFormat(format!("Failed to add elements: {}", e)))?;

    // Link appsrc to decodebin
    appsrc.link(&decodebin).map_err(|e| {
        MediaError::InvalidFormat(format!("Failed to link appsrc to decodebin: {}", e))
    })?;

    // Link convert -> resample -> sink (static linking)
    gst::Element::link_many([&convert, &resample, sink.upcast_ref()])
        .map_err(|e| MediaError::InvalidFormat(format!("Failed to link elements: {}", e)))?;

    // Connect pad-added signal for dynamic linking from decodebin
    let convert_weak = convert.downgrade();
    decodebin.connect_pad_added(move |_, pad| {
        let Some(convert) = convert_weak.upgrade() else {
            return;
        };

        // Only link audio pads
        let caps = pad.current_caps().unwrap_or_else(|| pad.query_caps(None));
        if let Some(structure) = caps.structure(0) {
            let name = structure.name();
            if name.starts_with("audio/") {
                if let Some(sink_pad) = convert.static_pad("sink") {
                    if !sink_pad.is_linked() {
                        let _ = pad.link(&sink_pad);
                    }
                }
            }
        }
    });

    // Push data to appsrc
    let buffer = gst::Buffer::from_slice(data.to_vec());
    appsrc.push_buffer(buffer).map_err(|e| {
        MediaError::InvalidFormat(format!("Failed to push buffer to appsrc: {:?}", e))
    })?;
    appsrc
        .end_of_stream()
        .map_err(|e| MediaError::InvalidFormat(format!("Failed to signal EOS: {:?}", e)))?;

    // Start pipeline
    pipeline
        .set_state(gst::State::Playing)
        .map_err(|e| MediaError::InvalidFormat(format!("Failed to start pipeline: {}", e)))?;

    // Collect all samples
    let mut all_samples: Vec<f32> = Vec::new();

    loop {
        match sink.try_pull_sample(gst::ClockTime::from_seconds(5)) {
            Some(sample) => {
                if let Some(buffer) = sample.buffer() {
                    if let Ok(map) = buffer.map_readable() {
                        let bytes = map.as_slice();
                        for chunk in bytes.chunks_exact(4) {
                            let sample =
                                f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                            all_samples.push(sample);
                        }
                    }
                }
            }
            None => {
                if sink.is_eos() {
                    break;
                }
                // Check for errors
                if let Some(bus) = pipeline.bus() {
                    while let Some(msg) = bus.pop() {
                        match msg.view() {
                            gst::MessageView::Error(err) => {
                                pipeline.set_state(gst::State::Null).ok();
                                return Err(MediaError::InvalidFormat(format!(
                                    "Decoding error: {}",
                                    err.error()
                                )));
                            }
                            gst::MessageView::Eos(_) => break,
                            _ => {}
                        }
                    }
                }
                break;
            }
        }
    }

    // Stop pipeline
    pipeline.set_state(gst::State::Null).ok();

    // Process samples into waveform bars
    process_samples_to_bars(all_samples, num_bars, 8000)
}

/// Extract waveform from a URI using GStreamer with seek-based sampling.
///
/// Instead of decoding the entire file (which takes minutes for large files),
/// we seek to `num_bars` positions and sample a small chunk at each position.
/// This reduces a 5-minute decode to ~1 second.
#[cfg(feature = "desktop-gstreamer")]
fn extract_waveform_from_uri(uri: &str, num_bars: usize) -> Result<Vec<u8>, MediaError> {
    // First, query the duration using playbin
    let duration_ns = query_audio_duration(uri)?;

    if duration_ns == 0 {
        return Err(MediaError::InvalidFormat("Unknown audio duration".into()));
    }

    // Calculate sample positions (evenly distributed across the file)
    // We sample ~50ms of audio at each of the num_bars positions
    let step_ns = duration_ns / num_bars as u64;

    // Create pipeline for seeking and sampling
    let pipeline = gst::Pipeline::new();

    let src = gst::ElementFactory::make("uridecodebin")
        .property("uri", uri)
        .build()
        .map_err(|e| MediaError::InvalidFormat(format!("Failed to create uridecodebin: {}", e)))?;

    let convert = gst::ElementFactory::make("audioconvert")
        .build()
        .map_err(|e| MediaError::InvalidFormat(format!("Failed to create audioconvert: {}", e)))?;

    let resample = gst::ElementFactory::make("audioresample")
        .build()
        .map_err(|e| MediaError::InvalidFormat(format!("Failed to create audioresample: {}", e)))?;

    // Configure appsink for mono float samples at low sample rate
    let caps = gst::Caps::builder("audio/x-raw")
        .field("format", "F32LE")
        .field("channels", 1i32)
        .field("rate", 8000i32)
        .build();

    let sink = gst_app::AppSink::builder().caps(&caps).build();

    pipeline
        .add_many([&src, &convert, &resample, sink.upcast_ref()])
        .map_err(|e| MediaError::InvalidFormat(format!("Failed to add elements: {}", e)))?;

    gst::Element::link_many([&convert, &resample, sink.upcast_ref()])
        .map_err(|e| MediaError::InvalidFormat(format!("Failed to link elements: {}", e)))?;

    // Connect pad-added signal for dynamic linking
    let convert_weak = convert.downgrade();
    src.connect_pad_added(move |_, pad| {
        let Some(convert) = convert_weak.upgrade() else {
            return;
        };
        let caps = pad.current_caps().unwrap_or_else(|| pad.query_caps(None));
        if let Some(structure) = caps.structure(0) {
            if structure.name().starts_with("audio/") {
                if let Some(sink_pad) = convert.static_pad("sink") {
                    if !sink_pad.is_linked() {
                        let _ = pad.link(&sink_pad);
                    }
                }
            }
        }
    });

    // Start in PAUSED to allow seeking
    pipeline
        .set_state(gst::State::Paused)
        .map_err(|e| MediaError::InvalidFormat(format!("Failed to pause pipeline: {}", e)))?;

    // Wait for PAUSED state (max 5 seconds) to allow seeking
    let _ = pipeline.state(gst::ClockTime::from_seconds(5));

    let mut energies: Vec<f32> = Vec::with_capacity(num_bars);
    let mut reference_energies = Vec::with_capacity(num_bars);

    // Sample at each position
    for i in 0..num_bars {
        let seek_pos_ns = (i as u64) * step_ns;
        let seek_pos = gst::ClockTime::from_nseconds(seek_pos_ns);

        // Seek to position
        let seek_result =
            pipeline.seek_simple(gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT, seek_pos);

        if seek_result.is_err() {
            energies.push(0.0); // No decoded samples at this position
            continue;
        }

        // Play briefly to get samples
        pipeline.set_state(gst::State::Playing).ok();

        // Collect samples for this chunk (50ms worth)
        let mut sum_squares = 0.0_f64;
        let samples_needed = 400; // 50ms at 8kHz
        let mut samples_collected = 0;

        let timeout = gst::ClockTime::from_mseconds(200);
        while samples_collected < samples_needed {
            match sink.try_pull_sample(timeout) {
                Some(sample) => {
                    if let Some(buffer) = sample.buffer() {
                        if let Ok(map) = buffer.map_readable() {
                            let bytes = map.as_slice();
                            for chunk in bytes.chunks_exact(4) {
                                let sample =
                                    f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                                let sample = f64::from(sample);
                                sum_squares += sample * sample;
                                samples_collected += 1;
                                if samples_collected >= samples_needed {
                                    break;
                                }
                            }
                        }
                    }
                }
                None => break, // Timeout or EOS
            }
        }

        // Pause for next seek
        pipeline.set_state(gst::State::Paused).ok();

        let energy = if samples_collected == 0 {
            0.0
        } else {
            (sum_squares / f64::from(samples_collected)).sqrt() as f32
        };
        energies.push(energy);
        if seek_pos_ns >= 100_000_000 && duration_ns.saturating_sub(seek_pos_ns) > 100_000_000 {
            reference_energies.push(energy);
        }
    }

    // Cleanup
    pipeline.set_state(gst::State::Null).ok();

    Ok(normalize_energies(&energies, &reference_energies))
}

/// Query audio duration without decoding.
#[cfg(feature = "desktop-gstreamer")]
fn query_audio_duration(uri: &str) -> Result<u64, MediaError> {
    let discoverer = gst_pbutils::Discoverer::new(gst::ClockTime::from_seconds(10))
        .map_err(|e| MediaError::InvalidFormat(format!("Failed to create discoverer: {}", e)))?;

    match discoverer.discover_uri(uri) {
        Ok(info) => {
            let duration = info.duration().map(|d| d.nseconds()).unwrap_or(0);
            Ok(duration)
        }
        Err(e) => {
            tracing::warn!("Failed to discover URI {}: {}", uri, e);
            Ok(0) // Return 0 duration on error, caller will use default waveform
        }
    }
}

/// Normalize against the 95th percentile of the interior bars, so one tap
/// cannot flatten speech. Do not boost energy below the absolute noise floor.
fn normalize_energies(energies: &[f32], reference_energies: &[f32]) -> Vec<u8> {
    const NOISE_FLOOR: f32 = 0.002;
    let mut sorted: Vec<f32> = reference_energies
        .iter()
        .copied()
        .filter(|energy| energy.is_finite() && *energy >= NOISE_FLOOR)
        .collect();
    if sorted.is_empty() {
        sorted.extend(
            energies
                .iter()
                .copied()
                .filter(|energy| energy.is_finite() && *energy >= NOISE_FLOOR),
        );
    }
    sorted.sort_by(f32::total_cmp);
    let reference = sorted
        .get(sorted.len().saturating_sub(1) * 95 / 100)
        .copied()
        .unwrap_or(0.0);
    energies
        .iter()
        .map(|&energy| {
            if reference >= NOISE_FLOOR && energy >= NOISE_FLOOR && energy.is_finite() {
                ((energy / reference * 255.0).round() as u8).max(13)
            } else {
                13
            }
        })
        .collect()
}

pub(crate) fn waveform_from_pcm(samples: &[i16], samples_per_second: usize) -> Vec<u8> {
    sample_bars(samples.len(), WAVEFORM_BAR_COUNT, samples_per_second, |i| {
        samples[i] as f32 / 32768.0
    })
}

pub(crate) fn waveform_from_recorded_f32(
    samples: &[f32],
    samples_per_second: usize,
) -> Vec<u8> {
    sample_bars(samples.len(), WAVEFORM_BAR_COUNT, samples_per_second, |i| {
        let pcm = (samples[i].clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
        pcm as f32 / 32768.0
    })
}

/// Process decoded samples without quantizing quiet audio before normalization.
fn process_samples_to_bars(
    samples: Vec<f32>,
    num_bars: usize,
    samples_per_second: usize,
) -> Result<Vec<u8>, MediaError> {
    if samples.is_empty() {
        return Err(MediaError::InvalidFormat("No audio samples decoded".into()));
    }
    Ok(sample_bars(
        samples.len(),
        num_bars,
        samples_per_second,
        |i| samples[i],
    ))
}

fn sample_bars(
    sample_count: usize,
    num_bars: usize,
    samples_per_second: usize,
    sample: impl Fn(usize) -> f32,
) -> Vec<u8> {
    if sample_count == 0 {
        return vec![13; num_bars];
    }
    let edge = if sample_count > samples_per_second / 5 {
        samples_per_second / 10
    } else {
        0
    };
    let mut energies = Vec::with_capacity(num_bars);
    let mut reference_energies = Vec::with_capacity(num_bars);
    for i in 0..num_bars {
        let start = i * sample_count / num_bars;
        let end = ((i + 1) * sample_count / num_bars)
            .max(start + 1)
            .min(sample_count);
        if start == end {
            energies.push(0.0);
            continue;
        }
        energies.push(rms(start, end, &sample));
        let interior_start = start.max(edge);
        let interior_end = end.min(sample_count - edge);
        if interior_start < interior_end {
            reference_energies.push(rms(interior_start, interior_end, &sample));
        }
    }
    normalize_energies(&energies, &reference_energies)
}

fn rms(start: usize, end: usize, sample: &impl Fn(usize) -> f32) -> f32 {
    let sum_squares: f64 = (start..end)
        .map(|i| {
            let value = f64::from(sample(i));
            value * value
        })
        .sum();
    (sum_squares / (end - start) as f64).sqrt() as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recorded_float_waveform_matches_encoded_pcm() {
        let samples = [-1.0, -0.25, 0.0, 0.125, 0.75, 1.0];
        let pcm: Vec<i16> = samples
            .iter()
            .map(|sample| (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)
            .collect();
        assert_eq!(waveform_from_recorded_f32(&samples, 8000), waveform_from_pcm(&pcm, 8000));
    }

    #[test]
    fn repair_detects_flat_and_absolute_scale_metadata() {
        assert!(needs_waveform_repair(None));
        assert!(needs_waveform_repair(Some(&[])));
        assert!(needs_waveform_repair(Some(&[80; 32])));
        assert!(needs_waveform_repair(Some(&[0, 1, 3, 13])));
        assert!(!needs_waveform_repair(Some(&[13, 128, 255])));
    }

    #[test]
    fn normalization_preserves_speech_shape_and_silence_floor() {
        assert_eq!(
            normalize_energies(&[0.0, 0.00001, 0.00002], &[0.0, 0.00001, 0.00002]),
            vec![13; 3]
        );
        assert_eq!(
            normalize_energies(&[0.0, 0.05, 0.1], &[0.0, 0.05, 0.1]),
            vec![13, 255, 255]
        );
        assert_eq!(normalize_energies(&[0.0; 32], &[0.0; 32]), vec![13; 32]);
    }

    #[test]
    fn wav_extraction_matches_recording_and_includes_last_sample() {
        let samples: Vec<i16> = (0..65)
            .map(|i| if i == 64 { -20000 } else { i * 200 })
            .collect();
        let mut bytes = std::io::Cursor::new(Vec::new());
        {
            let spec = hound::WavSpec {
                channels: 1,
                sample_rate: 8000,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            };
            let mut writer = hound::WavWriter::new(&mut bytes, spec).unwrap();
            for sample in &samples {
                writer.write_sample(*sample).unwrap();
            }
            writer.finalize().unwrap();
        }
        let extracted = extract_waveform_from_memory(bytes.get_ref()).unwrap();
        assert_eq!(extracted, waveform_from_pcm(&samples, 8000));
        assert_eq!(extracted.len(), 32);
        assert_eq!(extracted[31], 255);
        assert!(extracted[0] < extracted[30]);
    }

    #[test]
    fn click_does_not_flatten_speech_bars() {
        let sample_rate = 8000;
        let mut samples = vec![0_i16; sample_rate * 2];
        for (i, sample) in samples.iter_mut().enumerate() {
            let speech = if i / 1000 % 2 == 0 { 1400 } else { 4200 };
            *sample = if i % 2 == 0 { speech } else { -speech };
        }
        samples[..sample_rate / 200].fill(i16::MAX); // 5 ms click at the start
        let sent = waveform_from_pcm(&samples, sample_rate);
        let received = process_samples_to_bars(
            samples
                .iter()
                .map(|&sample| sample as f32 / 32768.0)
                .collect(),
            WAVEFORM_BAR_COUNT,
            sample_rate,
        )
        .unwrap();
        assert_eq!(sent, received);
        assert!(sent[2..30].iter().all(|&bar| bar > 40));
        assert!(sent[2..30].iter().any(|&bar| bar > 180));
    }
}

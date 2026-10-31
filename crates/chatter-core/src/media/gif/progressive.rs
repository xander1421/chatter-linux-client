//! GIF first-frame decode for instant static preview.

use image::codecs::gif::GifDecoder;
use image::{AnimationDecoder, ImageFormat};
use std::io::Cursor;
use std::time::Duration;

use crate::error::MediaError;

use super::animator::GifFrame;

/// Source canvas admission applies before any GIF frame allocation, separately
/// from retained animation samples. Works with authenticated range readers.
pub fn bounded_gif_decoder<R: std::io::BufRead + std::io::Seek>(
    reader: R,
) -> Result<GifDecoder<R>, MediaError> {
    use image::ImageDecoder;
    let mut decoder =
        GifDecoder::new(reader).map_err(|e| MediaError::InvalidFormat(e.to_string()))?;
    let (w, h) = decoder.dimensions();
    if u64::from(w) * u64::from(h) > super::super::image_processor::PREVIEW_PIXELS {
        return Err(MediaError::InvalidFormat(
            "GIF source pixel budget exceeded".into(),
        ));
    }
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(super::super::image_processor::PREVIEW_ALLOC_BYTES);
    limits.max_image_width = Some(16384);
    limits.max_image_height = Some(16384);
    decoder
        .set_limits(limits)
        .map_err(|e| MediaError::InvalidFormat(e.to_string()))?;
    Ok(decoder)
}

/// Decode first frame only (for quick preview).
///
/// Much faster than full decode - only processes first frame.
pub fn decode_first_frame_only(bytes: &[u8]) -> Result<Option<GifFrame>, MediaError> {
    let format =
        image::guess_format(bytes).map_err(|e| MediaError::InvalidFormat(e.to_string()))?;

    if format != ImageFormat::Gif {
        return Ok(None);
    }

    let cursor = Cursor::new(bytes);
    let decoder = bounded_gif_decoder(cursor)?;

    let mut frames_iter = decoder.into_frames();

    match frames_iter.next() {
        Some(Ok(frame)) => {
            let delay = frame_delay(&frame);
            let buffer = frame.into_buffer();
            let (width, height) = buffer.dimensions();

            Ok(Some(GifFrame {
                data: buffer.into_raw(),
                width,
                height,
                delay,
            }))
        }
        Some(Err(e)) => Err(MediaError::InvalidFormat(format!("GIF frame error: {}", e))),
        None => Ok(None),
    }
}

/// Extract delay from an image frame.
fn frame_delay(frame: &image::Frame) -> Duration {
    let delay = frame.delay();
    let (numer, denom) = delay.numer_denom_ms();
    if denom == 0 {
        Duration::from_millis(100)
    } else {
        Duration::from_millis(super::animator::normalize_delay_ms(
            (numer as u64) / (denom as u64),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Note: These tests require actual GIF files
    // They're marked as ignored for CI

    #[test]
    fn test_decode_first_frame_non_gif() {
        // PNG header
        let png_bytes = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        let result = decode_first_frame_only(&png_bytes);
        assert!(result.is_err() || result.unwrap().is_none());
    }
}

#[cfg(test)]
mod source_budget_tests {
    use super::*;
    #[test]
    fn tiny_encoded_gif_cannot_allocate_an_unbounded_canvas() {
        let mut bytes = Vec::new();
        image::codecs::gif::GifEncoder::new(&mut bytes)
            .encode_frame(image::Frame::new(image::RgbaImage::new(1, 1)))
            .unwrap();
        bytes[6..8].copy_from_slice(&65535u16.to_le_bytes());
        bytes[8..10].copy_from_slice(&65535u16.to_le_bytes());
        let error = match bounded_gif_decoder(Cursor::new(&bytes)) {
            Ok(_) => panic!("oversized canvas admitted"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("pixel budget"), "{error}");
    }

    #[test]
    fn bounded_reader_preserves_frame_order_and_delays() {
        let mut bytes = Vec::new();
        {
            let mut encoder = image::codecs::gif::GifEncoder::new(&mut bytes);
            for color in [10, 80, 150] {
                let frame = image::Frame::from_parts(
                    image::RgbaImage::from_pixel(16, 8, image::Rgba([color, 0, 0, 255])),
                    0,
                    0,
                    image::Delay::from_numer_denom_ms(40, 1),
                );
                encoder.encode_frame(frame).unwrap();
            }
        }
        let frames: Vec<_> =
            bounded_gif_decoder(std::io::BufReader::with_capacity(7, Cursor::new(bytes)))
                .unwrap()
                .into_frames()
                .collect::<Result<_, _>>()
                .unwrap();
        assert_eq!(frames.len(), 3);
        for (frame, color) in frames.iter().zip([10, 80, 150]) {
            assert_eq!(frame.buffer().dimensions(), (16, 8));
            assert_eq!(frame.buffer().get_pixel(0, 0)[0], color);
            assert_eq!(frame.delay().numer_denom_ms(), (40, 1));
        }
    }
}

/// Decode a complete, display-sized loop before playback. Temporal thinning
/// preserves total duration while bounding retained RGBA memory.
pub fn decode_loop(
    bytes: &[u8],
    max_width: u32,
    max_height: u32,
    max_frames: usize,
    max_bytes: usize,
) -> Result<Vec<GifFrame>, MediaError> {
    #[cfg(feature = "desktop-gstreamer")]
    if super::gstreamer_decoder::is_gstreamer_gif_available() {
        match super::gstreamer_decoder::decode_loop_cpu(
            bytes, max_width, max_height, max_frames, max_bytes,
        ) {
            Ok(frames) if !frames.is_empty() => return Ok(frames),
            Ok(_) => {}
            Err(e) => tracing::debug!("Native GIF decode unavailable, using image decoder: {e}"),
        }
    }
    let decoder = bounded_gif_decoder(Cursor::new(bytes))?;
    let mut selected = super::DecimatedGif::default();
    for frame in decoder.into_frames() {
        let frame = frame.map_err(|e| MediaError::InvalidFormat(e.to_string()))?;
        let delay = frame_delay(&frame).as_millis() as u64;
        // Once thinned, discarded frames still contribute their duration but
        // need no resize or retained pixel allocation.
        if selected.seen % selected.stride != 0 {
            selected.seen += 1;
            if let Some(last) = selected.frames.last_mut() {
                last.delay_ms += delay;
            }
            continue;
        }
        let buffer = frame.into_buffer();
        let buffer = if buffer.width() > max_width || buffer.height() > max_height {
            image::DynamicImage::ImageRgba8(buffer)
                .thumbnail(max_width.max(1), max_height.max(1))
                .to_rgba8()
        } else {
            buffer
        };
        let (width, height) = buffer.dimensions();
        let data = buffer.into_raw();
        let bytes = data.len();
        selected = super::decimate_gif_frame(
            selected,
            (data, width, height),
            delay,
            bytes,
            max_frames,
            max_bytes,
        );
    }
    Ok(selected
        .frames
        .into_iter()
        .map(|sample| GifFrame {
            data: sample.frame.0,
            width: sample.frame.1,
            height: sample.frame.2,
            delay: Duration::from_millis(sample.delay_ms),
        })
        .collect())
}

#[cfg(test)]
mod loop_tests {
    use super::*;
    #[test]
    fn bounded_decode_preserves_normalized_loop_duration() {
        let mut encoded = Vec::new();
        {
            let mut encoder = image::codecs::gif::GifEncoder::new(&mut encoded);
            encoder.set_repeat(image::codecs::gif::Repeat::Infinite).unwrap();
            for delay in [10, 20, 100, 200] {
                let frame = image::Frame::from_parts(
                    image::RgbaImage::from_pixel(8, 4, image::Rgba([255, 0, 0, 255])),
                    0,
                    0,
                    image::Delay::from_numer_denom_ms(delay, 1),
                );
                encoder.encode_frame(frame).unwrap();
            }
        }
        let complete = decode_loop(&encoded, 4, 4, 10, 1024).unwrap();
        assert_eq!(
            complete
                .iter()
                .map(|f| f.delay.as_millis())
                .collect::<Vec<_>>(),
            vec![100, 20, 100, 200]
        );
        let frames = decode_loop(&encoded, 4, 4, 2, 64).unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!((frames[0].width, frames[0].height), (4, 2));
        assert_eq!(
            frames.iter().map(|f| f.delay.as_millis()).sum::<u128>(),
            420
        );
        assert!(frames.iter().map(|f| f.data.len()).sum::<usize>() <= 64);
    }
}

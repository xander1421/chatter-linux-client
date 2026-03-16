//! Slint image conversion utilities.
//!
//! Converts raw pixel data from core types into Slint Image for UI display.
//! These functions were extracted from core to keep it Slint-free.

use chatter_linux::calls::shared_frames::VideoFrameData;
use chatter_linux::error::MediaError;
use chatter_linux::media::gif::GifFrame;
use chatter_linux::media::ImageProcessor;

/// Convert RGBA8 pixel data to a Slint Image.
pub fn rgba_to_slint_image(data: &[u8], width: u32, height: u32) -> slint::Image {
    let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(data, width, height);
    slint::Image::from_rgba8(buffer)
}

/// Convert RGB8 pixel data to a Slint Image.
pub fn rgb_to_slint_image(width: u32, height: u32, rgb_data: &[u8]) -> slint::Image {
    let mut pixel_buffer = slint::SharedPixelBuffer::<slint::Rgb8Pixel>::new(width, height);
    let bytes = pixel_buffer.make_mut_bytes();
    bytes.copy_from_slice(rgb_data);
    slint::Image::from_rgb8(pixel_buffer)
}

/// Convert a DynamicImage to Slint Image for display.
pub fn dynamic_image_to_slint(image: &image::DynamicImage) -> slint::Image {
    let rgba = image.to_rgba8();
    let width = rgba.width();
    let height = rgba.height();
    let data = rgba.into_raw();
    slint::Image::from_rgba8(slint::SharedPixelBuffer::clone_from_slice(&data, width, height))
}

/// Load an image from memory bytes and convert directly to Slint Image.
pub fn memory_to_slint_image(data: &[u8]) -> Result<slint::Image, MediaError> {
    let image = ImageProcessor::load_from_memory(data)?;
    Ok(dynamic_image_to_slint(&image))
}

/// Convert a GIF frame to a Slint Image.
pub fn gif_frame_to_slint_image(frame: &GifFrame) -> slint::Image {
    rgba_to_slint_image(&frame.data, frame.width, frame.height)
}

/// Convert a VideoFrameData to a Slint Image.
pub fn video_frame_to_slint_image(frame: &VideoFrameData) -> slint::Image {
    rgba_to_slint_image(&frame.rgba_data, frame.width, frame.height)
}

//! Thumbnail format benchmark
//!
//! Run with: cargo run --release --example thumb_benchmark -- <image_path>

use image::{ImageEncoder, ImageReader};
use std::path::Path;
use std::time::Instant;

fn main() {
    let source = std::env::args()
        .nth(1)
        .expect("Usage: thumb_benchmark <image_path>");
    let path = Path::new(&source);

    println!("Loading image: {}", source);
    let image = ImageReader::open(path)
        .unwrap()
        .with_guessed_format()
        .unwrap()
        .decode()
        .unwrap();

    println!("Original size: {}x{}\n", image.width(), image.height());

    // Test different thumbnail sizes
    for size in [200u32, 300, 400] {
        println!("═══════════════════════════════════════════════════════════");
        println!("  THUMBNAIL SIZE: {}px", size);
        println!("═══════════════════════════════════════════════════════════");

        let thumb = image.thumbnail(size, size);
        let actual_size = format!("{}x{}", thumb.width(), thumb.height());
        println!("  Actual dimensions: {}\n", actual_size);

        let rgb = thumb.to_rgb8();
        let rgba = thumb.to_rgba8();

        println!(
            "  {:<20} {:>10} {:>12} {:>10}",
            "FORMAT", "SIZE", "TIME", "RATIO"
        );
        println!("  {}", "-".repeat(54));

        // JPEG Q85
        let (jpeg_bytes, jpeg_time) = bench_jpeg(&rgb, thumb.width(), thumb.height(), 85);
        print_result("JPEG (q85)", jpeg_bytes.len(), jpeg_time, jpeg_bytes.len());

        // JPEG Q75
        let (jpeg75_bytes, jpeg75_time) = bench_jpeg(&rgb, thumb.width(), thumb.height(), 75);
        print_result("JPEG (q75)", jpeg75_bytes.len(), jpeg75_time, jpeg_bytes.len());

        // WebP lossless (image crate only supports lossless)
        let (webp_bytes, webp_time) = bench_webp_lossless(&rgba, thumb.width(), thumb.height());
        print_result("WebP lossless", webp_bytes.len(), webp_time, jpeg_bytes.len());

        // AVIF speed 10 (fastest)
        let (avif_fast_bytes, avif_fast_time) =
            bench_avif(&rgba, thumb.width(), thumb.height(), 10, 80);
        print_result("AVIF (s10 q80)", avif_fast_bytes.len(), avif_fast_time, jpeg_bytes.len());

        // AVIF speed 6 (balanced)
        let (avif_bal_bytes, avif_bal_time) =
            bench_avif(&rgba, thumb.width(), thumb.height(), 6, 85);
        print_result("AVIF (s6 q85)", avif_bal_bytes.len(), avif_bal_time, jpeg_bytes.len());

        // PNG (lossless baseline)
        let (png_bytes, png_time) = bench_png(&rgba, thumb.width(), thumb.height());
        print_result("PNG (lossless)", png_bytes.len(), png_time, jpeg_bytes.len());

        println!();
    }

    println!("RECOMMENDATION:");
    println!("  - For speed:    WebP lossy (fast + ~25% smaller than JPEG)");
    println!("  - For size:     AVIF s10 (slower but ~40% smaller than JPEG)");
    println!("  - For compat:   JPEG (universal support)");
}

fn print_result(name: &str, size: usize, time: std::time::Duration, baseline: usize) {
    let ratio = (size as f64 / baseline as f64 * 100.0) as i32;
    let ratio_str = if ratio == 100 {
        "baseline".to_string()
    } else {
        format!("{}%", ratio)
    };
    println!(
        "  {:<20} {:>7} KB {:>10.2?} {:>10}",
        name,
        size / 1024,
        time,
        ratio_str
    );
}

fn bench_jpeg(
    rgb: &image::RgbImage,
    width: u32,
    height: u32,
    quality: u8,
) -> (Vec<u8>, std::time::Duration) {
    let start = Instant::now();
    let mut bytes: Vec<u8> = Vec::new();
    let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, quality);
    encoder
        .write_image(rgb.as_raw(), width, height, image::ExtendedColorType::Rgb8)
        .unwrap();
    (bytes, start.elapsed())
}

fn bench_webp_lossless(
    rgba: &image::RgbaImage,
    width: u32,
    height: u32,
) -> (Vec<u8>, std::time::Duration) {
    let start = Instant::now();
    let mut bytes: Vec<u8> = Vec::new();
    let encoder = image::codecs::webp::WebPEncoder::new_lossless(&mut bytes);
    encoder
        .write_image(
            rgba.as_raw(),
            width,
            height,
            image::ExtendedColorType::Rgba8,
        )
        .unwrap();
    (bytes, start.elapsed())
}

fn bench_avif(
    rgba: &image::RgbaImage,
    width: u32,
    height: u32,
    speed: u8,
    quality: u8,
) -> (Vec<u8>, std::time::Duration) {
    let start = Instant::now();
    let mut bytes: Vec<u8> = Vec::new();
    let encoder =
        image::codecs::avif::AvifEncoder::new_with_speed_quality(&mut bytes, speed, quality);
    encoder
        .write_image(
            rgba.as_raw(),
            width,
            height,
            image::ExtendedColorType::Rgba8,
        )
        .unwrap();
    (bytes, start.elapsed())
}

fn bench_png(
    rgba: &image::RgbaImage,
    width: u32,
    height: u32,
) -> (Vec<u8>, std::time::Duration) {
    let start = Instant::now();
    let mut bytes: Vec<u8> = Vec::new();
    let encoder = image::codecs::png::PngEncoder::new(&mut bytes);
    encoder
        .write_image(
            rgba.as_raw(),
            width,
            height,
            image::ExtendedColorType::Rgba8,
        )
        .unwrap();
    (bytes, start.elapsed())
}

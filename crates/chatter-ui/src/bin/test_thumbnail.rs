//! Test video thumbnail extraction using the actual crate function.
//!
//! Run with: cargo run --bin test_thumbnail -- /path/to/video.mkv

use std::path::Path;
use std::time::Instant;

fn main() {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_target(false)
        .init();

    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: {} <video_file>", args[0]);
        std::process::exit(1);
    }

    let video_path = Path::new(&args[1]);
    if !video_path.exists() {
        eprintln!("File not found: {}", video_path.display());
        std::process::exit(1);
    }

    println!("\n=== Testing Video Thumbnail Extraction ===\n");
    println!("File: {}", video_path.display());

    let start = Instant::now();

    match chatter_core::media::extract_video_thumbnail(video_path) {
        Ok((webp_data, width, height, duration_ms)) => {
            println!("\nSUCCESS in {:?}", start.elapsed());
            println!("  Thumbnail: {}x{}", width, height);
            println!("  WebP size: {} bytes", webp_data.len());
            println!("  Duration: {}ms ({:.1}s)", duration_ms, duration_ms as f64 / 1000.0);

            // Save thumbnail
            let out_path = video_path.with_extension("thumb.webp");
            if let Err(e) = std::fs::write(&out_path, &webp_data) {
                eprintln!("Failed to save thumbnail: {}", e);
            } else {
                println!("  Saved to: {}", out_path.display());
            }
        }
        Err(e) => {
            eprintln!("\nFAILED in {:?}", start.elapsed());
            eprintln!("  Error: {}", e);
            std::process::exit(1);
        }
    }
}

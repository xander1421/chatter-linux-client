//! Benchmark for image viewing pipeline.
//!
//! Measures: decrypt → PNG decode → RGBA conversion → UI buffer creation
//!
//! Run with: cargo test --release image_viewer_bench -- --nocapture

use std::time::Instant;

/// Benchmark PNG decoding and RGBA conversion for large images.
///
/// Uses the space-galaxy benchmark file (21MB PNG, 5733x2169).
#[test]
fn bench_image_decode_pipeline() {
    let benchmark_file = std::path::Path::new("/home/alex/Downloads/space-galaxy");

    if !benchmark_file.exists() {
        println!("Benchmark file not found: {}", benchmark_file.display());
        println!("Skipping benchmark.");
        return;
    }

    // Read file into memory (simulating decrypted buffer)
    let start = Instant::now();
    let data = std::fs::read(benchmark_file).expect("Failed to read benchmark file");
    let read_time = start.elapsed();
    println!("\n=== Image Viewer Pipeline Benchmark ===");
    println!(
        "File: {} ({:.2} MB)",
        benchmark_file.display(),
        data.len() as f64 / 1_000_000.0
    );
    println!(
        "1. File read: {:?} ({:.2} GB/s)",
        read_time,
        data.len() as f64 / read_time.as_secs_f64() / 1_000_000_000.0
    );

    // Step 2: PNG decode
    let start = Instant::now();
    let cursor = std::io::Cursor::new(&data);
    let reader = image::ImageReader::new(cursor)
        .with_guessed_format()
        .expect("Failed to guess format");
    let image = reader.decode().expect("Failed to decode image");
    let decode_time = start.elapsed();
    println!(
        "2. PNG decode: {:?} ({}x{})",
        decode_time,
        image.width(),
        image.height()
    );

    // Step 3: to_rgba8() conversion
    let start = Instant::now();
    let rgba = image.to_rgba8();
    let rgba_time = start.elapsed();
    let rgba_size = rgba.width() as usize * rgba.height() as usize * 4;
    println!(
        "3. to_rgba8(): {:?} ({:.2} MB output)",
        rgba_time,
        rgba_size as f64 / 1_000_000.0
    );

    // Step 4: into_raw() - get raw bytes
    let start = Instant::now();
    let width = rgba.width();
    let height = rgba.height();
    let raw_data = rgba.into_raw();
    let raw_time = start.elapsed();
    println!("4. into_raw(): {:?}", raw_time);

    // Step 5: Simulate SharedPixelBuffer::clone_from_slice (memory copy)
    let start = Instant::now();
    let _buffer_copy: Vec<u8> = raw_data.clone();
    let copy_time = start.elapsed();
    println!(
        "5. Buffer copy: {:?} ({:.2} GB/s)",
        copy_time,
        raw_data.len() as f64 / copy_time.as_secs_f64() / 1_000_000_000.0
    );

    // Calculate totals
    let total = read_time + decode_time + rgba_time + raw_time + copy_time;
    println!("\n=== Summary ===");
    println!("Total pipeline: {:?}", total);
    println!("Image dimensions: {}x{}", width, height);
    println!(
        "Decompressed size: {:.2} MB",
        rgba_size as f64 / 1_000_000.0
    );

    // Breakdown percentages
    let total_ns = total.as_nanos() as f64;
    println!("\nBreakdown:");
    println!(
        "  Read:    {:.1}%",
        read_time.as_nanos() as f64 / total_ns * 100.0
    );
    println!(
        "  Decode:  {:.1}%",
        decode_time.as_nanos() as f64 / total_ns * 100.0
    );
    println!(
        "  RGBA:    {:.1}%",
        rgba_time.as_nanos() as f64 / total_ns * 100.0
    );
    println!(
        "  Raw:     {:.1}%",
        raw_time.as_nanos() as f64 / total_ns * 100.0
    );
    println!(
        "  Copy:    {:.1}%",
        copy_time.as_nanos() as f64 / total_ns * 100.0
    );
}

/// Benchmark decryption performance for large files.
#[test]
fn bench_decryption_pipeline() {
    use chatter_core::crypto::attachment_crypto::AttachmentCrypto;

    // Create a 21MB test buffer (simulating the PNG file)
    let plaintext_size = 21 * 1024 * 1024; // 21 MB
    let plaintext: Vec<u8> = (0..plaintext_size).map(|i| (i % 256) as u8).collect();

    println!("\n=== Decryption Pipeline Benchmark ===");
    println!(
        "Plaintext size: {:.2} MB",
        plaintext_size as f64 / 1_000_000.0
    );

    // Encrypt
    let start = Instant::now();
    let encrypted = AttachmentCrypto::encrypt(&plaintext).expect("Encryption failed");
    let encrypt_time = start.elapsed();
    println!(
        "1. Encrypt: {:?} ({:.2} GB/s)",
        encrypt_time,
        plaintext_size as f64 / encrypt_time.as_secs_f64() / 1_000_000_000.0
    );
    println!(
        "   Ciphertext size: {:.2} MB",
        encrypted.ciphertext.len() as f64 / 1_000_000.0
    );

    // Decrypt with zero-copy (parallel)
    let start = Instant::now();
    let decrypted = chatter_core::crypto::attachment_crypto_fast::decrypt_zero_copy(
        encrypted.ciphertext.clone(),
        &encrypted.key,
        &encrypted.digest,
        encrypted.plaintext_size,
    )
    .expect("Decryption failed");
    let decrypt_time = start.elapsed();
    println!(
        "2. Decrypt (parallel): {:?} ({:.2} GB/s)",
        decrypt_time,
        plaintext_size as f64 / decrypt_time.as_secs_f64() / 1_000_000_000.0
    );

    assert_eq!(decrypted.len(), plaintext_size);
    assert_eq!(&decrypted[..100], &plaintext[..100]);

    println!("\n=== Summary ===");
    println!(
        "Decryption throughput: {:.2} GB/s",
        plaintext_size as f64 / decrypt_time.as_secs_f64() / 1_000_000_000.0
    );
}

/// Full pipeline benchmark: decrypt + decode + display.
#[test]
fn bench_full_pipeline() {
    use chatter_core::crypto::attachment_crypto::AttachmentCrypto;

    let benchmark_file = std::path::Path::new("/home/alex/Downloads/space-galaxy");

    if !benchmark_file.exists() {
        println!("Benchmark file not found, skipping.");
        return;
    }

    // Read the actual PNG file
    let png_data = std::fs::read(benchmark_file).expect("Failed to read file");
    println!("\n=== Full Pipeline Benchmark (E2E) ===");
    println!("PNG file: {:.2} MB", png_data.len() as f64 / 1_000_000.0);

    // Step 1: Encrypt (simulating what upload does)
    let start = Instant::now();
    let encrypted = AttachmentCrypto::encrypt(&png_data).expect("Encryption failed");
    let encrypt_time = start.elapsed();
    println!("1. Encrypt: {:?}", encrypt_time);

    // Step 2: Decrypt (what viewing does)
    let start = Instant::now();
    let decrypted = chatter_core::crypto::attachment_crypto_fast::decrypt_zero_copy(
        encrypted.ciphertext.clone(),
        &encrypted.key,
        &encrypted.digest,
        encrypted.plaintext_size,
    )
    .expect("Decryption failed");
    let decrypt_time = start.elapsed();
    println!("2. Decrypt: {:?}", decrypt_time);

    // Step 3: PNG decode
    let start = Instant::now();
    let cursor = std::io::Cursor::new(&decrypted);
    let reader = image::ImageReader::new(cursor)
        .with_guessed_format()
        .expect("Failed to guess format");
    let image = reader.decode().expect("Failed to decode image");
    let decode_time = start.elapsed();
    println!(
        "3. PNG decode: {:?} ({}x{})",
        decode_time,
        image.width(),
        image.height()
    );

    // Step 4: RGBA conversion
    let start = Instant::now();
    let rgba = image.to_rgba8();
    let rgba_time = start.elapsed();
    println!("4. to_rgba8(): {:?}", rgba_time);

    // Step 5: Buffer preparation for UI
    let start = Instant::now();
    let _raw = rgba.into_raw();
    let raw_time = start.elapsed();
    println!("5. into_raw(): {:?}", raw_time);

    // Total time
    let total = decrypt_time + decode_time + rgba_time + raw_time;
    println!("\n=== RESULTS ===");
    println!("Total time (receive → display): {:?}", total);
    println!("\nThis is what the user experiences after clicking 'view'.");
    println!("(Excludes network download time)");
}

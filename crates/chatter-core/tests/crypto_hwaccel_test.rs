//! Test to verify hardware-accelerated AES is working.
//!
//! Run with: cargo test --test crypto_hwaccel_test -- --nocapture

use std::time::Instant;

// The standalone OpenSSL AES/SHA throughput micro-benchmarks were removed with
// the OpenSSL dependency (attachment crypto now uses RustCrypto aes/cbc/sha2;
// SQLCipher links webrtc's BoringSSL). Pipeline throughput is still covered by
// test_attachment_crypto_performance below.


/// Test full attachment encryption/decryption pipeline.
#[test]
fn test_attachment_crypto_performance() {
    use chatter_core::crypto::attachment_crypto::AttachmentCrypto;

    // 50 MB test (smaller to keep test fast)
    let size = 50 * 1024 * 1024;
    let plaintext: Vec<u8> = (0..size).map(|i| (i % 256) as u8).collect();

    // Encryption
    let start = Instant::now();
    let encrypted = AttachmentCrypto::encrypt(&plaintext).unwrap();
    let encrypt_time = start.elapsed();
    let encrypt_throughput = size as f64 / encrypt_time.as_secs_f64() / (1024.0 * 1024.0);

    // Decryption
    let start = Instant::now();
    let decrypted = AttachmentCrypto::decrypt(
        &encrypted.ciphertext,
        &encrypted.key,
        &encrypted.digest,
        encrypted.plaintext_size,
    ).unwrap();
    let decrypt_time = start.elapsed();
    let decrypt_throughput = size as f64 / decrypt_time.as_secs_f64() / (1024.0 * 1024.0);

    assert_eq!(decrypted, plaintext, "Decryption mismatch!");

    println!("\n========================================");
    println!("Full Attachment Crypto Pipeline Test");
    println!("(AES-256-CBC + HMAC-SHA256 + SHA256)");
    println!("========================================");
    println!("Data size:          {} MB", size / (1024 * 1024));
    println!("Encrypt time:       {:.3}s ({:.1} MB/s)", encrypt_time.as_secs_f64(), encrypt_throughput);
    println!("Decrypt time:       {:.3}s ({:.1} MB/s)", decrypt_time.as_secs_f64(), decrypt_throughput);
    println!("----------------------------------------");

    if decrypt_throughput > 300.0 {
        println!("✓ HARDWARE ACCELERATION WORKING");
    } else {
        println!("⚠ Throughput lower than expected");
    }
    println!("========================================\n");

    assert!(
        decrypt_throughput > 100.0,
        "Attachment decryption too slow ({:.1} MB/s)",
        decrypt_throughput
    );
}

/// Test parallel decryption with rayon (decrypt_zero_copy).
#[test]
fn test_parallel_decryption_with_rayon() {
    use chatter_core::crypto::attachment_crypto::AttachmentCrypto;
    use chatter_core::crypto::attachment_crypto_fast::decrypt_zero_copy;

    // 100 MB test - large enough to benefit from parallelization
    let size = 100 * 1024 * 1024;
    let plaintext: Vec<u8> = (0..size).map(|i| (i % 256) as u8).collect();

    // Encrypt
    let encrypted = AttachmentCrypto::encrypt(&plaintext).unwrap();

    // Decrypt with rayon parallelization
    let start = Instant::now();
    let decrypted = decrypt_zero_copy(
        encrypted.ciphertext.clone(),
        &encrypted.key,
        &encrypted.digest,
        encrypted.plaintext_size,
    ).unwrap();
    let decrypt_time = start.elapsed();
    let decrypt_throughput = size as f64 / decrypt_time.as_secs_f64() / (1024.0 * 1024.0);

    assert_eq!(decrypted, plaintext, "Decryption mismatch!");

    println!("\n========================================");
    println!("Parallel Decryption Test (rayon)");
    println!("(SHA256 || HMAC in parallel, then AES)");
    println!("========================================");
    println!("Data size:          {} MB", size / (1024 * 1024));
    println!("Decrypt time:       {:.3}s", decrypt_time.as_secs_f64());
    println!("Decrypt throughput: {:.1} MB/s", decrypt_throughput);
    println!("----------------------------------------");

    if decrypt_throughput > 1000.0 {
        println!("✓ PARALLEL ACCELERATION WORKING");
    } else if decrypt_throughput > 600.0 {
        println!("⚠ Parallel overhead limiting gains");
    } else {
        println!("✗ Parallelization not effective");
    }
    println!("========================================\n");

    // Floor confirms AES-NI is active (software AES-CBC is <50 MB/s). Exact
    // number varies with RustCrypto vs OpenSSL and machine load, so this only
    // guards against an accidental software fallback, not absolute speed.
    assert!(
        decrypt_throughput > 150.0,
        "Parallel decryption too slow ({:.1} MB/s)",
        decrypt_throughput
    );
}

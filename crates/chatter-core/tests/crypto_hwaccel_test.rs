//! Test to verify hardware-accelerated AES is working.
//!
//! Run with: cargo test --test crypto_hwaccel_test -- --nocapture

use std::time::Instant;

/// Test AES-256-CBC encryption speed using OpenSSL.
/// Hardware-accelerated (AES-NI) should achieve >500 MB/s.
/// Software fallback is typically <50 MB/s.
#[test]
fn test_openssl_aes_hardware_acceleration() {
    use openssl::symm::{Cipher, Crypter, Mode};

    // 100 MB test data
    let size = 100 * 1024 * 1024;
    let plaintext: Vec<u8> = (0..size).map(|i| (i % 256) as u8).collect();

    let key = [0x42u8; 32]; // AES-256 key
    let iv = [0x24u8; 16];  // IV

    let cipher = Cipher::aes_256_cbc();

    // Encryption benchmark
    let start = Instant::now();
    let mut crypter = Crypter::new(cipher, Mode::Encrypt, &key, Some(&iv)).unwrap();
    crypter.pad(false);

    let mut ciphertext = vec![0u8; size + 16];
    let mut count = crypter.update(&plaintext, &mut ciphertext).unwrap();
    count += crypter.finalize(&mut ciphertext[count..]).unwrap();
    ciphertext.truncate(count);

    let encrypt_time = start.elapsed();
    let encrypt_throughput = size as f64 / encrypt_time.as_secs_f64() / (1024.0 * 1024.0);

    // Decryption benchmark
    let start = Instant::now();
    let mut crypter = Crypter::new(cipher, Mode::Decrypt, &key, Some(&iv)).unwrap();
    crypter.pad(false);

    let mut decrypted = vec![0u8; size + 16];
    let mut count = crypter.update(&ciphertext, &mut decrypted).unwrap();
    count += crypter.finalize(&mut decrypted[count..]).unwrap();
    decrypted.truncate(count);

    let decrypt_time = start.elapsed();
    let decrypt_throughput = size as f64 / decrypt_time.as_secs_f64() / (1024.0 * 1024.0);

    // Verify correctness
    assert_eq!(&decrypted[..], &plaintext[..], "Decryption mismatch!");

    println!("\n========================================");
    println!("OpenSSL AES-256-CBC Hardware Acceleration Test");
    println!("========================================");
    println!("Data size:          {} MB", size / (1024 * 1024));
    println!("Encrypt time:       {:.3}s", encrypt_time.as_secs_f64());
    println!("Encrypt throughput: {:.1} MB/s", encrypt_throughput);
    println!("Decrypt time:       {:.3}s", decrypt_time.as_secs_f64());
    println!("Decrypt throughput: {:.1} MB/s", decrypt_throughput);
    println!("----------------------------------------");

    if decrypt_throughput > 500.0 {
        println!("✓ HARDWARE ACCELERATION CONFIRMED (AES-NI)");
        println!("  Expected: >500 MB/s, Got: {:.1} MB/s", decrypt_throughput);
    } else if decrypt_throughput > 100.0 {
        println!("⚠ PARTIAL ACCELERATION (may be software)");
        println!("  Expected: >500 MB/s, Got: {:.1} MB/s", decrypt_throughput);
    } else {
        println!("✗ SOFTWARE FALLBACK DETECTED");
        println!("  Expected: >500 MB/s, Got: {:.1} MB/s", decrypt_throughput);
    }
    println!("========================================\n");

    // Fail test if throughput is too low
    assert!(
        decrypt_throughput > 200.0,
        "AES throughput too low ({:.1} MB/s) - hardware acceleration may not be working",
        decrypt_throughput
    );
}

/// Test SHA-256 hashing speed with OpenSSL (should use SHA-NI).
/// Hardware-accelerated should achieve >1000 MB/s.
#[test]
fn test_sha256_hardware_acceleration() {
    use openssl::hash::{Hasher, MessageDigest};

    // 100 MB test data
    let size = 100 * 1024 * 1024;
    let data: Vec<u8> = (0..size).map(|i| (i % 256) as u8).collect();

    let start = Instant::now();
    let mut hasher = Hasher::new(MessageDigest::sha256()).unwrap();
    hasher.update(&data).unwrap();
    let _hash = hasher.finish().unwrap();
    let hash_time = start.elapsed();

    let throughput = size as f64 / hash_time.as_secs_f64() / (1024.0 * 1024.0);

    println!("\n========================================");
    println!("SHA-256 Hardware Acceleration Test (OpenSSL)");
    println!("========================================");
    println!("Data size:   {} MB", size / (1024 * 1024));
    println!("Hash time:   {:.3}s", hash_time.as_secs_f64());
    println!("Throughput:  {:.1} MB/s", throughput);
    println!("----------------------------------------");

    if throughput > 1000.0 {
        println!("✓ HARDWARE ACCELERATION CONFIRMED (SHA-NI)");
    } else if throughput > 500.0 {
        println!("⚠ PARTIAL ACCELERATION");
    } else {
        println!("✗ SOFTWARE FALLBACK");
    }
    println!("========================================\n");

    assert!(
        throughput > 500.0,
        "SHA-256 throughput too low ({:.1} MB/s) - hardware acceleration not working",
        throughput
    );
}

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

    assert!(
        decrypt_throughput > 500.0,
        "Parallel decryption too slow ({:.1} MB/s)",
        decrypt_throughput
    );
}

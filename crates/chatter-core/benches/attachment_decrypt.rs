//! Benchmark for attachment decryption path.
//!
//! Measures each step of the decryption pipeline to identify bottlenecks.
//!
//! Run with: cargo bench --bench attachment_decrypt

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use sha2::{Digest, Sha256};
use hmac::{Hmac, Mac};
use aes::Aes256;
use cbc::cipher::{BlockDecryptMut, BlockEncryptMut, KeyIvInit};

type Aes256CbcEnc = cbc::Encryptor<Aes256>;
type Aes256CbcDec = cbc::Decryptor<Aes256>;
type HmacSha256 = Hmac<Sha256>;

const IV_LENGTH: usize = 16;
const AES_KEY_LENGTH: usize = 32;
const MAC_KEY_LENGTH: usize = 32;
const HMAC_LENGTH: usize = 32;
const KEY_SET_LENGTH: usize = AES_KEY_LENGTH + MAC_KEY_LENGTH;

/// Generate test ciphertext of a given size.
/// Returns (ciphertext, key, digest, plaintext_size)
fn generate_test_data(size: usize) -> (Vec<u8>, [u8; KEY_SET_LENGTH], [u8; 32], usize) {
    use rand::RngCore;

    // Generate random key
    let mut key = [0u8; KEY_SET_LENGTH];
    rand::rng().fill_bytes(&mut key);

    let aes_key = &key[..AES_KEY_LENGTH];
    let mac_key = &key[AES_KEY_LENGTH..];

    // Padded size (round up to block size)
    let padded_size = (size + 15) & !15;
    let padded_size = padded_size.max(16);

    // Generate random plaintext and pad
    let mut padded = vec![0u8; padded_size];
    rand::rng().fill_bytes(&mut padded[..size.min(padded_size)]);

    // Generate IV
    let mut iv = [0u8; IV_LENGTH];
    rand::rng().fill_bytes(&mut iv);

    // Encrypt
    let cipher = Aes256CbcEnc::new_from_slices(aes_key, &iv).unwrap();
    let mut encrypted = padded.clone();
    cipher.encrypt_padded_mut::<block_padding::NoPadding>(&mut encrypted, padded_size).unwrap();

    // Compute HMAC
    let mut mac = HmacSha256::new_from_slice(mac_key).unwrap();
    mac.update(&iv);
    mac.update(&encrypted);
    let hmac_result = mac.finalize().into_bytes();

    // Assemble ciphertext
    let mut ciphertext = Vec::with_capacity(IV_LENGTH + encrypted.len() + HMAC_LENGTH);
    ciphertext.extend_from_slice(&iv);
    ciphertext.extend_from_slice(&encrypted);
    ciphertext.extend_from_slice(&hmac_result);

    // Compute digest
    let mut hasher = Sha256::new();
    hasher.update(&ciphertext);
    let digest: [u8; 32] = hasher.finalize().into();

    (ciphertext, key, digest, size)
}

/// Benchmark: SHA256 digest computation
fn bench_sha256_digest(c: &mut Criterion) {
    let mut group = c.benchmark_group("sha256_digest");

    for size in [1_000_000, 5_000_000, 10_000_000, 25_000_000] {
        let (ciphertext, _, _, _) = generate_test_data(size);

        group.throughput(Throughput::Bytes(ciphertext.len() as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{}MB", size / 1_000_000)),
            &ciphertext,
            |b, data| {
                b.iter(|| {
                    let mut hasher = Sha256::new();
                    hasher.update(black_box(data));
                    let _digest: [u8; 32] = hasher.finalize().into();
                });
            },
        );
    }

    group.finish();
}

/// Benchmark: HMAC-SHA256 verification
fn bench_hmac_verify(c: &mut Criterion) {
    let mut group = c.benchmark_group("hmac_verify");

    for size in [1_000_000, 5_000_000, 10_000_000, 25_000_000] {
        let (ciphertext, key, _, _) = generate_test_data(size);
        let mac_key = &key[AES_KEY_LENGTH..];
        let iv = &ciphertext[..IV_LENGTH];
        let encrypted = &ciphertext[IV_LENGTH..ciphertext.len() - HMAC_LENGTH];

        group.throughput(Throughput::Bytes(encrypted.len() as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{}MB", size / 1_000_000)),
            &(iv, encrypted, mac_key),
            |b, (iv, encrypted, mac_key)| {
                b.iter(|| {
                    let mut mac = HmacSha256::new_from_slice(mac_key).unwrap();
                    mac.update(black_box(*iv));
                    mac.update(black_box(*encrypted));
                    let _result = mac.finalize();
                });
            },
        );
    }

    group.finish();
}

/// Benchmark: AES-256-CBC decryption
fn bench_aes_decrypt(c: &mut Criterion) {
    let mut group = c.benchmark_group("aes_decrypt");

    for size in [1_000_000, 5_000_000, 10_000_000, 25_000_000] {
        let (ciphertext, key, _, _) = generate_test_data(size);
        let aes_key = &key[..AES_KEY_LENGTH];
        let iv = &ciphertext[..IV_LENGTH];
        let encrypted = &ciphertext[IV_LENGTH..ciphertext.len() - HMAC_LENGTH];

        group.throughput(Throughput::Bytes(encrypted.len() as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{}MB", size / 1_000_000)),
            &(iv, encrypted, aes_key),
            |b, (iv, encrypted, aes_key)| {
                b.iter(|| {
                    let cipher = Aes256CbcDec::new_from_slices(aes_key, iv).unwrap();
                    let mut decrypted = (*encrypted).to_vec();
                    cipher.decrypt_padded_mut::<block_padding::NoPadding>(&mut decrypted).unwrap();
                    black_box(decrypted)
                });
            },
        );
    }

    group.finish();
}

/// Benchmark: Full decryption pipeline (digest + HMAC + AES)
fn bench_full_decrypt(c: &mut Criterion) {
    let mut group = c.benchmark_group("full_decrypt");

    for size in [1_000_000, 5_000_000, 10_000_000, 25_000_000] {
        let (ciphertext, key, expected_digest, plaintext_size) = generate_test_data(size);

        group.throughput(Throughput::Bytes(ciphertext.len() as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{}MB", size / 1_000_000)),
            &(ciphertext.clone(), key, expected_digest, plaintext_size),
            |b, (ciphertext, key, expected_digest, plaintext_size)| {
                b.iter(|| {
                    // Step 1: Verify digest
                    let mut hasher = Sha256::new();
                    hasher.update(black_box(ciphertext));
                    let actual_digest: [u8; 32] = hasher.finalize().into();
                    assert_eq!(&actual_digest, expected_digest);

                    // Step 2: Split components
                    let aes_key = &key[..AES_KEY_LENGTH];
                    let mac_key = &key[AES_KEY_LENGTH..];
                    let iv = &ciphertext[..IV_LENGTH];
                    let encrypted = &ciphertext[IV_LENGTH..ciphertext.len() - HMAC_LENGTH];
                    let expected_hmac = &ciphertext[ciphertext.len() - HMAC_LENGTH..];

                    // Step 3: Verify HMAC
                    let mut mac = HmacSha256::new_from_slice(mac_key).unwrap();
                    mac.update(iv);
                    mac.update(encrypted);
                    let computed_hmac = mac.finalize().into_bytes();
                    assert_eq!(computed_hmac.as_slice(), expected_hmac);

                    // Step 4: Decrypt
                    let cipher = Aes256CbcDec::new_from_slices(aes_key, iv).unwrap();
                    let mut decrypted = encrypted.to_vec();
                    cipher.decrypt_padded_mut::<block_padding::NoPadding>(&mut decrypted).unwrap();

                    // Step 5: Truncate
                    decrypted.truncate(*plaintext_size);
                    black_box(decrypted)
                });
            },
        );
    }

    group.finish();
}

/// Benchmark: Memory allocation overhead
fn bench_allocation(c: &mut Criterion) {
    let mut group = c.benchmark_group("allocation");

    for size in [1_000_000, 5_000_000, 10_000_000, 25_000_000] {
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{}MB", size / 1_000_000)),
            &size,
            |b, &size| {
                b.iter(|| {
                    let v: Vec<u8> = vec![0u8; size];
                    black_box(v)
                });
            },
        );
    }

    group.finish();
}

/// Benchmark: Vec copy (to_vec())
fn bench_vec_copy(c: &mut Criterion) {
    let mut group = c.benchmark_group("vec_copy");

    for size in [1_000_000, 5_000_000, 10_000_000, 25_000_000] {
        let data: Vec<u8> = (0..size).map(|i| (i % 256) as u8).collect();

        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{}MB", size / 1_000_000)),
            &data,
            |b, data| {
                b.iter(|| {
                    let copy = black_box(data).to_vec();
                    black_box(copy)
                });
            },
        );
    }

    group.finish();
}

/// Benchmark: Single-pass decryption (optimized)
fn bench_single_pass_decrypt(c: &mut Criterion) {
    let mut group = c.benchmark_group("single_pass_decrypt");

    for size in [1_000_000, 5_000_000, 10_000_000, 25_000_000] {
        let (ciphertext, key, expected_digest, plaintext_size) = generate_test_data(size);

        group.throughput(Throughput::Bytes(ciphertext.len() as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{}MB", size / 1_000_000)),
            &(ciphertext.clone(), key, expected_digest, plaintext_size),
            |b, (ciphertext, key, expected_digest, plaintext_size)| {
                b.iter(|| {
                    // Single-pass: digest AND HMAC computed together
                    let aes_key = &key[..AES_KEY_LENGTH];
                    let mac_key = &key[AES_KEY_LENGTH..];
                    let iv = &ciphertext[..IV_LENGTH];
                    let encrypted = &ciphertext[IV_LENGTH..ciphertext.len() - HMAC_LENGTH];
                    let expected_hmac = &ciphertext[ciphertext.len() - HMAC_LENGTH..];

                    // Combined pass over data
                    let mut digest_hasher = Sha256::new();
                    let mut mac = HmacSha256::new_from_slice(mac_key).unwrap();

                    digest_hasher.update(black_box(iv));
                    mac.update(black_box(iv));

                    // Process in 64KB chunks for cache efficiency
                    for chunk in encrypted.chunks(64 * 1024) {
                        digest_hasher.update(black_box(chunk));
                        mac.update(black_box(chunk));
                    }

                    digest_hasher.update(expected_hmac);

                    let actual_digest: [u8; 32] = digest_hasher.finalize().into();
                    black_box(mac.finalize());
                    assert_eq!(&actual_digest, expected_digest);

                    // Decrypt
                    let cipher = Aes256CbcDec::new_from_slices(aes_key, iv).unwrap();
                    let mut decrypted = encrypted.to_vec();
                    cipher.decrypt_padded_mut::<block_padding::NoPadding>(&mut decrypted).unwrap();
                    decrypted.truncate(*plaintext_size);
                    black_box(decrypted)
                });
            },
        );
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_sha256_digest,
    bench_hmac_verify,
    bench_aes_decrypt,
    bench_full_decrypt,
    bench_single_pass_decrypt,
    bench_allocation,
    bench_vec_copy,
);

criterion_main!(benches);

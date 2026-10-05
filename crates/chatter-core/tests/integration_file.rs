//! Integration tests for FileClient.
//!
//! Tests the Signal-style presigned attachment upload flow.

use crate::{common, skip_if_no_server};

use std::sync::Arc;

use chatter_core::network::grpc::FileClient;
use chatter_core::proto::chat_v1::CompletedPart;
use chatter_core::services::AttachmentService;
use chatter_core::services::attachment::set_attachments_base_dir;
use chatter_core::storage::{AttachmentRepository, Database};

use common::*;

// The storage edge uses the same private PKI as the application transport.
// Exercise signed multipart HTTP directly without trusting public CA roots.
fn storage_http_client() -> reqwest::Client {
    let pem = chatter_core::config::TLS_ROOT_CA_PEM.expect("test storage CA must be pinned");
    let ca = reqwest::Certificate::from_pem(pem.as_bytes()).expect("valid test storage CA");
    reqwest::Client::builder()
        .tls_built_in_root_certs(false)
        .add_root_certificate(ca)
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .expect("pinned storage HTTP client")
}

// ============================================================================
// Attachment URL Tests
// ============================================================================

#[tokio::test]
async fn test_get_attachment_upload_url() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);

    let mut file_client = FileClient::new(pool.channel(), pool.auth_clone());

    // Request upload URL for a 1KB file
    let response = assert_ok(file_client.get_attachment_upload_url(1024).await);

    assert!(!response.upload_url.is_empty(), "Should get an upload URL");
    assert!(!response.cdn_key.is_empty(), "Should get a CDN key");
}

// ============================================================================
// Multipart Upload Tests
// ============================================================================

/// CRITICAL GATE: asserts RustFS actually supports real S3 multipart uploads
/// with presigned `UploadPart` URLs. The whole multipart-upload feature (Task
/// 1-6) is built on the assumption that `minio.Core`'s presigned part-upload
/// primitive works end-to-end against RustFS. If this test fails while
/// running against a real server, that is NOT a test bug — it means RustFS
/// does not support this primitive and the feature is blocked. Do not weaken
/// or skip this assertion to make it pass.
#[tokio::test]
async fn test_rustfs_supports_presigned_multipart() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);
    let mut file_client = FileClient::new(pool.channel(), pool.auth_clone());

    // 5 MiB + 1 byte: below the 16 MiB part size, so this always produces a
    // single part. That's fine for the gate — we only need to prove
    // create -> presigned part PUT -> complete works end-to-end.
    let size = 5 * 1024 * 1024 + 1;
    let create = assert_ok(
        file_client
            .create_attachment_multipart_upload(size as u64)
            .await,
    );
    assert!(
        !create.part_urls.is_empty(),
        "server returned zero presigned part URLs"
    );

    let http = storage_http_client();
    let body = vec![7u8; size];
    let resp = http
        .put(&create.part_urls[0])
        .header("Content-Length", size as u64)
        .body(body)
        .send()
        .await
        .map_err(reqwest::Error::without_url)
        .expect("presigned part PUT request should reach RustFS");

    assert!(
        resp.status().is_success(),
        "CRITICAL: RustFS rejected presigned UploadPart with status {} — \
         real S3 multipart + presigned part upload is NOT supported by this \
         RustFS deployment. The multipart attachment feature is blocked until \
         this is resolved.",
        resp.status()
    );

    let etag = resp
        .headers()
        .get(reqwest::header::ETAG)
        .expect("RustFS part PUT response is missing an ETag header")
        .to_str()
        .expect("ETag header should be valid ASCII")
        .trim_matches('"')
        .to_string();

    assert_ok(
        file_client
            .complete_attachment_multipart_upload(
                &create.cdn_key,
                &create.upload_id,
                vec![CompletedPart { number: 1, etag }],
            )
            .await,
    );
}

/// MINIMAL RustFS gate — no registration. The 3 multipart RPCs are PUBLIC
/// (no auth, like GetAttachmentUploadUrl), so this proves the RustFS presigned
/// UploadPart primitive works without touching the auth/registration path.
#[tokio::test]
async fn test_rustfs_presigned_multipart_minimal() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let mut file_client = FileClient::new(pool.channel(), pool.auth_clone());

    let size = 5 * 1024 * 1024 + 1; // one part (< 16 MiB)
    let create = assert_ok(
        file_client
            .create_attachment_multipart_upload(size as u64)
            .await,
    );
    eprintln!(
        "CREATE ok: cdn_key={} upload_id={} part_urls={} part_size={}",
        create.cdn_key,
        create.upload_id,
        create.part_urls.len(),
        create.part_size
    );
    assert!(
        !create.part_urls.is_empty(),
        "server returned zero part URLs"
    );

    let http = storage_http_client();
    let resp = http
        .put(&create.part_urls[0])
        .header("Content-Length", size as u64)
        .body(vec![7u8; size])
        .send()
        .await
        .map_err(reqwest::Error::without_url)
        .expect("presigned part PUT should reach RustFS");

    eprintln!(
        "PART PUT status={} etag={:?}",
        resp.status(),
        resp.headers().get(reqwest::header::ETAG)
    );
    assert!(
        resp.status().is_success(),
        "CRITICAL: RustFS rejected presigned UploadPart with status {}",
        resp.status()
    );

    let etag = resp
        .headers()
        .get(reqwest::header::ETAG)
        .expect("RustFS part PUT missing ETag header")
        .to_str()
        .unwrap()
        .trim_matches('"')
        .to_string();

    assert_ok(
        file_client
            .complete_attachment_multipart_upload(
                &create.cdn_key,
                &create.upload_id,
                vec![CompletedPart { number: 1, etag }],
            )
            .await,
    );
    eprintln!("RUSTFS MULTIPART OK — presigned UploadPart accepted + completed");
}

/// Full round-trip through the real client code path: encrypt + upload a
/// ~150 MB file (forces the multipart branch, threshold is 100 MB), download
/// the ciphertext back via the presigned GET path, decrypt it, and assert the
/// bytes match the original. Exercises `AttachmentService::upload_to_cdn_encrypted`,
/// `download_to_filesystem`, and `decrypt_auto` together — the same calls the
/// UI makes.
#[tokio::test]
async fn test_multipart_upload_round_trip_over_100mb() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);

    // Isolated temp dir for both the attachments store and the source file,
    // so this test never touches the real user data directory.
    let temp_dir = assert_ok(tempfile::tempdir());
    set_attachments_base_dir(temp_dir.path().to_path_buf());

    let db_path = temp_dir.path().join("test.db");
    let db = Arc::new(assert_ok(Database::open(&db_path, "test_key_passphrase")));
    assert_ok(db.run_migrations());

    let attachment_repo = Arc::new(AttachmentRepository::new(db.clone()));
    let attachment_service = AttachmentService::new(attachment_repo, pool.clone());

    // ~150 MB source file — comfortably over the 100 MB multipart threshold.
    let size: usize = 150 * 1024 * 1024;
    let original = generate_random_bytes(size);
    let source_path = temp_dir.path().join("source_150mb.bin");
    assert_ok(tokio::fs::write(&source_path, &original).await);

    let upload_result = assert_ok(
        attachment_service
            .upload_to_cdn_encrypted(&source_path)
            .await,
    );
    assert_eq!(
        upload_result.plaintext_size, size as u64,
        "uploaded plaintext size should match the source file"
    );

    assert_ok(
        attachment_service
            .download_to_filesystem(&upload_result.attachment_id)
            .await,
    );

    let decrypted = assert_ok(attachment_service.decrypt_auto(&upload_result.attachment_id));
    assert_eq!(
        decrypted.as_slice(),
        original.as_slice(),
        "decrypted round-trip bytes must equal the original file"
    );
}

/// Generate `size` pseudo-random bytes without pulling in a full CSPRNG per
/// byte — good enough entropy to catch any accidental all-zero/truncation
/// bugs in the multipart path while staying fast for 150 MB.
fn generate_random_bytes(size: usize) -> Vec<u8> {
    use rand::RngCore;
    let mut buf = vec![0u8; size];
    rand::rng().fill_bytes(&mut buf);
    buf
}

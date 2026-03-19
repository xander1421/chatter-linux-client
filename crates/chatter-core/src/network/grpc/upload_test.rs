//! Upload streaming tests - A/B/C testing different approaches.
//!
//! Run with: cargo test --package chatter-linux --lib network::grpc::upload_test -- --nocapture

use sha2::{Digest, Sha256};
use tonic::transport::Channel;

use crate::network::grpc::client_pool::GrpcClientPool;
use crate::network::grpc::interceptors::AuthInterceptor;
use crate::proto::chat_v1::{
    file_service_client::FileServiceClient, FileChunk, FileMetadata,
};

/// Test data - small file for quick testing
fn create_test_chunks(transfer_id: &str) -> Vec<FileChunk> {
    let test_data = b"Hello, this is test file content for upload testing!";
    let chunk_size = 16; // Small chunks for testing

    test_data
        .chunks(chunk_size)
        .enumerate()
        .map(|(seq, chunk_data)| {
            let mut hasher = Sha256::new();
            hasher.update(chunk_data);
            let hash = hasher.finalize();

            FileChunk {
                transfer_id: transfer_id.to_string(),
                sequence: seq as u32,
                data: chunk_data.to_vec(),
                auth_tag: hash[..16].to_vec(),
            }
        })
        .collect()
}

/// Approach A: tokio_stream::iter (canonical tonic pattern)
async fn upload_approach_a(
    client: &mut FileServiceClient<tonic::service::interceptor::InterceptedService<Channel, AuthInterceptor>>,
    channel_id: &str,
    transfer_id: &str,
) -> Result<FileMetadata, String> {
    let chunks = create_test_chunks(transfer_id);
    let chunk_count = chunks.len();

    tracing::info!("[A] tokio_stream::iter - {} chunks", chunk_count);

    // Canonical tonic pattern
    let stream = tokio_stream::iter(chunks);

    let mut request = tonic::Request::new(stream);
    request.metadata_mut().insert("channel-id", channel_id.parse().unwrap());
    request.metadata_mut().insert("file-name", "test_a.txt".parse().unwrap());
    request.metadata_mut().insert("mime-type", "text/plain".parse().unwrap());
    request.metadata_mut().insert("content-length", "52".parse().unwrap());

    match client.upload_file(request).await {
        Ok(r) => Ok(r.into_inner()),
        Err(e) => {
            tracing::error!("[A] Status code: {:?}", e.code());
            tracing::error!("[A] Message: {}", e.message());
            if let Some(source) = std::error::Error::source(&e) {
                tracing::error!("[A] Source: {:?}", source);
            }
            Err(format!("[A] Failed: {} (code: {:?})", e.message(), e.code()))
        }
    }
}

/// Approach B: futures::stream::iter with Box::pin
async fn upload_approach_b(
    client: &mut FileServiceClient<tonic::service::interceptor::InterceptedService<Channel, AuthInterceptor>>,
    channel_id: &str,
    transfer_id: &str,
) -> Result<FileMetadata, String> {
    let chunks = create_test_chunks(transfer_id);
    let chunk_count = chunks.len();

    tracing::info!("[B] futures::stream::iter + Box::pin - {} chunks", chunk_count);

    let stream = Box::pin(futures::stream::iter(chunks));

    let mut request = tonic::Request::new(stream);
    request.metadata_mut().insert("channel-id", channel_id.parse().unwrap());
    request.metadata_mut().insert("file-name", "test_b.txt".parse().unwrap());
    request.metadata_mut().insert("mime-type", "text/plain".parse().unwrap());
    request.metadata_mut().insert("content-length", "52".parse().unwrap());

    match client.upload_file(request).await {
        Ok(r) => Ok(r.into_inner()),
        Err(e) => {
            tracing::error!("[B] Status code: {:?}", e.code());
            tracing::error!("[B] Message: {}", e.message());
            Err(format!("[B] Failed: {} (code: {:?})", e.message(), e.code()))
        }
    }
}

/// Approach C: mpsc channel with ReceiverStream
async fn upload_approach_c(
    client: &mut FileServiceClient<tonic::service::interceptor::InterceptedService<Channel, AuthInterceptor>>,
    channel_id: &str,
    transfer_id: &str,
) -> Result<FileMetadata, String> {
    use tokio_stream::wrappers::ReceiverStream;

    let chunks = create_test_chunks(transfer_id);
    let chunk_count = chunks.len();

    tracing::info!("[C] mpsc channel + ReceiverStream - {} chunks", chunk_count);

    let (tx, rx) = tokio::sync::mpsc::channel::<FileChunk>(chunk_count);

    // Spawn sender task
    let sender = tokio::spawn(async move {
        for chunk in chunks {
            if tx.send(chunk).await.is_err() {
                break;
            }
        }
    });

    let stream = ReceiverStream::new(rx);

    let mut request = tonic::Request::new(stream);
    request.metadata_mut().insert("channel-id", channel_id.parse().unwrap());
    request.metadata_mut().insert("file-name", "test_c.txt".parse().unwrap());
    request.metadata_mut().insert("mime-type", "text/plain".parse().unwrap());
    request.metadata_mut().insert("content-length", "52".parse().unwrap());

    let result = client
        .upload_file(request)
        .await
        .map(|r| r.into_inner())
        .map_err(|e| format!("[C] Failed: {}", e));

    let _ = sender.await;
    result
}

/// Approach D: async_stream macro
async fn upload_approach_d(
    client: &mut FileServiceClient<tonic::service::interceptor::InterceptedService<Channel, AuthInterceptor>>,
    channel_id: &str,
    transfer_id: &str,
) -> Result<FileMetadata, String> {
    let chunks = create_test_chunks(transfer_id);
    let chunk_count = chunks.len();

    tracing::info!("[D] async_stream macro - {} chunks", chunk_count);

    let stream = async_stream::stream! {
        for chunk in chunks {
            // Small yield to allow async runtime to process
            tokio::task::yield_now().await;
            yield chunk;
        }
    };

    let mut request = tonic::Request::new(stream);
    request.metadata_mut().insert("channel-id", channel_id.parse().unwrap());
    request.metadata_mut().insert("file-name", "test_d.txt".parse().unwrap());
    request.metadata_mut().insert("mime-type", "text/plain".parse().unwrap());
    request.metadata_mut().insert("content-length", "52".parse().unwrap());

    client
        .upload_file(request)
        .await
        .map(|r| r.into_inner())
        .map_err(|e| format!("[D] Failed: {}", e))
}

/// Approach E: Buffered channel with explicit close signal
async fn upload_approach_e(
    client: &mut FileServiceClient<tonic::service::interceptor::InterceptedService<Channel, AuthInterceptor>>,
    channel_id: &str,
    transfer_id: &str,
) -> Result<FileMetadata, String> {
    use tokio_stream::wrappers::ReceiverStream;

    let chunks = create_test_chunks(transfer_id);
    let chunk_count = chunks.len();

    tracing::info!("[E] Buffered channel (buffer=1) - {} chunks", chunk_count);

    // Use buffer of 1 to create backpressure
    let (tx, rx) = tokio::sync::mpsc::channel::<FileChunk>(1);

    let stream = ReceiverStream::new(rx);

    let mut request = tonic::Request::new(stream);
    request.metadata_mut().insert("channel-id", channel_id.parse().unwrap());
    request.metadata_mut().insert("file-name", "test_e.txt".parse().unwrap());
    request.metadata_mut().insert("mime-type", "text/plain".parse().unwrap());
    request.metadata_mut().insert("content-length", "52".parse().unwrap());

    // Start upload in background
    let upload_future = client.upload_file(request);

    // Send chunks concurrently
    let sender = async move {
        for chunk in chunks {
            if tx.send(chunk).await.is_err() {
                tracing::warn!("[E] Receiver dropped");
                break;
            }
        }
        drop(tx); // Explicitly close
        tracing::debug!("[E] All chunks sent, channel closed");
    };

    // Run both concurrently
    let (upload_result, _) = tokio::join!(upload_future, sender);

    upload_result
        .map(|r| r.into_inner())
        .map_err(|e| format!("[E] Failed: {}", e))
}

/// Approach F: Direct stream passing without Request wrapper first
async fn upload_approach_f(
    client: &mut FileServiceClient<tonic::service::interceptor::InterceptedService<Channel, AuthInterceptor>>,
    channel_id: &str,
    transfer_id: &str,
) -> Result<FileMetadata, String> {
    let chunks = create_test_chunks(transfer_id);
    let chunk_count = chunks.len();

    tracing::info!("[F] Direct stream (no early Request wrap) - {} chunks", chunk_count);

    // Create stream that yields with small delays
    let stream = async_stream::stream! {
        for chunk in chunks {
            yield chunk;
        }
    };

    // Pass stream directly - tonic will wrap in Request
    let mut request = tonic::Request::new(stream);
    request.metadata_mut().insert("channel-id", channel_id.parse().unwrap());
    request.metadata_mut().insert("file-name", "test_f.txt".parse().unwrap());
    request.metadata_mut().insert("mime-type", "text/plain".parse().unwrap());
    request.metadata_mut().insert("content-length", "52".parse().unwrap());

    client
        .upload_file(request)
        .await
        .map(|r| r.into_inner())
        .map_err(|e| format!("[F] Failed: {}", e))
}

/// Run all upload approach tests
#[cfg(test)]
mod tests {
    use super::*;

    /// Integration test - requires running server
    /// Run with: cargo test test_upload_approaches -- --nocapture --ignored
    #[tokio::test]
    #[ignore = "Requires running server"]
    async fn test_upload_approaches() {
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_target(false)
            .init();

        let server_url = std::env::var("CHATTER_SERVER_URL")
            .unwrap_or_else(|_| "http://localhost:50051".to_string());

        tracing::info!("Connecting to server: {}", server_url);

        // Use streaming pool (no timeout) for upload testing
        let pool = GrpcClientPool::new_for_streaming(&server_url).expect("Failed to create pool");
        pool.connect().await.expect("Failed to connect");

        // Set test auth token (you'll need to get a valid token)
        // For now, we'll try without auth to see if that's the issue
        // pool.auth().set_bearer("your-test-token".to_string());

        let channel_id = "test-channel-id";

        // Test each approach
        let approaches = vec![
            ("A", "tokio_stream::iter"),
            ("B", "futures::stream::iter + Box::pin"),
            ("C", "mpsc + ReceiverStream"),
            ("D", "async_stream macro"),
            ("E", "Buffered channel (buffer=1)"),
            ("F", "async_stream direct"),
        ];

        let mut results = Vec::new();

        for (label, desc) in &approaches {
            let transfer_id = uuid::Uuid::new_v4().to_string();
            let mut client = FileServiceClient::with_interceptor(
                pool.channel(),
                pool.auth_clone(),
            );

            tracing::info!("\n========================================");
            tracing::info!("Testing Approach {}: {}", label, desc);
            tracing::info!("========================================");

            let result = match *label {
                "A" => upload_approach_a(&mut client, channel_id, &transfer_id).await,
                "B" => upload_approach_b(&mut client, channel_id, &transfer_id).await,
                "C" => upload_approach_c(&mut client, channel_id, &transfer_id).await,
                "D" => upload_approach_d(&mut client, channel_id, &transfer_id).await,
                "E" => upload_approach_e(&mut client, channel_id, &transfer_id).await,
                "F" => upload_approach_f(&mut client, channel_id, &transfer_id).await,
                _ => unreachable!(),
            };

            match &result {
                Ok(metadata) => {
                    tracing::info!("✅ Approach {} SUCCEEDED! File ID: {}", label, metadata.id);
                    results.push((*label, true, format!("File ID: {}", metadata.id)));
                }
                Err(e) => {
                    tracing::error!("❌ Approach {} FAILED: {}", label, e);
                    results.push((*label, false, e.clone()));
                }
            }

            // Small delay between tests
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        }

        // Summary
        tracing::info!("\n========================================");
        tracing::info!("RESULTS SUMMARY");
        tracing::info!("========================================");

        for (label, success, msg) in &results {
            let status = if *success { "✅ PASS" } else { "❌ FAIL" };
            tracing::info!("{} [{}]: {}", status, label, msg);
        }

        let passed = results.iter().filter(|(_, s, _)| *s).count();
        tracing::info!("\nTotal: {}/{} approaches succeeded", passed, results.len());
    }

    /// Quick test with just approach A
    /// Run with: CHATTER_AUTH_TOKEN=xxx cargo test test_approach_a_only -- --nocapture --ignored
    #[tokio::test]
    #[ignore = "Requires running server"]
    async fn test_approach_a_only() {
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .init();

        let pool = GrpcClientPool::new_for_streaming("http://localhost:50051")
            .expect("Failed to create pool");
        pool.connect().await.expect("Failed to connect");

        // Set auth token from environment if available
        if let Ok(token) = std::env::var("CHATTER_AUTH_TOKEN") {
            tracing::info!("Using auth token from CHATTER_AUTH_TOKEN");
            pool.auth().set_bearer(token);
        } else {
            tracing::warn!("No CHATTER_AUTH_TOKEN set, request may fail auth");
        }

        let mut client = FileServiceClient::with_interceptor(
            pool.channel(),
            pool.auth_clone(),
        );

        let result = upload_approach_a(
            &mut client,
            "test-channel",
            &uuid::Uuid::new_v4().to_string(),
        ).await;

        match result {
            Ok(m) => tracing::info!("Success: {:?}", m),
            Err(e) => tracing::error!("Failed: {}", e),
        }
    }

    /// Test with both pool types to compare
    #[tokio::test]
    #[ignore = "Requires running server"]
    async fn test_pool_timeout_comparison() {
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .init();

        let server_url = "http://localhost:50051";

        // Test 1: Pool with 30s timeout
        tracing::info!("\n=== Testing with 30s timeout pool ===");
        let pool_timeout = GrpcClientPool::new(server_url).expect("Failed to create pool");
        pool_timeout.connect().await.expect("Failed to connect");

        if let Ok(token) = std::env::var("CHATTER_AUTH_TOKEN") {
            pool_timeout.auth().set_bearer(token.clone());
        }

        let mut client1 = FileServiceClient::with_interceptor(
            pool_timeout.channel(),
            pool_timeout.auth_clone(),
        );
        let result1 = upload_approach_a(&mut client1, "test-channel", &uuid::Uuid::new_v4().to_string()).await;
        tracing::info!("With timeout: {:?}", result1.is_ok());

        // Test 2: Pool without timeout
        tracing::info!("\n=== Testing with no-timeout pool ===");
        let pool_no_timeout = GrpcClientPool::new_for_streaming(server_url).expect("Failed to create pool");
        pool_no_timeout.connect().await.expect("Failed to connect");

        if let Ok(token) = std::env::var("CHATTER_AUTH_TOKEN") {
            pool_no_timeout.auth().set_bearer(token);
        }

        let mut client2 = FileServiceClient::with_interceptor(
            pool_no_timeout.channel(),
            pool_no_timeout.auth_clone(),
        );
        let result2 = upload_approach_a(&mut client2, "test-channel", &uuid::Uuid::new_v4().to_string()).await;
        tracing::info!("Without timeout: {:?}", result2.is_ok());

        tracing::info!("\n=== Summary ===");
        tracing::info!("With 30s timeout: {}", if result1.is_ok() { "✅ PASS" } else { "❌ FAIL" });
        tracing::info!("Without timeout: {}", if result2.is_ok() { "✅ PASS" } else { "❌ FAIL" });
    }
}

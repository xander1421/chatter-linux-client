# AttachmentService

**File**: `src/services/attachment_service.rs`

File upload and download operations with chunking and verification.

## Purpose

AttachmentService handles all file operations - uploading attachments to server, verifying uploads, and (future) downloading attachments.

## Methods

### Upload File

```rust
/// Upload a file to a channel
pub async fn upload_file(
    &self,
    grpc_pool: &GrpcClientPool,
    file_path: &Path,
    channel_id: &ChannelId,
) -> Result<UploadResult, AttachmentError>
```

**Returns**:
```rust
pub struct UploadResult {
    pub attachment_id: AttachmentId,  // Server-assigned ID
    pub filename: String,
    pub file_size: u64,
    pub mime_type: String,
}
```

**What it does**:
1. Reads file metadata (size, MIME type)
2. Chunks file into 256KB pieces
3. Generates SHA256 auth tag per chunk
4. Generates transfer_id UUID
5. Streams chunks via gRPC
6. Handles h2 protocol errors with verification fallback
7. Saves attachment to local repository
8. Returns UploadResult

### Verify Upload (Internal)

```rust
/// Verify upload succeeded by checking channel files
async fn verify_upload(
    &self,
    grpc_pool: &GrpcClientPool,
    channel_id: &ChannelId,
    filename: &str,
    file_size: u64,
) -> Result<FileMetadata, AttachmentError>
```

Used as fallback when h2 protocol errors occur - the upload may have succeeded despite the error.

## Error Types

```rust
pub enum AttachmentError {
    /// File system error
    FileError(String),

    /// Network error during upload/download
    NetworkError(String),

    /// Invalid attachment data
    InvalidAttachment(String),

    /// Storage error
    StorageError(String),
}
```

## Upload Flow

```
User clicks attachment button
    │
    ▼
File picker → selected files
    │
    ▼
AttachmentService::upload_file()
    │
    ├─→ Read file metadata
    │   (size, MIME type from extension)
    │
    ├─→ Read file content
    │
    ├─→ Chunk into 256KB pieces
    │   │
    │   └─→ Per chunk:
    │       - sequence number
    │       - SHA256 auth tag (16 bytes)
    │       - data bytes
    │
    ├─→ Stream via FileClient::upload_file_with_metadata()
    │   │
    │   ├─→ Success → FileMetadata
    │   │
    │   └─→ h2 error → verify_upload() fallback
    │
    ├─→ Save to AttachmentRepository
    │
    └─→ Return UploadResult
```

## Usage Example

```rust
// In composer.rs - UI callback
window.on_attachment_clicked(move || {
    let attachment_service = AttachmentService::new(attachment_repo.clone());
    let grpc_pool = grpc_pool.clone();
    let chat_state = chat_state.clone();

    tokio::spawn(async move {
        // Pick files
        let selected_files = FilePicker::pick_files(config).await;

        // Get active channel
        let channel_id = chat_state.get().active_channel_id?;

        // Upload each file
        for file_path in &selected_files {
            match attachment_service.upload_file(&grpc_pool, file_path, &channel_id).await {
                Ok(result) => {
                    // Add to pending attachments
                    let pending = PendingAttachment::new(
                        result.attachment_id,
                        result.filename,
                        file_path.to_path_buf(),
                        result.file_size,
                        result.mime_type,
                    );
                    chat_state.add_pending_attachment(pending);
                }
                Err(e) => tracing::error!("Upload failed: {}", e),
            }
        }

        // Update UI
        // ...
    });
});
```

## Chunk Format

Each chunk sent to server:

```rust
FileChunk {
    transfer_id: String,     // UUID for this upload session
    sequence: u32,           // 0, 1, 2, ...
    data: Vec<u8>,           // Up to 256KB
    auth_tag: Vec<u8>,       // SHA256(data)[..16]
}
```

## What NOT to Do

```rust
// ❌ WRONG - Upload logic in UI callback (230+ lines!)
window.on_attachment_clicked(move || {
    let file_content = tokio::fs::read(file_path).await?;
    let chunks: Vec<FileChunk> = file_content
        .chunks(CHUNK_SIZE)
        .enumerate()
        .map(|(seq, chunk_data)| {
            let mut hasher = Sha256::new();
            hasher.update(chunk_data);
            // ... 200 more lines of chunk building, streaming, etc.
        })
        .collect();
    // ...
});

// ✅ CORRECT - Delegate to AttachmentService
window.on_attachment_clicked(move || {
    let attachment_service = AttachmentService::new(attachment_repo.clone());
    tokio::spawn(async move {
        let result = attachment_service.upload_file(&grpc_pool, &file_path, &channel_id).await?;
        // Just handle result
    });
});
```

## Dependencies

- `AttachmentRepository` - Local attachment storage
- `FileClient` - gRPC file upload
- `GrpcClientPool` - Connection pool

## Future Work

- `download_file()` - Download attachment from server
- Chunked download with progress
- Encryption/decryption for attachment content
- Thumbnail generation
- Caching layer

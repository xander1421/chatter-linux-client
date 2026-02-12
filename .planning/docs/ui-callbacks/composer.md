# Composer Callbacks

**File**: `src/bin_ui/features/composer.rs`

UI callbacks for the message composer - attachments, emoji, GIF, mentions.

## Callbacks

| Callback | Purpose | Status |
|----------|---------|--------|
| `on_attachment_clicked` | File picker + upload | ✅ Implemented |
| `on_remove_attachment` | Remove pending attachment | ✅ Implemented |
| `on_emoji_clicked` | Toggle emoji picker | Callback wired, UI not built |
| `on_gif_clicked` | Toggle GIF picker | Callback wired, UI not built |
| `on_mention_triggered` | Mention autocomplete | Callback wired, logic not built |
| `on_process_text` | Word-break long text | ✅ Implemented |

## on_attachment_clicked

Opens file picker, uploads selected files, adds to pending attachments.

```rust
window.on_attachment_clicked(move || {
    // 1. Open file picker
    // 2. Upload each file via AttachmentService
    // 3. Add to pending attachments in ChatState
    // 4. Update UI model
});
```

### Correct Pattern

```rust
// Delegate to AttachmentService
let attachment_service = AttachmentService::new(attachment_repo.clone());

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
slint::invoke_from_event_loop(move || {
    let slint_attachments: Vec<PendingAttachmentData> = attachments
        .iter()
        .map(pending_attachment_to_slint)
        .collect();
    w.set_pending_attachments(ModelRc::new(VecModel::from(slint_attachments)));
});
```

### What NOT to Do

```rust
// ❌ WRONG - Upload logic inline (was 230+ lines!)
window.on_attachment_clicked(move || {
    // Reading file
    let file_content = tokio::fs::read(file_path).await?;

    // Chunking
    let chunks: Vec<FileChunk> = file_content
        .chunks(CHUNK_SIZE)
        .enumerate()
        .map(|(seq, chunk_data)| {
            let mut hasher = Sha256::new();
            hasher.update(chunk_data);
            let hash = hasher.finalize();
            FileChunk {
                transfer_id: transfer_id.clone(),
                sequence: seq as u32,
                data: chunk_data.to_vec(),
                auth_tag: hash[..16].to_vec(),
            }
        })
        .collect();

    // Streaming upload
    let chunk_stream = tokio_stream::iter(chunks);
    file_client.upload_file_with_metadata(...).await?;

    // Error handling, verification...
    // ... 200 more lines
});

// ✅ CORRECT - Delegate to service (10 lines)
let attachment_service = AttachmentService::new(attachment_repo.clone());
let result = attachment_service.upload_file(&grpc_pool, file_path, &channel_id).await?;
```

## on_remove_attachment

Removes a pending attachment before sending.

```rust
window.on_remove_attachment(move |attachment_id: SharedString| {
    let id: AttachmentId = attachment_id.parse()?;
    chat_state.remove_pending_attachment(id);
    // Update UI model
});
```

## on_process_text

Processes text for display - inserts zero-width spaces for word wrapping.

```rust
window.on_process_text(|text: SharedString| -> SharedString {
    if needs_word_breaks(text.as_str()) {
        insert_word_breaks(text.as_str()).into()
    } else {
        text
    }
});
```

This is a pure transformation - no service call needed.

## Pending Attachments State

Stored in `ChatState`:

```rust
// ChatState methods
chat_state.add_pending_attachment(pending);
chat_state.remove_pending_attachment(id);
chat_state.get_pending_attachments() -> Vec<PendingAttachment>
chat_state.get_pending_attachment_ids() -> Vec<AttachmentId>
chat_state.clear_pending_attachments();
```

## UI Update Pattern

```rust
// Update UI from async context
slint::invoke_from_event_loop(move || {
    if let Some(w) = weak.upgrade() {
        // Convert to Slint types
        let slint_attachments: Vec<PendingAttachmentData> = attachments
            .iter()
            .map(pending_attachment_to_slint)
            .collect();

        // Set model
        w.set_pending_attachments(ModelRc::new(VecModel::from(slint_attachments)));

        // Update placeholder
        let placeholder = match attachments.len() {
            0 => "Type a message...".to_string(),
            1 => "1 file attached - type a message...".to_string(),
            n => format!("{} files attached - type a message...", n),
        };
        w.set_composer_placeholder(placeholder.into());
    }
});
```

## Future Work

| Callback | What's Needed |
|----------|---------------|
| `on_emoji_clicked` | EmojiPicker Slint component |
| `on_gif_clicked` | GifPicker component + Tenor/Giphy API |
| `on_mention_triggered` | User search + autocomplete UI |

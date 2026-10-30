//! Unit tests for attachment operations.
//!
//! Tests attachment upload, download, encryption,
//! and metadata handling using the testing framework.

use crate::framework;

use framework::prelude::*;

// ============================================================================
// Attachment Builder Tests
// ============================================================================

#[tokio::test]
async fn test_image_attachment() {
    let attachment = AttachmentBuilder::image("photo.jpg").build();

    assert_eq!(attachment.filename, "photo.jpg");
    assert_eq!(attachment.mime_type, "image/jpeg");
    assert_eq!(attachment.size, 100 * 1024); // Default 100KB
}

#[tokio::test]
async fn test_video_attachment() {
    let attachment = AttachmentBuilder::video("video.mp4").build();

    assert_eq!(attachment.filename, "video.mp4");
    assert_eq!(attachment.mime_type, "video/mp4");
    assert_eq!(attachment.size, 10 * 1024 * 1024); // Default 10MB
}

#[tokio::test]
async fn test_audio_attachment() {
    let attachment = AttachmentBuilder::audio("podcast.mp3").build();

    assert_eq!(attachment.filename, "podcast.mp3");
    assert_eq!(attachment.mime_type, "audio/mp3");
    assert_eq!(attachment.size, 5 * 1024 * 1024); // Default 5MB
}

#[tokio::test]
async fn test_document_attachment() {
    let attachment = AttachmentBuilder::document("report.pdf").build();

    assert_eq!(attachment.filename, "report.pdf");
    assert_eq!(attachment.mime_type, "application/pdf");
    assert_eq!(attachment.size, 1024 * 1024); // Default 1MB
}

#[tokio::test]
async fn test_custom_mime_type() {
    let attachment = AttachmentBuilder::document("data.json")
        .mime_type("application/json")
        .build();

    assert_eq!(attachment.mime_type, "application/json");
}

#[tokio::test]
async fn test_custom_size() {
    let attachment = AttachmentBuilder::image("large.jpg")
        .size(50 * 1024 * 1024) // 50MB
        .build();

    assert_eq!(attachment.size, 50 * 1024 * 1024);
}

#[tokio::test]
async fn test_cdn_url() {
    let attachment = AttachmentBuilder::image("photo.jpg")
        .cdn_url("https://cdn.example.com/attachments/abc123")
        .build();

    assert_eq!(
        attachment.cdn_url,
        Some("https://cdn.example.com/attachments/abc123".to_string())
    );
}

// ============================================================================
// Attachment Encryption Tests
// ============================================================================

#[tokio::test]
async fn test_attachment_has_encryption_key() {
    let attachment = AttachmentBuilder::image("encrypted.jpg").build();

    assert_eq!(
        attachment.key.len(),
        32,
        "Encryption key should be 32 bytes (AES-256)"
    );
}

#[tokio::test]
async fn test_attachment_has_digest() {
    let attachment = AttachmentBuilder::image("verified.jpg").build();

    assert_eq!(
        attachment.digest.len(),
        32,
        "Digest should be 32 bytes (SHA-256)"
    );
}

#[tokio::test]
async fn test_unique_encryption_keys() {
    let att1 = AttachmentBuilder::image("1.jpg").build();
    let att2 = AttachmentBuilder::image("2.jpg").build();

    assert_ne!(
        att1.key, att2.key,
        "Each attachment should have unique encryption key"
    );
}

#[tokio::test]
async fn test_unique_digests() {
    let att1 = AttachmentBuilder::image("1.jpg").build();
    let att2 = AttachmentBuilder::image("2.jpg").build();

    assert_ne!(
        att1.digest, att2.digest,
        "Each attachment should have unique digest"
    );
}

// ============================================================================
// Attachment ID Tests
// ============================================================================

#[tokio::test]
async fn test_attachment_has_unique_id() {
    let att1 = AttachmentBuilder::image("1.jpg").build();
    let att2 = AttachmentBuilder::image("2.jpg").build();

    assert!(!att1.id.is_empty());
    assert!(!att2.id.is_empty());
    assert_ne!(att1.id, att2.id, "Attachments should have unique IDs");
}

// ============================================================================
// Message with Attachments Tests
// ============================================================================

#[tokio::test]
async fn test_message_single_attachment() {
    let attachment = AttachmentBuilder::image("photo.jpg").build();

    let msg = MessageBuilder::text("Check out this photo!")
        .with_attachment(attachment)
        .build();

    assert_eq!(msg.attachments.len(), 1);
    assert_eq!(msg.attachments[0].filename, "photo.jpg");
}

#[tokio::test]
async fn test_message_multiple_attachments() {
    let img = AttachmentBuilder::image("photo.jpg").build();
    let doc = AttachmentBuilder::document("notes.pdf").build();
    let video = AttachmentBuilder::video("demo.mp4").build();

    let msg = MessageBuilder::text("Here are the files")
        .with_attachment(img)
        .with_attachment(doc)
        .with_attachment(video)
        .build();

    assert_eq!(msg.attachments.len(), 3);
}

#[tokio::test]
async fn test_message_attachment_types() {
    let img = AttachmentBuilder::image("photo.jpg").build();
    let audio = AttachmentBuilder::audio("voice.mp3").build();

    let msg = MessageBuilder::text("Voice note with photo")
        .with_attachment(img)
        .with_attachment(audio)
        .build();

    let mime_types: Vec<_> = msg.attachments.iter().map(|a| &a.mime_type).collect();
    assert!(mime_types.contains(&&"image/jpeg".to_string()));
    assert!(mime_types.contains(&&"audio/mp3".to_string()));
}

// ============================================================================
// Attachment Size Tests
// ============================================================================

#[tokio::test]
async fn test_small_attachment() {
    let attachment = AttachmentBuilder::document("tiny.txt")
        .size(100) // 100 bytes
        .build();

    assert_eq!(attachment.size, 100);
}

#[tokio::test]
async fn test_large_attachment() {
    let attachment = AttachmentBuilder::video("movie.mp4")
        .size(2 * 1024 * 1024 * 1024) // 2GB
        .build();

    assert_eq!(attachment.size, 2 * 1024 * 1024 * 1024);
}

#[tokio::test]
async fn test_zero_size_attachment() {
    let attachment = AttachmentBuilder::document("empty.txt").size(0).build();

    assert_eq!(attachment.size, 0);
}

// ============================================================================
// Attachment Filename Tests
// ============================================================================

#[tokio::test]
async fn test_filename_with_spaces() {
    let attachment = AttachmentBuilder::document("my document.pdf").build();
    assert_eq!(attachment.filename, "my document.pdf");
}

#[tokio::test]
async fn test_filename_with_special_chars() {
    let attachment = AttachmentBuilder::image("photo (1).jpg").build();
    assert_eq!(attachment.filename, "photo (1).jpg");
}

#[tokio::test]
async fn test_unicode_filename() {
    let attachment = AttachmentBuilder::document("文档.pdf").build();
    assert_eq!(attachment.filename, "文档.pdf");
}

// ============================================================================
// Fixture Helper Tests
// ============================================================================

#[tokio::test]
async fn test_fake_cdn_url() {
    let url = fake_cdn_url();

    assert!(url.starts_with("https://cdn.example.com/attachments/"));
    assert!(url.len() > 40); // Should have UUID suffix
}

#[tokio::test]
async fn test_unique_cdn_urls() {
    let url1 = fake_cdn_url();
    let url2 = fake_cdn_url();

    assert_ne!(url1, url2, "CDN URLs should be unique");
}

// ============================================================================
// Storage Integration Tests
// ============================================================================

#[tokio::test]
async fn test_attachment_in_stored_message() {
    use framework::mocks::InMemoryStorage;

    let storage = InMemoryStorage::new();

    let attachment = AttachmentBuilder::image("stored.jpg").build();
    let msg = MessageBuilder::text("Message with attachment")
        .in_channel("channel-1")
        .with_attachment(attachment)
        .build();

    let msg_id = msg.id.clone();
    storage.save_message(msg);

    let retrieved = storage.get_message(&msg_id).unwrap();
    assert_eq!(retrieved.attachments.len(), 1);
    assert_eq!(retrieved.attachments[0].filename, "stored.jpg");
}

// ============================================================================
// Attachment Type Detection Tests
// ============================================================================

#[tokio::test]
async fn test_image_types() {
    let jpg = AttachmentBuilder::image("photo.jpg").build();
    let png = AttachmentBuilder::image("icon.png")
        .mime_type("image/png")
        .build();
    let gif = AttachmentBuilder::image("animation.gif")
        .mime_type("image/gif")
        .build();

    assert_eq!(jpg.mime_type, "image/jpeg");
    assert_eq!(png.mime_type, "image/png");
    assert_eq!(gif.mime_type, "image/gif");
}

#[tokio::test]
async fn test_video_types() {
    let mp4 = AttachmentBuilder::video("video.mp4").build();
    let webm = AttachmentBuilder::video("video.webm")
        .mime_type("video/webm")
        .build();
    let mov = AttachmentBuilder::video("video.mov")
        .mime_type("video/quicktime")
        .build();

    assert_eq!(mp4.mime_type, "video/mp4");
    assert_eq!(webm.mime_type, "video/webm");
    assert_eq!(mov.mime_type, "video/quicktime");
}

#[tokio::test]
async fn test_audio_types() {
    let mp3 = AttachmentBuilder::audio("song.mp3").build();
    let wav = AttachmentBuilder::audio("sound.wav")
        .mime_type("audio/wav")
        .build();
    let ogg = AttachmentBuilder::audio("audio.ogg")
        .mime_type("audio/ogg")
        .build();

    assert_eq!(mp3.mime_type, "audio/mp3");
    assert_eq!(wav.mime_type, "audio/wav");
    assert_eq!(ogg.mime_type, "audio/ogg");
}

// ============================================================================
// Harness Integration Tests
// ============================================================================

#[tokio::test]
async fn test_harness_attachment_message_flow() {
    let harness = TestHarness::unit().await;
    let (alice, _bob) = harness.conversation_pair().await;

    let channel = ChannelBuilder::public("media-channel")
        .created_by(alice.user_id())
        .build();
    let channel_id = harness.mock_server().create_channel(channel);

    // Alice sends a message with attachment
    let attachment = AttachmentBuilder::image("vacation.jpg")
        .size(2 * 1024 * 1024)
        .build();

    let msg = MessageBuilder::text("My vacation photo!")
        .in_channel(&channel_id)
        .from(alice.user_id())
        .with_attachment(attachment)
        .build();

    harness.mock_server().store_message(msg);

    // Verify message with attachment
    let messages = harness.mock_server().get_messages(&channel_id, 10);
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].attachments.len(), 1);
    assert_eq!(messages[0].attachments[0].filename, "vacation.jpg");
}

// ============================================================================
// Crypto Key Verification Tests
// ============================================================================

#[tokio::test]
async fn test_attachment_key_length() {
    // AES-256 key should be 32 bytes
    let attachment = AttachmentBuilder::image("secure.jpg").build();
    assert_eq!(attachment.key.len(), 32);
}

#[tokio::test]
async fn test_attachment_digest_length() {
    // SHA-256 digest should be 32 bytes
    let attachment = AttachmentBuilder::image("verified.jpg").build();
    assert_eq!(attachment.digest.len(), 32);
}

#[tokio::test]
async fn test_attachment_keys_are_random() {
    // Generate many attachments and verify keys are random
    let attachments: Vec<_> = (0..10)
        .map(|i| AttachmentBuilder::image(&format!("{}.jpg", i)).build())
        .collect();

    let keys: std::collections::HashSet<_> = attachments.iter().map(|a| a.key.clone()).collect();

    assert_eq!(keys.len(), 10, "All keys should be unique");
}

// ============================================================================
// Media Message Builder Tests
// ============================================================================

#[tokio::test]
async fn test_image_message_builder() {
    let msg = MessageBuilder::image().build();

    assert_eq!(msg.attachments.len(), 1);
    assert!(msg.attachments[0].mime_type.starts_with("image/"));
}

#[tokio::test]
async fn test_video_message_builder() {
    let msg = MessageBuilder::video().build();

    assert_eq!(msg.attachments.len(), 1);
    assert!(msg.attachments[0].mime_type.starts_with("video/"));
}

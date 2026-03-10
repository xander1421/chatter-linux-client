//! Attachment auto-download for DMs.
//!
//! Uses shared attachment_download module for consistent handling across all channel types.
//! Decrypts once and reuses bytes for GIF animation - no redundant decryption.

use std::sync::Arc;

use chatter_linux::app::UIContext;
use chatter_linux::proto::content_v1::AttachmentPointer;

use crate::bin_ui::features::auth::inbox::attachment_download;
use crate::MainWindow;

/// Auto-download all attachments in background.
///
/// Downloads encrypted content to filesystem (no temporary files) as soon as pointer arrives.
/// For images, generates thumbnail. For GIFs, registers for inline animation.
///
/// Uses shared attachment_download module which:
/// - Decrypts only once
/// - Reuses decrypted bytes for GIF animation (no re-decrypt)
/// - Handles both GIFs and images consistently
pub async fn auto_download_attachments(
    pointers: Vec<AttachmentPointer>,
    ui_ctx: Arc<UIContext>,
    weak_window: slint::Weak<MainWindow>,
    peer_id: String,
) {
    attachment_download::download_attachments(
        pointers,
        ui_ctx.attachment_service.clone(),
        ui_ctx.grpc_pool.clone(),
        weak_window,
        peer_id,
    ).await;
}

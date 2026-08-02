//! Chat action callbacks.
//!
//! Handles message interactions (reply, react, thread, more, view attachment).
//!
//! This module is organized into:
//! - `timers` - Audio/video progress timers
//! - `code_preview` - Code file preview with syntax highlighting
//! - `pdf_preview` - PDF file preview
//! - `video_player` - Video playback controls
//! - `message_actions` - Message click, reply, react, thread callbacks
//! - `attachment_view` - Main attachment viewing handler
//! - `audio_seek` - Audio seek callback
//! - `mini_player` - Persistent audio player strip
//! - `transfer_status` - Download/upload progress updates

use std::sync::Arc;

use chatter_core::client::ChatterClient;
use crate::MainWindow;

mod attachment_view;
mod audio_seek;
mod code_preview;
mod message_actions;
pub mod mini_player;
mod pdf_preview;
mod timers;
mod transfer_status;
mod video_player;


/// Wire message action callbacks.
pub fn wire(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    message_actions::wire_message_clicked(window);
    message_actions::wire_quote_clicked(window);
    message_actions::wire_reply_clicked(window, ui_ctx);
    message_actions::wire_cancel_reply(window);
    message_actions::wire_react_clicked(window);
    message_actions::wire_thread_clicked(window, ui_ctx);
    message_actions::wire_close_image_viewer(window);
    attachment_view::wire_view_attachment(window, ui_ctx);
    audio_seek::wire_audio_seek(window);
    mini_player::wire(window, ui_ctx);
    code_preview::wire_code_preview_callbacks(window);
    pdf_preview::wire_pdf_preview_callbacks(window);
    video_player::wire_video_player_callbacks(window);
}

//! Drag and drop file handling for the composer.
//!
//! Allows users to drag files from the file manager directly into the window
//! to attach them to messages.

use std::path::PathBuf;

use i_slint_backend_winit::{EventResult, WinitWindowAccessor};
use slint::ComponentHandle;
use winit::event::WindowEvent;

use chatter_core::state::{ActiveConversation, ChatState};

use super::attachments::process_and_add_file;
use crate::MainWindow;

/// Wire up drag and drop file handling.
///
/// Files dropped anywhere on the window will be added as pending attachments
/// if there's an active conversation.
pub(crate) fn wire_drag_drop(window: &MainWindow, chat_state: ChatState) {
    let weak = window.as_weak();
    let state = chat_state.clone();

    window.window().on_winit_window_event(move |_win, event| {
        match event {
            WindowEvent::DroppedFile(path) => {
                handle_dropped_file(path.clone(), &state, &weak);
                EventResult::PreventDefault
            }
            WindowEvent::HoveredFile(_path) => {
                // Could show a visual drop indicator here in the future
                tracing::trace!("File hovering over window");
                EventResult::Propagate
            }
            WindowEvent::HoveredFileCancelled => {
                // Could hide the visual drop indicator here
                tracing::trace!("File hover cancelled");
                EventResult::Propagate
            }
            _ => EventResult::Propagate,
        }
    });

    tracing::debug!("Drag and drop file handling wired");
}

/// Handle a file dropped onto the window.
fn handle_dropped_file(
    path: PathBuf,
    chat_state: &ChatState,
    weak: &slint::Weak<MainWindow>,
) {
    tracing::info!("File dropped: {:?}", path);

    // Check if we have an active conversation
    if matches!(chat_state.active_conversation(), ActiveConversation::None) {
        tracing::warn!("File dropped but no active conversation - ignoring");
        return;
    }

    // Process the file asynchronously
    let chat_state = chat_state.clone();
    let weak = weak.clone();

    tokio::spawn(async move {
        if let Err(e) = process_and_add_file(path, &chat_state, &weak).await {
            tracing::error!("Failed to process dropped file: {}", e);
        }
    });
}

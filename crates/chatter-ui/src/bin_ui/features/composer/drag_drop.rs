//! Drag and drop file handling for the composer.
//!
//! Allows users to drag files from the file manager directly into the window
//! to attach them to messages.

use std::path::PathBuf;
use std::sync::Arc;

use i_slint_backend_winit::{EventResult, WinitWindowAccessor};
use slint::ComponentHandle;
use winit::event::WindowEvent;

use chatter_core::client::ChatterClient;
use chatter_core::state::ActiveConversation;

use super::attachments::process_and_add_file;
use crate::MainWindow;

/// Wire up drag and drop file handling, and report window focus to core
/// (flushing pending drafts when focus leaves).
///
/// Files dropped anywhere on the window will be added as pending attachments
/// if there's an active conversation.
///
/// Focus lives here because a window has exactly one winit event filter —
/// registering a second one elsewhere would silently replace this one. A
/// window without focus is "not visible" to core: nothing is marked read or
/// receipted until the user is back (`ChatterClient::set_app_visible`).
pub(crate) fn wire_drag_drop(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak = window.as_weak();
    let state = ui_ctx.clone();
    let ui_ctx = ui_ctx.clone();

    window.window().on_winit_window_event(move |_win, event| {
        match event {
            WindowEvent::Focused(focused) => {
                let _rt = crate::rt_handle().enter();
                if !*focused {
                    // Leaving the window is the desktop's lifecycle edge: a
                    // session logout or kill may follow, so store the draft
                    // now rather than after the autosave delay.
                    if let Some(w) = weak.upgrade() {
                        super::flush_drafts(&w, &ui_ctx);
                    }
                }
                ui_ctx.set_app_visible(*focused);
                EventResult::Propagate
            }
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
    chat_state: &Arc<ChatterClient>,
    weak: &slint::Weak<MainWindow>,
) {
    let Some(window) = weak.upgrade() else { return };
    if window.get_current_view().as_str() != "main"
        || window.get_active_modal().as_str() != "none"
        || window.global::<crate::SearchPane>().get_unified_search_open()
        || window.global::<crate::SidebarPane>().get_active_tab().as_str() != "chats"
        || window.global::<crate::ChatPane>().get_thread_panel_visible()
    {
        show_drop_error(weak, "Open a chat to attach a file.");
        return;
    }

    let snapshot = chat_state.chat_snapshot();
    let conversation = snapshot.active_conversation;
    let revision = snapshot.attachment_revision;
    // Check if we have an active conversation
    if matches!(conversation, ActiveConversation::None) {
        show_drop_error(weak, "Open a conversation before dropping a file.");
        return;
    }

    // Process the file asynchronously
    let chat_state = chat_state.clone();
    let weak = weak.clone();

    tokio::spawn(async move {
        if let Err(e) =
            process_and_add_file(path, None, &chat_state, conversation, revision, &weak).await
        {
            tracing::warn!("Dropped file could not be staged: {e}");
            show_drop_error(&weak, "Couldn't attach that file. Check that it is still available and try again.");
        }
    });
}

fn show_drop_error(weak: &slint::Weak<MainWindow>, message: &'static str) {
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(window) = weak.upgrade() {
            window.set_toast_message(message.into());
            window.set_toast_variant("error".into());
            window.set_toast_visible(true);
        }
    });
}

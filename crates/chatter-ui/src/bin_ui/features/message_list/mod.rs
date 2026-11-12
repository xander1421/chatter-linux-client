//! The message list: viewport, retained history window and paging,
//! jump-to-message, and the pinned-message bar above it.

mod history;
pub(crate) mod jump;
pub(crate) mod pinned_bar;
pub(crate) mod viewport;

use std::sync::Arc;

use slint::ComponentHandle;

use chatter_core::client::ChatterClient;

use crate::MainWindow;

/// Wire message-list callbacks.
pub fn wire(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    window
        .global::<crate::VirtualizerTrace>()
        .on_settle_phase(viewport::trace_viewport_settle);
    pinned_bar::wire_pinned_navigation(window, ui_ctx);
    history::wire_load_more(window, ui_ctx);
    viewport::wire_visible_media(window, ui_ctx);
}

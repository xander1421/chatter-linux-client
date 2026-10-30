//! Settings modal callbacks.

mod account;
mod ai;
mod appearance;
mod message_backup;
mod notifications;
mod privacy;
mod recovery;

use std::sync::Arc;

use slint::{ComponentHandle, Model, SharedString};

use chatter_core::client::{ChatterClient, ClientError, ImportSummary};

use crate::MainWindow;

pub fn wire_settings_callbacks(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    window
        .global::<crate::ModalHostState>()
        .set_third_party_notices(slint::ModelRc::new(slint::VecModel::from(
            // One row per blank-line-separated block: the list only lays out
            // what is on screen, where one 2 MB text field took seconds.
            include_str!("../../../../../../../assets/licenses/THIRD_PARTY_NOTICES.txt")
                .split("\n\n")
                .filter(|block| !block.trim().is_empty())
                .map(slint::SharedString::from)
                .collect::<Vec<_>>(),
        )));
    if let Some((height, width)) = ui_ctx.composer_keyboard_geometry() {
        window.set_remembered_keyboard_height_px(height);
        window.set_remembered_keyboard_width_px(width);
    }
    let ui_ctx_keyboard = ui_ctx.clone();
    window.on_composer_keyboard_height_learned(move |height, width| {
        if let Err(error) = ui_ctx_keyboard.set_composer_keyboard_geometry(height, width) {
            tracing::warn!("Failed to save keyboard geometry: {error}");
        }
    });

    account::wire_credentials(window, ui_ctx);
    privacy::wire_preferences(window, ui_ctx);
    appearance::wire(window, ui_ctx);
    notifications::wire(window, ui_ctx);
    privacy::wire_identity(window, ui_ctx);
    recovery::wire_pin(window, ui_ctx);
    message_backup::wire(window, ui_ctx);
    recovery::wire_codes(window, ui_ctx);
    account::wire_profile_and_deletion(window, ui_ctx);
    ai::wire(window, ui_ctx);
}

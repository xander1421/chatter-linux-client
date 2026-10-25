//! Explicit user-tap web destinations; never launch message text automatically.
use crate::MainWindow;
use crate::bin_ui::converters::markdown::safe_web_url;
use slint::ComponentHandle;
use std::sync::Arc;
use chatter_core::client::ChatterClient;

pub fn wire(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak = window.as_weak();
    let client = ui_ctx.clone();
    window
        .global::<crate::ChatPane>()
        .on_open_message_link(move |value| {
            if let Some(id) = value.as_str().strip_prefix("chatter-mention:") {
                let Ok(user_id) = id.parse::<chatter_core::domain::ids::UserId>() else { return; };
                let Some(w) = weak.upgrade() else { return; };
                crate::bin_ui::features::modals::open_user_profile_in_conversation(
                    &w, &user_id, Some(&client), client.active_conversation(),
                );
                crate::bin_ui::features::back::push(&w, "user-profile");
                return;
            }
            let Some(url) = safe_web_url(value.as_str()) else {
                return;
            };
            #[cfg(target_os = "android")]
            let result = crate::platform::android::media::open_web_link(&url);
            #[cfg(not(any(target_os = "android", target_os = "ios")))]
            let result = chatter_core::system::ExternalOpener
                .open_url(&url)
                .map_err(|_| ());
            #[cfg(target_os = "ios")]
            let result: Result<(), ()> = Err(());
            if result.is_err() {
                tracing::warn!("Browser launch failed");
            }
        });
}

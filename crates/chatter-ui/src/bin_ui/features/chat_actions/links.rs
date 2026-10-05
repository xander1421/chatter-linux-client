//! Explicit user-tap web destinations; never launch message text automatically.
use crate::MainWindow;
use crate::bin_ui::converters::markdown::safe_web_url;

pub fn wire(window: &MainWindow) {
    window.on_open_message_link(|value| {
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

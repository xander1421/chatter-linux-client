//! Settings notifications callbacks.

use super::*;

pub(super) fn wire(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    // Account push delivery is read from and saved to the server. Keep this
    // separate from device sound/OS permissions and notification privacy.
    let delivery = window.global::<crate::NotificationDelivery>();
    let weak_delivery = window.as_weak();
    let client_delivery = ui_ctx.clone();
    delivery.on_load(move || {
        update_notification_delivery(&weak_delivery, &client_delivery, None);
    });
    let weak_delivery = window.as_weak();
    let client_delivery = ui_ctx.clone();
    delivery.on_push_enabled_changed(move |enabled| {
        update_notification_delivery(&weak_delivery, &client_delivery, Some(enabled));
    });

    // Sound and vibration (Settings › Notifications). Android keeps both on
    // the OS notification channel, so the row opens its page; desktop
    // notifiers take a sound flag; iOS has neither, so the row is hidden.
    let sound = window.global::<crate::NotificationSound>();
    #[cfg(target_os = "android")]
    {
        sound.set_mode("system".into());
        sound.on_open_system_settings(
            crate::platform::android::system_settings::open_message_notification_settings,
        );
    }
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        sound.set_mode("toggle".into());
        sound.set_enabled(ui_ctx.notification_sound_enabled());
        let ui_ctx_sound = ui_ctx.clone();
        let weak_sound = window.as_weak();
        sound.on_enabled_changed(move |enabled| {
            if let Err(e) = ui_ctx_sound.set_notification_sound_enabled(enabled) {
                tracing::error!("Failed to save notification sound: {}", e);
            }
            if let Some(w) = weak_sound.upgrade() {
                w.global::<crate::NotificationSound>()
                    .set_enabled(ui_ctx_sound.notification_sound_enabled());
            }
        });
    }
    #[cfg(target_os = "ios")]
    let _ = sound;
}

fn update_notification_delivery(
    weak: &slint::Weak<MainWindow>,
    client: &Arc<ChatterClient>,
    enabled: Option<bool>,
) {
    let Some(window) = weak.upgrade() else { return };
    let delivery = window.global::<crate::NotificationDelivery>();
    if delivery.get_busy() {
        return;
    }
    delivery.set_busy(true);
    delivery.set_error("".into());
    if enabled.is_none() {
        delivery.set_loaded(false);
    }
    let client = client.clone();
    let weak = weak.clone();
    tokio::spawn(async move {
        let result = match enabled {
            Some(value) => client.set_account_push_enabled(value).await,
            None => client.account_push_enabled().await,
        };
        let _ = slint::invoke_from_event_loop(move || {
            let Some(window) = weak.upgrade() else { return };
            let delivery = window.global::<crate::NotificationDelivery>();
            delivery.set_busy(false);
            match result {
                Ok(value) => {
                    delivery.set_push_enabled(value);
                    delivery.set_saved_push_enabled(value);
                    delivery.set_loaded(true);
                }
                Err(error) => {
                    delivery.set_push_enabled(delivery.get_saved_push_enabled());
                    delivery.set_error(
                        format!("Could not update notification preferences: {error}").into(),
                    );
                }
            }
        });
    });
}

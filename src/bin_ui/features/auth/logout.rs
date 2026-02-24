//! Logout callback wiring.

use slint::ComponentHandle;

use chatter_linux::app::AppContext;
use chatter_linux::services::auth_service::AuthService;

use crate::MainWindow;

pub fn wire_logout(window: &MainWindow, ctx: &AppContext) {
    let auth_state = ctx.auth_state.clone();
    let grpc_pool = ctx.grpc_pool.clone();
    let identity_manager = ctx.identity_manager.clone();
    let prekey_manager = ctx.prekey_manager.clone();
    let user_repo = ctx.user_repo.clone();
    let settings_repo = ctx.settings_repo.clone();
    let key_store = ctx.key_store.clone();
    let auth_interceptor = ctx.auth_interceptor.clone();
    let inbox_provider = ctx.get_device_inbox_provider();

    let weak_window = window.as_weak();

    window.on_settings_logout(move || {
        let auth_state = auth_state.clone();
        let grpc_pool = grpc_pool.clone();
        let identity_manager = identity_manager.clone();
        let prekey_manager = prekey_manager.clone();
        let user_repo = user_repo.clone();
        let settings_repo = settings_repo.clone();
        let key_store = key_store.clone();
        let auth_interceptor = auth_interceptor.clone();
        let inbox_provider = inbox_provider.clone();
        let weak = weak_window.clone();

        tokio::spawn(async move {
            // Stop inbox provider if available
            if let Some(provider) = &inbox_provider {
                if let Err(e) = provider.stop().await {
                    tracing::error!("Failed to stop inbox provider: {}", e);
                }
            }

            // Create auth service with grpc_pool for internal client creation
            let auth_service = AuthService::new(
                grpc_pool.clone(),
                identity_manager,
                prekey_manager,
                user_repo,
                settings_repo,
                key_store,
                auth_interceptor,
            );

            // Attempt logout (ignore errors - we logout locally regardless)
            let _ = auth_service.do_logout().await;

            // Update auth state
            auth_state.set_logged_out();

            // Switch to login view
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    w.set_current_view("login".into());
                    w.set_active_modal("none".into());
                    w.set_login_username("".into());
                    w.set_login_password("".into());
                    w.set_login_error("".into());
                }
            });
        });
    });
}

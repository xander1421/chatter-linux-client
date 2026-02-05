//! Chatter Linux Client
//!
//! Native Linux desktop client for Chatter secure messaging.

use slint::SharedString;

use chatter_linux::app::AppContext;
use chatter_linux::services::auth_service::AuthService;

// Include Slint-generated code in the binary
slint::include_modules!();

#[tokio::main]
async fn main() {
    // Initialize tracing for logging
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .init();

    tracing::info!("Chatter Linux Client v0.1.0");

    // Initialize application context (all subsystems)
    let ctx = match AppContext::new() {
        Ok(ctx) => ctx,
        Err(e) => {
            tracing::error!("Failed to initialize application: {}", e);
            eprintln!("Failed to initialize: {}", e);
            std::process::exit(1);
        }
    };

    // Connect to server (continue in offline mode if fails)
    if let Err(e) = ctx.grpc_pool.connect().await {
        tracing::warn!("Could not connect to server: {} (offline mode)", e);
    }

    // Create main window
    let window = MainWindow::new().expect("Failed to create window");

    // Set initial view based on login state
    let auth_service = ctx.auth_service();
    if auth_service.is_logged_in() {
        window.set_current_view("main".into());
        // Populate display name from auth state if available
        if let Some((_, _)) = auth_service.current_user() {
            let display_name = ctx.auth_state.get().display_name.unwrap_or_default();
            window.set_user_display_name(display_name.into());
        }
    } else {
        window.set_current_view("login".into());
    }

    // Wire callbacks
    wire_auth_callbacks(&window, &ctx);
    wire_navigation_callbacks(&window);

    // Run Slint event loop (blocks until window closes)
    window.run().expect("Failed to run application");

    tracing::info!("Application closed");
}

/// Wire authentication callbacks (login, register, logout).
fn wire_auth_callbacks(window: &MainWindow, ctx: &AppContext) {
    // Clone what we need for callbacks
    let auth_state = ctx.auth_state.clone();
    let grpc_pool = ctx.grpc_pool.clone();
    let identity_manager = ctx.identity_manager.clone();
    let prekey_manager = ctx.prekey_manager.clone();
    let user_repo = ctx.user_repo.clone();
    let settings_repo = ctx.settings_repo.clone();
    let key_store = ctx.key_store.clone();
    let auth_interceptor = ctx.auth_interceptor.clone();

    // Login callback
    let weak_window = window.as_weak();
    let auth_state_login = auth_state.clone();
    let grpc_pool_login = grpc_pool.clone();
    let identity_manager_login = identity_manager.clone();
    let prekey_manager_login = prekey_manager.clone();
    let user_repo_login = user_repo.clone();
    let settings_repo_login = settings_repo.clone();
    let key_store_login = key_store.clone();
    let auth_interceptor_login = auth_interceptor.clone();

    window.on_login(move |username: SharedString, password: SharedString| {
        let auth_state = auth_state_login.clone();
        let grpc_pool = grpc_pool_login.clone();
        let identity_manager = identity_manager_login.clone();
        let prekey_manager = prekey_manager_login.clone();
        let user_repo = user_repo_login.clone();
        let settings_repo = settings_repo_login.clone();
        let key_store = key_store_login.clone();
        let auth_interceptor = auth_interceptor_login.clone();
        let weak = weak_window.clone();
        let username = username.to_string();
        let password = password.to_string();

        tokio::spawn(async move {
            // Set loading state
            let weak_for_loading = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_for_loading.upgrade() {
                    w.set_login_loading(true);
                    w.set_login_error("".into());
                }
            });

            // Create auth service and client
            let auth_service = AuthService::new(
                identity_manager,
                prekey_manager,
                user_repo,
                settings_repo,
                key_store,
                auth_interceptor,
            );

            // Need mutable client for login
            let mut auth_client = chatter_linux::network::grpc::auth_client::AuthClient::new(
                grpc_pool.channel(),
                grpc_pool.auth_clone(),
            );

            // Attempt login
            match auth_service
                .login(&mut auth_client, &username, &password, "linux-client")
                .await
            {
                Ok(result) => {
                    // Update auth state
                    auth_state.set_logged_in(
                        result.user_id,
                        result.device_id,
                        result.display_name.clone(),
                    );

                    // Switch to main view
                    let display_name = result.display_name;
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_login_loading(false);
                            w.set_current_view("main".into());
                            w.set_user_display_name(display_name.into());
                        }
                    });
                }
                Err(e) => {
                    // Show error
                    let error_msg = e.to_string();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_login_loading(false);
                            w.set_login_error(error_msg.into());
                        }
                    });
                }
            }
        });
    });

    // Register callback
    let weak_window = window.as_weak();
    let auth_state_register = auth_state.clone();
    let grpc_pool_register = grpc_pool.clone();
    let identity_manager_register = identity_manager.clone();
    let prekey_manager_register = prekey_manager.clone();
    let user_repo_register = user_repo.clone();
    let settings_repo_register = settings_repo.clone();
    let key_store_register = key_store.clone();
    let auth_interceptor_register = auth_interceptor.clone();

    window.on_register(
        move |username: SharedString, password: SharedString, display_name: SharedString| {
            let auth_state = auth_state_register.clone();
            let grpc_pool = grpc_pool_register.clone();
            let identity_manager = identity_manager_register.clone();
            let prekey_manager = prekey_manager_register.clone();
            let user_repo = user_repo_register.clone();
            let settings_repo = settings_repo_register.clone();
            let key_store = key_store_register.clone();
            let auth_interceptor = auth_interceptor_register.clone();
            let weak = weak_window.clone();
            let username = username.to_string();
            let password = password.to_string();
            let display_name = display_name.to_string();

            tokio::spawn(async move {
                // Set loading state
                let weak_for_loading = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak_for_loading.upgrade() {
                        w.set_register_loading(true);
                        w.set_register_error("".into());
                    }
                });

                // Create auth service and client
                let auth_service = AuthService::new(
                    identity_manager,
                    prekey_manager,
                    user_repo,
                    settings_repo,
                    key_store,
                    auth_interceptor,
                );

                let mut auth_client = chatter_linux::network::grpc::auth_client::AuthClient::new(
                    grpc_pool.channel(),
                    grpc_pool.auth_clone(),
                );

                // Attempt registration
                match auth_service
                    .register(
                        &mut auth_client,
                        &username,
                        &password,
                        &display_name,
                        "linux-client",
                    )
                    .await
                {
                    Ok(result) => {
                        // Update auth state
                        auth_state.set_logged_in(
                            result.user_id,
                            result.device_id,
                            result.display_name.clone(),
                        );

                        // Switch to main view
                        let display_name = result.display_name;
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                w.set_register_loading(false);
                                w.set_current_view("main".into());
                                w.set_user_display_name(display_name.into());
                            }
                        });
                    }
                    Err(e) => {
                        // Show error
                        let error_msg = e.to_string();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                w.set_register_loading(false);
                                w.set_register_error(error_msg.into());
                            }
                        });
                    }
                }
            });
        },
    );

    // Logout callback (via settings)
    let weak_window = window.as_weak();
    let auth_state_logout = auth_state;
    let grpc_pool_logout = grpc_pool;
    let identity_manager_logout = identity_manager;
    let prekey_manager_logout = prekey_manager;
    let user_repo_logout = user_repo;
    let settings_repo_logout = settings_repo;
    let key_store_logout = key_store;
    let auth_interceptor_logout = auth_interceptor;

    window.on_settings_logout(move || {
        let auth_state = auth_state_logout.clone();
        let grpc_pool = grpc_pool_logout.clone();
        let identity_manager = identity_manager_logout.clone();
        let prekey_manager = prekey_manager_logout.clone();
        let user_repo = user_repo_logout.clone();
        let settings_repo = settings_repo_logout.clone();
        let key_store = key_store_logout.clone();
        let auth_interceptor = auth_interceptor_logout.clone();
        let weak = weak_window.clone();

        tokio::spawn(async move {
            // Create auth service and client
            let auth_service = AuthService::new(
                identity_manager,
                prekey_manager,
                user_repo,
                settings_repo,
                key_store,
                auth_interceptor,
            );

            let mut auth_client = chatter_linux::network::grpc::auth_client::AuthClient::new(
                grpc_pool.channel(),
                grpc_pool.auth_clone(),
            );

            // Attempt logout (ignore errors - we logout locally regardless)
            let _ = auth_service.logout(&mut auth_client).await;

            // Update auth state
            auth_state.set_logged_out();

            // Switch to login view
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    w.set_current_view("login".into());
                    w.set_active_modal("none".into());
                    // Clear login form
                    w.set_login_username("".into());
                    w.set_login_password("".into());
                    w.set_login_error("".into());
                }
            });
        });
    });
}

/// Wire navigation callbacks (switch between auth screens).
fn wire_navigation_callbacks(window: &MainWindow) {
    // Go to register page
    let weak_window = window.as_weak();
    window.on_go_to_register(move || {
        if let Some(w) = weak_window.upgrade() {
            w.set_current_view("register".into());
            // Clear register form
            w.set_register_display_name("".into());
            w.set_register_username("".into());
            w.set_register_password("".into());
            w.set_register_confirm_password("".into());
            w.set_register_error("".into());
        }
    });

    // Go to login page
    let weak_window = window.as_weak();
    window.on_go_to_login(move || {
        if let Some(w) = weak_window.upgrade() {
            w.set_current_view("login".into());
            // Clear login form
            w.set_login_username("".into());
            w.set_login_password("".into());
            w.set_login_error("".into());
        }
    });
}

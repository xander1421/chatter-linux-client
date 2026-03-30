//! Chatter Client Library
//!
//! Cross-platform client for Chatter secure messaging.
//! Supports Linux, Windows, macOS, and Android.

use chatter_core::system::{init_antidebug, antidebug_check};
use tokio_util::sync::CancellationToken;

// Include Slint-generated code in the library crate
slint::include_modules!();

// UI modules (require Slint types)
pub mod bin_ui;

/// Global tokio runtime handle for use in Slint callbacks.
/// On Android, Slint's event loop doesn't preserve the tokio runtime context,
/// so tokio::spawn silently fails from callbacks. This handle is set once at
/// startup and used via `rt_handle().spawn(...)` instead of `tokio::spawn(...)`.
static RT_HANDLE: std::sync::OnceLock<tokio::runtime::Handle> = std::sync::OnceLock::new();

/// Get the tokio runtime handle. Panics if called before app_main.
pub fn rt_handle() -> &'static tokio::runtime::Handle {
    RT_HANDLE.get().expect("tokio runtime not initialized")
}

/// Android entry point - called by android-activity glue.
#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
fn android_main(app: i_slint_backend_android_activity::AndroidApp) {
    // Set up the Slint Android platform backend
    slint::platform::set_platform(Box::new(
        i_slint_backend_android_activity::AndroidPlatform::new(app),
    ))
    .expect("Failed to set Android platform");

    // Initialize Android notification channel (required API 26+)
    // Must be done before any notifications are shown.
    {
        let notifs = chatter_core::system::android_notifications::AndroidNotifications::new();
        if let Err(e) = notifs.init_channel() {
            // Non-fatal — app works without notifications
            eprintln!("Failed to init Android notification channel: {}", e);
        }
    }

    // Two-phase startup:
    // Phase 1: block_on() for async init (client, gRPC channels, window setup)
    // Phase 2: rt.enter() + window.run() — Slint owns the thread, tokio reactor
    //          stays accessible via the enter guard, worker threads stay alive
    //
    // We MUST NOT call window.run() inside block_on() — that suspends block_on's
    // polling loop, killing the I/O reactor. gRPC channels bound to this reactor
    // would stop working.
    let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");
    let _ = RT_HANDLE.set(rt.handle().clone());

    // Phase 1: async init inside block_on (reactor is polling here)
    let app_state = rt.block_on(app_init());

    // Phase 2: drop out of block_on, enter runtime context, run Slint event loop.
    // The enter guard keeps tokio's I/O reactor registered on this thread.
    // Worker threads remain alive for rt_handle().spawn() from callbacks.
    let _guard = rt.enter();

    setup_keyboard_inset_tracking(&app_state.window);

    app_run(app_state);
}

/// Poll Android system bar insets and update Slint safe-area properties.
///
/// NativeActivity's GL surface extends behind system bars (status bar, nav bar)
/// and doesn't resize with `adjustResize`. We poll insets via JNI every 200ms
/// using WindowInsets API and push changes to the Slint properties that control
/// content positioning (see `app.slint` lines 771-774).
#[cfg(target_os = "android")]
fn setup_keyboard_inset_tracking(window: &MainWindow) {
    use std::cell::Cell;

    let weak = window.as_weak();
    let last_top = std::rc::Rc::new(Cell::new(-1.0f32));
    let last_bottom = std::rc::Rc::new(Cell::new(-1.0f32));

    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_millis(200),
        move || {
            if let Some(insets) = chatter_core::system::android_keyboard::query_display_insets() {
                let top_changed = (insets.top - last_top.get()).abs() > 0.5;
                let bottom_changed = (insets.bottom - last_bottom.get()).abs() > 0.5;

                if top_changed || bottom_changed {
                    if let Some(window) = weak.upgrade() {
                        if top_changed {
                            window.set_safe_area_top(insets.top);
                            last_top.set(insets.top);
                        }
                        if bottom_changed {
                            window.set_safe_area_bottom(insets.bottom);
                            last_bottom.set(insets.bottom);
                        }
                        tracing::debug!(
                            top = insets.top,
                            bottom = insets.bottom,
                            "Safe area insets updated"
                        );
                    }
                }
            }
        },
    );

    // Timer must live for the app's lifetime — it runs inside the Slint event loop
    std::mem::forget(timer);
}

/// State produced by async init, consumed by the sync event loop.
pub struct AppState {
    window: MainWindow,
    ui_ctx: std::sync::Arc<chatter_core::client::ChatterClient>,
    shutdown_token: CancellationToken,
}

/// Phase 1: Async initialization (runs inside block_on — reactor is active).
/// Creates client, gRPC channels, window, and wires callbacks.
/// Returns state for the sync event loop phase.
pub async fn app_init() -> AppState {
    let _ = RT_HANDLE.set(tokio::runtime::Handle::current());

    let shutdown_token = CancellationToken::new();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .init();

    tracing::info!("Chatter Client v0.1.0 ({})", std::env::consts::OS);

    if !init_antidebug() {
        tracing::warn!("Security: Continuing with debugger detected - sensitive data may be exposed");
    }

    if let Err(e) = gstreamer::init() {
        tracing::warn!("GStreamer initialization failed: {} (media features may not work)", e);
    } else {
        tracing::info!("GStreamer initialized: {}", gstreamer::version_string());
    }

    #[cfg(not(target_os = "android"))]
    if let Err(e) = slint::BackendSelector::new()
        .backend_name("winit".into())
        .require_opengl_es()
        .select()
    {
        tracing::warn!("OpenGL ES backend not available: {} (falling back to default)", e);
    } else {
        tracing::info!("Slint OpenGL ES backend selected for hardware-accelerated video");
    }

    #[allow(unused_mut)]
    let mut builder = chatter_core::client::ChatterClient::builder();
    #[cfg(target_os = "android")]
    {
        builder = builder.data_dir("/data/data/com.chatter.client/files");
    }
    let client = match builder.build() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("Failed to initialize application: {}", e);
            eprintln!("Failed to initialize: {}", e);
            std::process::exit(1);
        }
    };
    let ui_ctx = client.clone();

    ui_ctx.auth().restore_session();

    let _typing_cleanup = ui_ctx.typing().start_cleanup_task();
    tracing::info!("Typing indicator cleanup task started");

    // Connect to server (reactor is active here inside block_on)
    let connected = match client.connect().await {
        Ok(()) => {
            client.offline_state().set_connection_state(
                chatter_core::network::connection::ConnectionState::Connected,
            );
            true
        }
        Err(e) => {
            tracing::warn!("Could not connect to server: {} (offline mode)", e);
            client.offline_state().set_connection_state(
                chatter_core::network::connection::ConnectionState::Disconnected,
            );
            false
        }
    };

    let window = MainWindow::new().expect("Failed to create window");

    // Android: flag as mobile platform so the UI uses mobile layout (full-screen
    // sidebar, bottom nav, etc.) when the screen is narrow.
    #[cfg(target_os = "android")]
    window.set_is_mobile_platform(true);

    // Desktop: explicitly zero safe-area insets (only Android sets non-zero values
    // via JNI inset polling). Prevents any stale or default-mismatch values from
    // pushing the content area away from window edges.
    #[cfg(not(target_os = "android"))]
    {
        window.set_safe_area_top(0.0);
        window.set_safe_area_bottom(0.0);
    }

    if bin_ui::video::init_hw_video(&window) {
        tracing::info!("Hardware-accelerated video enabled");
    } else {
        tracing::info!("Using software video rendering");
    }

    if ui_ctx.auth().is_logged_in() {
        if let Some((user_id, device_id)) = ui_ctx.auth().current_user() {
            let display_name = ui_ctx.users().get_display_name(&user_id);
            ui_ctx.auth_state().set_logged_in(user_id, device_id, display_name);
            tracing::info!("Restored auth state for user {}", user_id);
        }

        window.set_current_view("main".into());

        if !connected {
            tracing::info!("Starting in offline mode — gRPC calls will retry when server becomes available");
        }
        if let Err(e) = client.start().await {
            tracing::warn!("Could not start services: {} (some features may not work)", e);
        }

        let weak_window = window.as_weak();
        bin_ui::features::modals::spawn_anon_dm_request_handler(weak_window.clone(), &ui_ctx);
        bin_ui::features::modals::spawn_anon_group_invite_handler(weak_window.clone(), &ui_ctx);

        {
            let mut domain_rx = ui_ctx.event_bus().subscribe_domain();
            let weak_for_domain = weak_window.clone();
            let ui_ctx_for_domain = ui_ctx.clone();
            tokio::spawn(async move {
                loop {
                    match domain_rx.recv().await {
                        Ok(event) => {
                            bin_ui::features::auth::inbox::domain_handler::handle_domain_event(
                                event, &weak_for_domain, &ui_ctx_for_domain,
                            ).await;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!(dropped_count = n, "Domain event receiver lagged");
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            });
        }

        {
            let ui_ctx_for_events = ui_ctx.clone();
            tokio::spawn(async move {
                bin_ui::features::auth::load_initial_data(weak_window, &ui_ctx_for_events).await;
            });
        }
    } else {
        let has_registered_before = ui_ctx.auth().has_registered_before();
        if has_registered_before {
            window.set_current_view("login".into());
        } else {
            window.set_current_view("register".into());
        }
    }

    bin_ui::features::wire_all(&window, &ui_ctx);

    // Dev mode auto-register (desktop: CHATTER_DEV=1, Android: always in dev builds)
    let dev_mode = std::env::var("CHATTER_DEV").is_ok() || cfg!(target_os = "android");
    if dev_mode && !ui_ctx.auth().is_logged_in() && !ui_ctx.auth().has_registered_before() {
        let suffix: u16 = rand::random();
        let username = format!("dev{}", suffix);
        let display_name = format!("Dev User {}", suffix);
        let password = "devpass123";
        let pin = "123456";
        let server_url = if cfg!(target_os = "android") {
            "http://192.168.0.7:50051"
        } else {
            "http://localhost:50051"
        };

        window.set_register_username(username.clone().into());
        window.set_register_display_name(display_name.clone().into());
        window.set_register_password(password.into());
        window.set_register_confirm_password(password.into());
        window.set_register_pin(pin.into());
        window.set_register_server_url(server_url.into());

        tracing::info!("DEV MODE: Auto-registering as {} ({})", username, display_name);
        window.invoke_register(
            username.into(), password.into(), display_name.into(),
            pin.into(), server_url.into(),
        );
    }

    // Anti-debug check (background task on tokio workers)
    let shutdown_for_antidebug = shutdown_token.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(30));
        loop {
            tokio::select! {
                _ = shutdown_for_antidebug.cancelled() => break,
                _ = interval.tick() => { let _ = antidebug_check(); }
            }
        }
    });

    #[cfg(not(target_os = "android"))]
    {
        let shutdown_for_ctrlc = shutdown_token.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                tracing::info!("Ctrl+C received, initiating shutdown...");
                shutdown_for_ctrlc.cancel();
                slint::quit_event_loop().ok();
            }
        });
    }

    AppState { window, ui_ctx: client, shutdown_token }
}

/// Phase 2: Sync event loop (runs OUTSIDE block_on — rt.enter() keeps reactor accessible).
/// Slint owns the thread. Tokio worker threads handle async I/O.
fn app_run(state: AppState) {
    // Run Slint event loop (blocks until window closes)
    state.window.run().expect("Failed to run application");

    // Shutdown
    tracing::info!("Window closed, signaling shutdown...");
    state.shutdown_token.cancel();
    tracing::info!("Shutting down services...");

    // Spawn async shutdown on the runtime and wait via sync channel.
    // We can't use block_on here because desktop's #[tokio::main] already
    // has an active block_on on this thread — nesting would panic.
    let ui_ctx = state.ui_ctx;
    let (tx, rx) = std::sync::mpsc::channel();
    rt_handle().spawn(async move {
        if ui_ctx.auth_state().is_logged_in() {
            tracing::info!("Sending offline presence before shutdown...");
            bin_ui::features::auth::presence_integration::shutdown_presence(&ui_ctx).await;
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        ui_ctx.shutdown().await;
        let _ = tx.send(());
    });
    let _ = rx.recv_timeout(std::time::Duration::from_secs(5));

    tracing::info!("Shutdown complete, exiting...");
    std::process::exit(0);
}

/// Desktop entry point. Runs init then event loop inside tokio::main's block_on.
/// On desktop, tokio worker threads keep the reactor alive while window.run() blocks.
pub async fn app_main() {
    let state = app_init().await;
    app_run(state);
}

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

// Platform integration (JNI on Android, etc.)
pub mod platform;

/// Global tokio runtime handle for use in Slint callbacks.
/// On Android, Slint's event loop doesn't preserve the tokio runtime context,
/// so tokio::spawn silently fails from callbacks. This handle is set once at
/// startup and used via `rt_handle().spawn(...)` instead of `tokio::spawn(...)`.
static RT_HANDLE: std::sync::OnceLock<tokio::runtime::Handle> = std::sync::OnceLock::new();

/// Get the tokio runtime handle. Panics if called before app_main.
pub fn rt_handle() -> &'static tokio::runtime::Handle {
    RT_HANDLE.get().expect("tokio runtime not initialized")
}

/// Set the runtime handle explicitly. First call wins; later calls are no-ops.
/// Used by the ui_wiring test harness, which has no `app_init` phase.
pub fn init_rt_handle(handle: tokio::runtime::Handle) {
    let _ = RT_HANDLE.set(handle);
}

/// Android: the app's internal data directory (`Context.getFilesDir()`),
/// resolved from `AndroidApp::internal_data_path()` at `android_main` entry.
/// Used by `app_init` to tell `ChatterClient` where to open the DB.
#[cfg(target_os = "android")]
static ANDROID_DATA_DIR: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Android entry point — called by android-activity glue.
///
/// Runs on the dedicated android_main pthread that android-activity spawns.
/// That thread owns the Looper; `AndroidApp::poll_events` (and therefore the
/// Slint event loop) must run on this same thread. We cannot rehost the
/// event loop elsewhere, so all initialization and the event loop execute
/// inline here.
///
/// Modals and overlays in `ui/app.slint` are wrapped in `if cond:` blocks so
/// their generated inner components become `sp::Conditional<...>` instead of
/// being embedded inline in `InnerApp_root`. Without that wrapping the
/// stack frame for `MainWindow::new()` exceeds the ~2MB default bionic
/// thread stack and we crash with SIGSEGV inside Slint codegen.
#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
fn android_main(app: i_slint_backend_android_activity::AndroidApp) {
    let data_dir = app
        .internal_data_path()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "/data/data/com.chatter.client/files".to_string());
    let _ = ANDROID_DATA_DIR.set(data_dir);

    slint::platform::set_platform(Box::new(
        i_slint_backend_android_activity::AndroidPlatform::new(app),
    ))
    .expect("Failed to set Android platform");

    // Two-phase startup (see desktop `app_main` for the same split):
    //   Phase 1: block_on(app_init) while the reactor is polling.
    //   Phase 2: leave block_on, enter runtime ctx, run Slint event loop.
    // Running `window.run()` inside `block_on` would stall the I/O reactor
    // and kill any in-flight gRPC streams bound to it.
    let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");
    let app_state = rt.block_on(app_init());
    let _guard = rt.enter();

    // Safe-area insets (status bar, nav bar, IME) are now bound in `main.slint`
    // directly off the Window's built-in `safe-area-insets` and
    // `virtual-keyboard-size` out properties — Slint's Android backend feeds
    // them via `WindowInsetsCompat`, so no manual JNI polling is needed.

    // Request runtime permissions now the activity is resumed. Notifications
    // belong at startup; mic/camera ideally move to on-demand (before a call),
    // but requesting here surfaces the grant dialogs so the features work at
    // all. Fire-and-forget — outcomes are logged via the JNI result callback.
    // Only notifications at startup; mic/camera are requested on-demand when a
    // call starts (see features/calls/outgoing.rs) so we don't prompt for
    // hardware the user may never use.
    use crate::platform::android::permissions;
    permissions::request(&[permissions::POST_NOTIFICATIONS]);

    // Register CameraX as chatter-core's video capture backend so calls can
    // publish camera video (chatter-core drives capture, this feeds it frames).
    crate::platform::android::camera::init();

    app_run(app_state);
}

/// State produced by async init, consumed by the sync event loop.
pub struct AppState {
    window: MainWindow,
    ui_ctx: std::sync::Arc<chatter_core::client::ChatterClient>,
    shutdown_token: CancellationToken,
    #[cfg(target_os = "linux")]
    _tray_handle: Option<chatter_core::system::TrayHandle>,
}

/// Phase 1: Async initialization (runs inside block_on — reactor is active).
/// Creates client, gRPC channels, window, and wires callbacks.
/// Returns state for the sync event loop phase.
pub async fn app_init() -> AppState {
    let _ = RT_HANDLE.set(tokio::runtime::Handle::current());

    // NB: LiveKit/webrtc's android init (initialize_android) runs from
    // MainActivity.onCreate via nativeInitWebrtc — it must execute on the Java
    // main thread so libwebrtc can resolve its bundled Java classes through the
    // app classloader. See platform::android::activity.

    let shutdown_token = CancellationToken::new();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into())
                .add_directive("glycin=warn".parse().unwrap())
                .add_directive("glycin_utils=warn".parse().unwrap())
                .add_directive("zbus=warn".parse().unwrap()),
        )
        .init();

    tracing::info!("Chatter Client v0.1.0 ({})", std::env::consts::OS);

    // SIGINT handler — register BEFORE GStreamer / Slint / LiveKit init.
    // Several of those libs touch signal masks or install their own
    // handlers on native threads, and if `tokio::signal::ctrl_c` is
    // registered after them its installation can lose the race (or its
    // signal handler thread can end up with SIGINT masked). Registering
    // first means tokio's `sigaction` is in place before anyone else
    // can interfere. The handler only posts the quit event; the real
    // cleanup (leave call, stop services) runs in `app_run` after
    // `window.run()` returns, via `ChatterClient::shutdown`.
    #[cfg(not(target_os = "android"))]
    {
        let shutdown_token = shutdown_token.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                tracing::info!("Ctrl+C received, initiating shutdown...");
                shutdown_token.cancel();
                slint::quit_event_loop().ok();
            }
        });
    }

    if !init_antidebug() {
        tracing::warn!("Security: Continuing with debugger detected - sensitive data may be exposed");
    }

    #[cfg(feature = "desktop-media")]
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
        // Prefer the real `Context.getFilesDir()` path set by `android_main`.
        // Fall back to the hardcoded sandboxed path only when the OnceLock
        // is somehow unset (should never happen in a real app lifecycle).
        let data_dir = ANDROID_DATA_DIR
            .get()
            .cloned()
            .unwrap_or_else(|| "/data/data/com.chatter.client/files".to_string());
        builder = builder.data_dir(data_dir);

        let android_sink = std::sync::Arc::new(
            crate::platform::android::AndroidNotifications::new(),
        );
        if let Err(e) = android_sink.init_channel() {
            // Non-fatal — notifications just won't fire.
            eprintln!("Failed to init Android notification channel: {}", e);
        }
        builder = builder.notification_sink(android_sink);
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

    // Let incoming-call notification actions (Accept/Decline) reach the client.
    #[cfg(target_os = "android")]
    crate::platform::android::call_actions::register_call_client(client.clone());

    // Restore a saved device-auth session before the first RPC runs.
    ui_ctx.restore_session();

    // Connect to server (reactor is active here inside block_on).
    //
    // `connect()` records the outcome in `offline_state` itself, so there is
    // nothing to mirror here — doing it again is how the two writes that used to
    // live in this function came to be the *only* reachability writes in the
    // process. The monitor started below is what keeps them current.
    //
    // Bounded, because this is on the critical path to the first frame: nothing
    // is drawn until `app_init` returns. The pool's own connect timeout is 10s
    // direct and 60s over onion, so an unroutable server URL — a LAN address on a
    // network the phone has left, which is exactly the mesh's scenario — meant up
    // to a minute of black screen before the app admitted anything was wrong.
    // Giving up early costs nothing: the reachability monitor re-probes, and RPCs
    // dial on demand.
    const BOOT_CONNECT_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);
    let connected = match tokio::time::timeout(BOOT_CONNECT_BUDGET, client.connect()).await {
        Ok(Ok(())) => true,
        Ok(Err(e)) => {
            tracing::warn!("Could not connect to server: {} (offline mode)", e);
            false
        }
        Err(_) => {
            tracing::warn!(
                budget_s = BOOT_CONNECT_BUDGET.as_secs(),
                "Server did not answer within the startup budget (offline mode)"
            );
            // Nothing wrote the outcome, because nothing finished. Record it, or
            // the UI would render the store's default until the first probe.
            client.offline_state().set_connection_state(
                chatter_core::network::connection::ConnectionState::Disconnected,
            );
            false
        }
    };

    // Keep "is the server reachable" true for the rest of the process, and do it
    // regardless of login: the user who most needs to be told the server is
    // unreachable is the one who could not sign in to it. Without this the state
    // recorded above is a reading of one instant that is never revised — an
    // offline boot claimed "offline" forever, and an online boot claimed
    // "connected" long after the server died.
    client.start_server_monitor();

    let window = MainWindow::new().expect("Failed to create window");

    // Android: flag as mobile platform so the UI uses mobile layout (full-screen
    // sidebar, bottom nav, etc.) when the screen is narrow.
    #[cfg(target_os = "android")]
    window.set_is_mobile_platform(true);

    // Let the notification "Accept" activity-intent drive the in-app accept flow.
    #[cfg(target_os = "android")]
    crate::platform::android::call_actions::register_main_window(window.as_weak());

    // Jarvis feature gate: flip the Slint global so jarvis surfaces render.
    // Slint can't `cfg`-gate, so we use a runtime bool driven by the Cargo feature.
    #[cfg(feature = "jarvis")]
    {
        use slint::ComponentHandle;
        window.global::<JarvisGlobal>().set_enabled(true);
    }

    // Anon Mode: enforce default-off on every fresh window. The core flag
    // is already false (in-memory AtomicBool), this keeps the Slint global
    // in sync defensively. Auto-revert on restart is the privacy property
    // of this feature; do NOT persist this flag.
    {
        use slint::ComponentHandle;
        window.global::<AnonMode>().set_enabled(false);
    }

    if bin_ui::video::init_hw_video(&window) {
        tracing::info!("Hardware-accelerated video enabled");
    } else {
        tracing::info!("Using software video rendering");
    }

    // The mesh comes up regardless of login. It is serverless by design — no
    // RPC, no account, no internet required — and the situations it exists for
    // are precisely the ones where nobody is signing in to anything. Starting it
    // inside the authenticated branch below would have made it useless exactly
    // when it matters. A failure is logged and the app continues.
    #[cfg(feature = "mesh")]
    {
        // The user's own switch, checked here and nowhere else. Off means the
        // radio never comes up at all this session — not that it starts and is
        // then stopped, which would advertise this device to everyone in range
        // for as long as startup takes.
        if client.mesh_enabled() {
            match client.start_mesh().await {
                Ok(()) => tracing::info!("mesh: started"),
                Err(error) => {
                    tracing::warn!("mesh: failed to start, continuing without it: {error}")
                }
            }
        } else {
            tracing::info!("mesh: off by preference; the radio was not started");
        }
        window.global::<MeshGlobal>().set_mesh_on(client.mesh_enabled());
    }

    window.set_is_signed_in(ui_ctx.is_logged_in());

    if ui_ctx.is_logged_in() {
        if let Some((user_id, device_id)) = ui_ctx.current_user() {
            let display_name = ui_ctx.display_name(&user_id);
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
        bin_ui::features::modals::load_channel_invites(weak_window.clone(), &ui_ctx);

        // Tor status indicator: mirror the process-global Tor status into the
        // Slint `TorStatus` global once a second, so the sidebar shield dot
        // reflects live onion connectivity (Connecting/Connected/Error). Cheap —
        // reads an atomic, only touches the UI thread when the value changes.
        {
            let weak_for_tor = weak_window.clone();
            tokio::spawn(async move {
                let mut last = -1i32;
                loop {
                    let s = chatter_core::network::connection::tor_status().as_i32();
                    if s != last {
                        last = s;
                        let weak = weak_for_tor.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            use slint::ComponentHandle;
                            if let Some(w) = weak.upgrade() {
                                w.global::<TorStatus>().set_state(s);
                            }
                        });
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
            });
        }

        {
            let mut domain_rx = ui_ctx.subscribe_events();
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
        let has_registered_before = ui_ctx.has_registered_before();
        if has_registered_before {
            window.set_current_view("login".into());
        } else {
            window.set_current_view("register".into());
        }
    }

    bin_ui::features::wire_all(&window, &ui_ctx);

    // System tray (Linux only — StatusNotifierItem via ksni)
    #[cfg(target_os = "linux")]
    let tray_handle = {
        match chatter_core::system::SystemTray::spawn() {
            Ok((tray_controller, handle, mut event_rx)) => {
                tracing::info!("System tray initialized");

                // Forward tray menu events → Slint window
                let weak_for_tray = window.as_weak();
                let shutdown_for_tray = shutdown_token.clone();
                tokio::spawn(async move {
                    loop {
                        tokio::select! {
                            _ = shutdown_for_tray.cancelled() => break,
                            event = event_rx.recv() => {
                                match event {
                                    Some(chatter_core::system::TrayMessage::OpenWindow) => {
                                        let weak = weak_for_tray.clone();
                                        let _ = slint::invoke_from_event_loop(move || {
                                            if let Some(w) = weak.upgrade() {
                                                w.window().show().ok();
                                            }
                                        });
                                    }
                                    Some(chatter_core::system::TrayMessage::Quit) | None => {
                                        slint::quit_event_loop().ok();
                                        break;
                                    }
                                }
                            }
                        }
                    }
                });

                // Forward unread count changes to tray badge
                let tray_for_unread = tray_controller.clone();
                let weak_for_unread = window.as_weak();
                // Poll total-unread-count periodically (Slint properties can't be
                // observed directly from Rust; a 1-second poll is cheap and sufficient).
                let last_count = std::sync::Arc::new(std::sync::atomic::AtomicI32::new(0));
                tokio::spawn(async move {
                    let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
                    loop {
                        interval.tick().await;
                        let weak = weak_for_unread.clone();
                        let tray = tray_for_unread.clone();
                        let last = last_count.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                let count = w.get_total_unread_count();
                                let prev = last.load(std::sync::atomic::Ordering::Relaxed);
                                if count != prev {
                                    last.store(count, std::sync::atomic::Ordering::Relaxed);
                                    tray.set_unread_count(count.max(0) as u32);
                                }
                            }
                        });
                    }
                });

                Some(handle)
            }
            Err(e) => {
                tracing::warn!("Failed to initialize system tray: {}", e);
                None
            }
        }
    };

    // Dev mode auto-register (desktop: CHATTER_DEV=1, Android: always in dev builds)
    let dev_mode = std::env::var("CHATTER_DEV").is_ok() || cfg!(target_os = "android");
    if dev_mode && !ui_ctx.is_logged_in() && !ui_ctx.has_registered_before() {
        let suffix: u16 = rand::random();
        let username = format!("dev{}", suffix);
        let display_name = format!("Dev User {}", suffix);
        let password = "devpass123";
        // PIN must satisfy `crypto::svr::validate_pin`: 8-64 ASCII printable AND
        // clear the entropy floor (audit M1). Mixed alnum → ~46 bits, passes.
        // (was "123456", then "12345678" — both rejected now.)
        let pin = "devpin123";
        let server_url: String = if cfg!(target_os = "android") {
            // Android dev defaults to localhost via `adb reverse` tunnels
            // (50051 gRPC, 9000 RustFS, 7880/7881 LiveKit). For tunnel-free LAN
            // testing (needed for WebRTC UDP media), build with
            // CHATTER_ANDROID_SERVER=http://<host-lan-ip>:50051 and point the
            // server's LIVEKIT_URL + S3 endpoint at the same LAN IP.
            option_env!("CHATTER_ANDROID_SERVER")
                .unwrap_or("http://localhost:50051")
                .to_string()
        } else {
            // Desktop dev: override the gRPC server at runtime with CHATTER_SERVER
            // (e.g. http://192.168.122.1:50051 to reach the host from a VM guest),
            // otherwise default to localhost.
            std::env::var("CHATTER_SERVER")
                .unwrap_or_else(|_| "http://localhost:50051".to_string())
        };

        window.set_register_username(username.clone().into());
        window.set_register_display_name(display_name.clone().into());
        window.set_register_password(password.into());
        window.set_register_confirm_password(password.into());
        window.set_register_pin(pin.into());
        window.set_register_server_url(server_url.clone().into());

        tracing::info!("DEV MODE: Auto-registering as {} ({}) against {}", username, display_name, server_url);
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

    AppState {
        window,
        ui_ctx: client,
        shutdown_token,
        #[cfg(target_os = "linux")]
        _tray_handle: tray_handle,
    }
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

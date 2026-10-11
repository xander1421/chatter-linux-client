//! Chatter Client Library
//!
//! Cross-platform client for Chatter secure messaging.
//! Supports Linux, Windows, macOS, and Android.

use chatter_core::system::{antidebug_check, init_antidebug};
use tokio_util::sync::CancellationToken;

// Keep the public UI types at the crate root for app wiring and consumers.
// The generated component tree compiles once in its own dependency.
pub use chatter_ui_slint::*;

// UI modules (require Slint types)
pub mod bin_ui;

// Platform integration (JNI on Android, etc.)
pub mod platform;

/// Global tokio runtime handle for use in Slint callbacks.
/// On Android, Slint's event loop doesn't preserve the tokio runtime context,
/// so tokio::spawn silently fails from callbacks. This handle is set once at
/// startup and used via `rt_handle().spawn(...)` instead of `tokio::spawn(...)`.
static RT_HANDLE: std::sync::OnceLock<tokio::runtime::Handle> = std::sync::OnceLock::new();

static STARTUP_STARTED: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
static FIRST_FRAME_RECORDED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Called by existing rendering notifiers so timing never replaces video/JNI hooks.
pub(crate) fn record_startup_frame(state: &slint::RenderingState) {
    if matches!(state, slint::RenderingState::AfterRendering)
        && !FIRST_FRAME_RECORDED.swap(true, std::sync::atomic::Ordering::Relaxed)
    {
        if let Some(started) = STARTUP_STARTED.get() {
            tracing::debug!(
                elapsed_ms = started.elapsed().as_millis(),
                "startup: first frame rendered"
            );
        }
    }
}

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

    // Activity Resume/Pause is the app's visibility: core marks nothing read
    // and sends no receipt while it is paused (backgrounded, screen locked).
    // The client does not exist yet when the platform is installed, so the
    // listener reads it from a cell filled once `app_init` returns.
    let visibility_client: std::rc::Rc<
        std::cell::OnceCell<std::sync::Arc<chatter_core::client::ChatterClient>>,
    > = Default::default();
    let listener_client = visibility_client.clone();
    let visibility_window: std::rc::Rc<std::cell::OnceCell<slint::Weak<MainWindow>>> =
        Default::default();
    let listener_window = visibility_window.clone();
    slint::platform::set_platform(Box::new(
        i_slint_backend_android_activity::AndroidPlatform::new_with_event_listener(
            app,
            move |event| {
                use i_slint_backend_android_activity::android_activity::{MainEvent, PollEvent};
                let visible = match event {
                    PollEvent::Main(MainEvent::Resume { .. }) => true,
                    PollEvent::Main(MainEvent::Pause) => false,
                    _ => return,
                };
                if !visible {
                    // Screen locked or app backgrounded: drop every decoded image.
                    crate::bin_ui::decode_cache::on_lock();
                    if let Some(window) = listener_window.get().and_then(slint::Weak::upgrade) {
                        window.invoke_cancel_composer_keyboard_handover();
                        // The process may be killed while paused: store the
                        // draft now rather than after the autosave delay.
                        if let Some(client) = listener_client.get() {
                            crate::bin_ui::features::composer::flush_drafts(&window, client);
                        }
                    } else if let Some(client) = listener_client.get() {
                        // Recovery still needs a lifecycle checkpoint without a UI handle.
                        client.flush_composer_persistence();
                    }
                }
                if let Some(client) = listener_client.get() {
                    let _rt = rt_handle().enter();
                    client.set_app_visible(visible);
                }
            },
        )
        .with_refresh_rate_vote(crate::platform::android::activity::set_ui_moving)
        .with_text_input_bridge(crate::platform::android::text_input::Bridge),
    ))
    .expect("Failed to set Android platform");

    // The SQLCipher passphrase and mesh seal key are Keystore-sealed; the
    // wrapper must be in place before `app_init` builds the client.
    crate::platform::android::keystore::init();
    crate::platform::android::video_thumbnail::init();

    // Two-phase startup (see desktop `app_main` for the same split):
    //   Phase 1: block_on(app_init) while the reactor is polling.
    //   Phase 2: leave block_on, enter runtime ctx, run Slint event loop.
    // Running `window.run()` inside `block_on` would stall the I/O reactor
    // and kill any in-flight gRPC streams bound to it.
    let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");
    let app_state = rt.block_on(app_init());
    let _guard = rt.enter();
    let _ = visibility_client.set(app_state.ui_ctx.clone());
    let _ = visibility_window.set(slint::ComponentHandle::as_weak(&app_state.window));

    // Safe-area insets (status bar, nav bar, IME) are now bound in `app.slint`
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

    crate::platform::android::activity::clear_launch_cover_after_first_frame(
        slint::ComponentHandle::window(&app_state.window),
    );

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

/// Anon Mode: enforce default-off on every fresh window and mirror the `onion`
/// Cargo feature into `AnonMode.onion-built`.
///
/// The core flag is already false (in-memory AtomicBool); this keeps the Slint
/// global in sync defensively. Auto-revert on restart is the privacy property
/// of this feature; do NOT persist this flag.
///
/// STUB-1: mirror `onion` exactly as the `jarvis` block in `app_init` mirrors
/// its own. Shield itself is offered in every build (it is the anonymous-mailbox
/// mode); only the sidebar's Tor status dot is gated on `onion-built`, because
/// without Tor it would be a permanent red.
///
/// Public so the headless UI suite drives the real mirror rather than reading
/// the Slint default back (which equals `cfg!(feature = "onion")` only in a
/// build without `onion`).
pub fn mirror_anon_mode_build(window: &MainWindow) {
    use slint::ComponentHandle;
    window.global::<AnonMode>().set_enabled(false);
    window
        .global::<AnonMode>()
        .set_onion_built(cfg!(feature = "onion"));
}

/// Phase 1: Async initialization (runs inside block_on — reactor is active).
/// Creates client, gRPC channels, window, and wires callbacks.
/// Returns state for the sync event loop phase.
pub async fn app_init() -> AppState {
    let startup_started = std::time::Instant::now();
    let _ = STARTUP_STARTED.set(startup_started);
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
        tracing::warn!(
            "Security: Continuing with debugger detected - sensitive data may be exposed"
        );
    }

    #[cfg(feature = "desktop-gstreamer")]
    {
        #[cfg(target_os = "macos")]
        crate::platform::macos::prepare_bundled_gstreamer();
        if let Err(e) = gstreamer::init() {
            tracing::warn!(
                "GStreamer initialization failed: {} (media features may not work)",
                e
            );
        } else {
            tracing::info!("GStreamer initialized: {}", gstreamer::version_string());
        }
    }

    // Only Linux's hardware video path (gl_linux.rs) needs a GLES/EGL context.
    // Windows uses D3D11 and macOS has no hw video path at all (see
    // video/mod.rs), so forcing GLES there just risks the platform's default,
    // better-tested Slint renderer (e.g. ANGLE's GLES-on-macOS emulation is
    // known to mishandle window resize/backing-layer sizing).
    #[cfg(target_os = "linux")]
    if let Err(e) = slint::BackendSelector::new()
        .backend_name("winit".into())
        .require_opengl_es()
        .select()
    {
        tracing::warn!(
            "OpenGL ES backend not available: {} (falling back to default)",
            e
        );
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

        let android_sink =
            std::sync::Arc::new(crate::platform::android::AndroidNotifications::new());
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
    // Overlap Tor directory/guard work with UI/login. This never contacts the
    // Chatter service, including when signed out on the Shield sign-in screen.
    client.prewarm_tor();
    let ui_ctx = client.clone();

    // Let incoming-call notification actions (Accept/Decline) reach the client.
    #[cfg(target_os = "android")]
    crate::platform::android::call_actions::register_call_client(client.clone());

    // Restore a saved device-auth session before the first RPC runs.
    ui_ctx.restore_session();

    // Server contact at startup — chatter-core decides: signed in, it connects
    // (bounded, because nothing is drawn until `app_init` returns) and starts
    // the reachability monitor; signed out, it contacts nothing until a
    // sign-in action, so choosing Shield on the sign-in screen leaks nothing.
    const BOOT_CONNECT_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);
    let connected = client.startup_connect(BOOT_CONNECT_BUDGET).await;

    let window = MainWindow::new().expect("Failed to create window");
    // Native winit window is created later, on show. The id has to be set
    // before that, and the Slint platform exists only after `MainWindow::new`.
    #[cfg(target_os = "linux")]
    crate::platform::linux::apply_desktop_app_id();

    // The register form's Server URL field is required to submit, and until now
    // it was only ever filled in dev mode. On a phone there is no shell to set
    // CHATTER_SERVER_URL in, so an empty field meant a dead Create Account
    // button. Seed it with the endpoint the client is already connected to.
    window.set_register_server_url(ui_ctx.endpoint().into());
    // Same for Sign In, which had no binding at all: the field started empty and
    // the only way to fill it was the LAN preset, which was a hardcoded
    // 192.168.0.7 that resolved to nothing on any other network.
    window.set_login_server_url(ui_ctx.endpoint().into());
    window.set_configured_server_url(ui_ctx.endpoint().into());

    // Touch platform: touch-sized targets and gestures. The phone layout
    // (single pane, bottom nav) is NOT implied — app.slint's `compact-layout`
    // picks it from the window's logical width, so a tablet gets two panes.
    #[cfg(any(target_os = "android", target_os = "ios"))]
    window.set_is_mobile_platform(true);

    // Let the notification "Accept" activity-intent drive the in-app accept flow.
    #[cfg(target_os = "android")]
    crate::platform::android::call_actions::register_main_window(window.as_weak());

    // Screenshot/recording protection while the recovery codes are shown.
    #[cfg(target_os = "android")]
    window.on_secure_screen(crate::platform::android::activity::set_secure);

    // Earpiece / speaker / headset routing for calls; the speaker button
    // becomes a toggle bound to the real route.
    #[cfg(all(target_os = "android", feature = "call-ui"))]
    crate::platform::android::audio_route::install(&window);

    // Jarvis feature gate: flip the Slint global so jarvis surfaces render.
    // Slint can't `cfg`-gate, so we use a runtime bool driven by the Cargo feature.
    #[cfg(feature = "jarvis")]
    {
        use slint::ComponentHandle;
        window.global::<JarvisGlobal>().set_enabled(true);
    }

    // FEAT-5: the create-channel "AI assistant" template. Same mirror pattern,
    // but off by default — the path cannot supply the provider API key the
    // server requires, so the template always produced an empty channel. The
    // Slint block is compiled and renders as nothing until the
    // `ai-channel-template` Cargo feature is built. See ui/ai/ai_global.slint.
    {
        use slint::ComponentHandle;
        window
            .global::<AiIntegrationGlobal>()
            .set_channel_template_enabled(cfg!(feature = "ai-channel-template"));
    }

    {
        use slint::ComponentHandle;
        window
            .global::<AvatarText>()
            .on_initials(|name| chatter_core::ui::initials_from_name(&name).into());
    }

    mirror_anon_mode_build(&window);

    // Touch feedback. One binding site for the whole app — see the module docs.
    bin_ui::haptics::install(&window);

    if bin_ui::video::init_hw_video(&window) {
        tracing::info!("Hardware-accelerated video enabled");
    } else {
        tracing::info!("Using software video rendering");
    }

    // On desktop without a video notifier, install the timing-only notifier.
    // Android's launch-cover notifier owns the fallback and records the same event.
    #[cfg(not(target_os = "android"))]
    let _ = window
        .window()
        .set_rendering_notifier(|state, _| record_startup_frame(&state));

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
        window
            .global::<MeshGlobal>()
            .set_mesh_on(client.mesh_enabled());
    }

    // Tor status indicator: mirror the process-global Tor status into the
    // Slint `TorStatus` global once a second, so every Shield control's dot
    // reflects live onion connectivity (Connecting/Connected/Error). Cheap —
    // reads an atomic, only touches the UI thread when the value changes.
    // Runs for every session, signed in or not: the sign-in / create-account
    // Shield toggle needs it, and a fresh sign-in never reached the
    // restored-session branch that used to start it.
    {
        let weak_for_tor = window.as_weak();
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

    window.set_is_signed_in(ui_ctx.is_logged_in());

    if ui_ctx.is_logged_in() {
        if let Some((user_id, device_id)) = ui_ctx.current_user() {
            let display_name = ui_ctx.display_name(&user_id);
            ui_ctx
                .auth_state()
                .set_logged_in(user_id, device_id, display_name);
            tracing::info!("Restored auth state for user {}", user_id);
        }

        window.set_current_view("main".into());

        if !connected {
            tracing::info!(
                "Starting in offline mode — gRPC calls will retry when server becomes available"
            );
        }
        if let Err(e) = client.start().await {
            tracing::warn!(
                "Could not start services: {} (some features may not work)",
                e
            );
        }

        let weak_window = window.as_weak();
        bin_ui::features::modals::spawn_anon_dm_request_handler(weak_window.clone(), &ui_ctx);
        bin_ui::features::modals::spawn_anon_group_invite_handler(weak_window.clone(), &ui_ctx);
        bin_ui::features::modals::load_channel_invites(weak_window.clone(), &ui_ctx);

        {
            let mut domain_rx = ui_ctx.subscribe_events();
            let weak_for_domain = weak_window.clone();
            let ui_ctx_for_domain = ui_ctx.clone();
            tokio::spawn(async move {
                loop {
                    match domain_rx.recv().await {
                        Ok(event) => {
                            bin_ui::features::auth::inbox::domain_handler::handle_domain_event(
                                event,
                                &weak_for_domain,
                                &ui_ctx_for_domain,
                            )
                            .await;
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

    // Dev-mode auto-register. The whole path — including its fixed, publicly
    // known password and PIN — is COMPILED OUT unless the `dev-auto-register`
    // cargo feature is on, so the credentials cannot exist in a shipped binary.
    //
    // It used to be gated on a RUNTIME `target_os` test, which is true for every
    // Android build including release ones, so a shipped APK auto-registered an
    // account with credentials anyone can read here. Same mistake the
    // dev sealed-sender trust root made with `cfg(debug_assertions)`, and the
    // same fix — an explicit cargo feature (Cargo.toml:47-57 of the workspace,
    // chatter-core/src/config.rs:326).
    #[cfg(feature = "dev-auto-register")]
    dev_auto_register(&window, &ui_ctx);

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

    tracing::debug!(
        elapsed_ms = startup_started.elapsed().as_millis(),
        "startup: UI initialized"
    );
    AppState {
        window,
        ui_ctx: client,
        shutdown_token,
        #[cfg(target_os = "linux")]
        _tray_handle: tray_handle,
    }
}

/// Register a throwaway development account on first launch so a dev build lands
/// in the app instead of on the register screen.
///
/// Compiled out entirely unless the `dev-auto-register` cargo feature is enabled
/// — this function and every literal in it. That feature is enabled by
/// `dev-run.sh` (desktop) and by `android/build-android.sh` (the debug APK that
/// targets the local docker-compose stack); nothing else turns it on, and it is
/// deliberately not in `default`.
///
/// Desktop additionally requires `CHATTER_DEV=1` at runtime, so a dev-featured
/// desktop build does not auto-register on every launch. Android has no way to
/// set an env var before `android_main`, so there the feature flag alone is the
/// switch.
#[cfg(feature = "dev-auto-register")]
fn dev_auto_register(
    window: &MainWindow,
    ui_ctx: &std::sync::Arc<chatter_core::client::ChatterClient>,
) {
    // Desktop opt-in, per run.
    #[cfg(not(target_os = "android"))]
    if std::env::var("CHATTER_DEV").is_err() {
        return;
    }

    if ui_ctx.is_logged_in() || ui_ctx.has_registered_before() {
        return;
    }

    let suffix: u16 = rand::random();
    let username = format!("dev{}", suffix);
    let display_name = format!("Dev User {}", suffix);
    let password = "devpass123";
    // PIN must satisfy `crypto::svr::validate_pin`: 8-64 ASCII printable AND
    // clear the entropy floor (audit M1). Mixed alnum -> ~46 bits, passes.
    // (was "123456", then "12345678" — both rejected now.)
    let pin = "devpin123";

    // Use the same endpoint as startup and the normal authentication screens.
    // Build-time Android overrides are resolved by core, including production.
    let server_url = ui_ctx.endpoint();

    window.set_register_username(username.clone().into());
    window.set_register_display_name(display_name.clone().into());
    window.set_register_password(password.into());
    window.set_register_confirm_password(password.into());
    window.set_register_pin(pin.into());
    window.set_register_server_url(server_url.clone().into());

    tracing::info!(
        "DEV MODE: Auto-registering as {} ({}) against {}",
        username,
        display_name,
        server_url
    );
    window.invoke_register(
        username.into(),
        password.into(),
        display_name.into(),
        pin.into(),
        server_url.into(),
    );
}

/// Phase 2: Sync event loop (runs OUTSIDE block_on — rt.enter() keeps reactor accessible).
/// Slint owns the thread. Tokio worker threads handle async I/O.
fn app_run(state: AppState) {
    // macOS: the native NSWindow only exists once the platform window has
    // actually been created, which needs at least one event-loop iteration
    // after the window is shown (see platform::macos's docs) — in practice a
    // single Duration::ZERO tick isn't always enough (observed: still no
    // NSWindow on the very first tick), so retry a bounded number of times
    // at a short interval instead of gambling on one fixed delay.
    #[cfg(target_os = "macos")]
    let _unified_titlebar_timer = {
        let weak = state.window.as_weak();
        let attempts_left = std::cell::Cell::new(20u32); // ~1s at 50ms, generous margin
        let timer = std::rc::Rc::new(slint::Timer::default());
        let timer_handle = timer.clone();
        timer.start(slint::TimerMode::Repeated, std::time::Duration::from_millis(50), move || {
            let Some(w) = weak.upgrade() else { return };
            if crate::platform::macos::apply_unified_titlebar(w.window()) {
                w.set_macos_titlebar_height(crate::platform::macos::TITLEBAR_HEIGHT);
                w.set_macos_traffic_lights_width(crate::platform::macos::TRAFFIC_LIGHTS_WIDTH);
                timer_handle.stop();
            } else {
                let left = attempts_left.get().saturating_sub(1);
                attempts_left.set(left);
                if left == 0 {
                    tracing::warn!("macOS titlebar: gave up after repeated attempts, window handle never became available");
                    timer_handle.stop();
                }
            }
        });
        timer
    };

    // Run Slint event loop (blocks until window closes)
    state.window.run().expect("Failed to run application");

    // Shutdown
    tracing::info!("Window closed, signaling shutdown...");
    // Whatever the composer still holds outlives this exit.
    bin_ui::features::composer::flush_drafts(&state.window, &state.ui_ctx);
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

    // Android: `_exit`, never `exit`. `exit` runs atexit handlers and C++
    // static destructors while the framework's own threads are still live —
    // HWUI's hwuiTask workers then lock a destroyed mutex and abort (SIGABRT,
    // SI_QUEUE, thread hwuiTask1). Nothing of ours needs a destructor here:
    // shutdown above already flushed what matters.
    #[cfg(target_os = "android")]
    {
        unsafe extern "C" {
            fn _exit(status: std::ffi::c_int) -> !;
        }
        unsafe { _exit(0) }
    }
    #[cfg(not(target_os = "android"))]
    std::process::exit(0);
}

/// Desktop entry point. Runs init then event loop inside tokio::main's block_on.
/// On desktop, tokio worker threads keep the reactor alive while window.run() blocks.
pub async fn app_main() {
    let state = app_init().await;
    app_run(state);
}

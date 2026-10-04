//! VALIDATED-UI headless harness — smoke test.
//!
//! Boots the real `MainWindow` (the generated Slint root from `ui/main.slint`)
//! under the headless testing backend and proves basic round-trips. This is the
//! LAYER-1 (headless-dynamic) foundation for VALIDATED-UI.md.
//!
//! Each `#[test]` runs on its own cargo test thread; `init_no_event_loop()`
//! installs a per-thread testing platform, so tests do not collide.

use i_slint_backend_testing as testing;
use chatter_ui::MainWindow;
// DROPPED-P1s: feature-mirror globals asserted by the anon-mode / AI-template tests.
use chatter_ui::{AiIntegrationGlobal, AnonMode};

/// Route the SQLCipher passphrase through the documented env override so offline
/// `ChatterClient::build()` never touches the OS keyring / Secret Service (absent
/// on a headless CI box). Mirrors `validated.rs::no_keyring_prompts`.
/// Never open a native dialog from a test.
///
/// The picker handlers spawn a real `rfd` dialog. On a developer machine with
/// a display that pops a window mid-run and waits for a human; in CI it just
/// blocks. `CHATTER_HEADLESS` makes every FilePicker entry point resolve to
/// "cancelled" instead.
fn headless_dialogs() {
    unsafe { std::env::set_var("CHATTER_HEADLESS", "1") };
}

fn no_keyring() {
    headless_dialogs();
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe {
        std::env::set_var("CHATTER_DB_PASSPHRASE", "ui-wiring-harness-passphrase");
    });
}

/// Install the headless backend for the current test thread. Safe to call once
/// per thread; a second call on the same thread would panic (platform already
/// initialized), which cargo's thread-per-test model avoids.
fn boot() -> MainWindow {
    testing::init_no_event_loop();
    MainWindow::new().expect("MainWindow::new under testing backend")
}

/// Fail with the cause, not the symptom, when the element-query API is inert.
///
/// `ElementHandle` needs debug info that the Slint compiler only emits when
/// the `ui-test-debug` feature is enabled at build time. Without it every query
/// returns nothing, and the assertions here then report a UI that looks
/// structurally broken -- "ThreadPanel was never instantiated", "there are no
/// Rectangles" -- when the only thing wrong is the command line. Check the real
/// precondition first.
fn require_element_queries() {
    assert!(
        chatter_ui::SLINT_DEBUG_INFO_ENABLED,
        "the ElementHandle query API is inert: chatter-ui was built without \
         the ui-test-debug feature, so every element query returns nothing and \
         nothing asserted below can mean anything. Run the UI suites with:\n\n    \
         ./scripts/test-ui.sh\n\n  or directly:\n\n    \
         cargo test -p chatter-ui --features ui-test-debug --test ui_wiring --test ui_layout"
    );
}

#[test]
fn smoke_boots_and_view_roundtrips() {
    let w = boot();
    // current-view is an in-out property bridged App<=>MainWindow. Setting it and
    // reading it back proves the two-way binding across the component boundary is
    // live (not a dead alias).
    w.set_current_view("register".into());
    assert_eq!(w.get_current_view().as_str(), "register");
    w.set_current_view("login".into());
    assert_eq!(w.get_current_view().as_str(), "login");
    w.set_current_view("main".into());
    assert_eq!(w.get_current_view().as_str(), "main");
}

#[test]
fn smoke_can_query_elements() {
    require_element_queries();
    let w = boot();
    // Prove the element tree is walkable at all: there must be at least one
    // element with a type name. If this returns empty, the whole query layer is
    // dead and every other dynamic assertion is vacuous.
    let any = testing::ElementHandle::find_by_element_type_name(&w, "Rectangle").count();
    assert!(any > 0, "expected at least one Rectangle in the element tree");
}

// ============================================================================
// LAYER 1b — HANDLER→CORE (offline, real ChatterClient, no server)
//
// The strongest proof we can run without the shared backend: build a REAL
// `ChatterClient` in a throwaway temp dir (build() never connects — connect()
// is a separate call we never make), wire the ACTUAL production callbacks via
// `wire_all`, invoke the callback the UI element fires, and assert the value
// landed in CORE state — not just a local Slint bool. This is exactly the bug
// class VALIDATED-UI hunts: "a settings toggle wired to a LOCAL STUB that never
// hit core". Read-receipts + typing-indicators write a local settings row
// (`sessions().set_bool_setting`), so they need no server and no login.
// ============================================================================

/// Build an offline client + wired window sharing one temp data dir.
/// A current-thread tokio runtime is entered so any `sessions()` DB access that
/// expects a reactor context has one (we never spin the loop).
/// One PROCESS-WIDE test runtime. `chatter_ui::rt_handle` is a global
/// OnceLock — the first test to set it wins, and a per-test Runtime would be
/// DROPPED when that test returns, leaving every later test spawning onto a
/// dead runtime (observed as order-dependent "never persisted" failures under
/// the full suite while passing standalone). A static runtime lives for the
/// whole test process, so the handle is always valid regardless of order.
fn test_rt() -> &'static tokio::runtime::Runtime {
    static RT: std::sync::LazyLock<tokio::runtime::Runtime> =
        std::sync::LazyLock::new(|| tokio::runtime::Runtime::new().unwrap());
    &RT
}

fn wired_offline() -> (
    &'static tokio::runtime::Runtime,
    tempfile::TempDir,
    std::sync::Arc<chatter_core::client::ChatterClient>,
    MainWindow,
) {
    no_keyring();
    let rt = test_rt();
    let _guard = rt.enter();
    let tmp = tempfile::tempdir().unwrap();
    testing::init_no_event_loop();
    let client = chatter_core::client::ChatterClient::builder()
        .data_dir(tmp.path().to_string_lossy().to_string())
        .build()
        .expect("offline ChatterClient::build");
    let w = MainWindow::new().expect("MainWindow::new");
    // Wire ONLY the settings callbacks (not full `wire_all`, which spawns a
    // call-orchestrator subscriber requiring a live event loop). These are the
    // real production handlers from features/modals/settings.rs.
    chatter_ui::bin_ui::features::modals::settings::wire_settings_callbacks(&w, &client);
    (rt, tmp, client, w)
}

/// Settings > Privacy > Notification content: the selector's callback reaches
/// core, the level is applied to the running NotificationService, and it
/// survives a restart (core_services.rs loads NOTIFICATION_DETAIL_LEVEL at
/// construction). Default is the private "none".
#[test]
fn settings_notification_detail_level_persists_across_restart() {
    use chatter_core::services::notification_service::NotificationDetailLevel;
    let (rt, tmp, client, w) = wired_offline();
    assert_eq!(client.notification_detail_level(), NotificationDetailLevel::None, "precondition");

    w.invoke_settings_notification_detail_changed("name_only".into());
    assert_eq!(client.notification_detail_level(), NotificationDetailLevel::NameOnly);
    assert_eq!(w.get_settings_notification_detail().as_str(), "name_only");

    drop(w);
    drop(client);
    let _guard = rt.enter();
    let client = chatter_core::client::ChatterClient::builder()
        .data_dir(tmp.path().to_string_lossy().to_string())
        .build()
        .expect("offline ChatterClient::build (restart)");
    assert_eq!(
        client.notification_detail_level(),
        NotificationDetailLevel::NameOnly,
        "detail level did not survive a restart"
    );
}

/// Settings > Appearance > Theme: a pick reaches core, flips the palette the
/// whole UI reads (`Appearance.dark` drives every `Colors` token), and survives
/// a restart.
#[test]
fn settings_theme_pick_persists_and_flips_palette() {
    use chatter_core::services::appearance::ThemeMode;
    use slint::ComponentHandle;
    let (rt, tmp, client, w) = wired_offline();
    let appearance = w.global::<chatter_ui::Appearance>();
    assert_eq!(client.theme_mode(), ThemeMode::System, "precondition");

    appearance.invoke_theme_changed("light".into());
    assert_eq!(client.theme_mode(), ThemeMode::Light);
    assert!(!appearance.get_dark(), "light pick left the dark palette in force");
    appearance.invoke_theme_changed("dark".into());
    assert!(appearance.get_dark());

    drop(w);
    drop(client);
    let _guard = rt.enter();
    let client = chatter_core::client::ChatterClient::builder()
        .data_dir(tmp.path().to_string_lossy().to_string())
        .build()
        .expect("offline ChatterClient::build (restart)");
    assert_eq!(client.theme_mode(), ThemeMode::Dark, "theme did not survive a restart");
}

#[test]
fn settings_glass_effects_persists_and_restores_before_first_frame() {
    use slint::ComponentHandle;
    let (rt, tmp, client, w) = wired_offline();
    let appearance = w.global::<chatter_ui::Appearance>();
    assert!(client.glass_effects_enabled());
    assert!(appearance.get_glass_effects());

    appearance.invoke_glass_effects_changed(false);
    assert!(!client.glass_effects_enabled());
    assert!(!appearance.get_glass_effects());
    drop(w);
    drop(client);

    let _guard = rt.enter();
    let client = chatter_core::client::ChatterClient::builder()
        .data_dir(tmp.path().to_string_lossy().to_string())
        .build()
        .expect("offline client restart");
    let w = MainWindow::new().expect("restarted window");
    chatter_ui::bin_ui::features::modals::settings::wire_settings_callbacks(&w, &client);
    let appearance = w.global::<chatter_ui::Appearance>();
    assert!(!appearance.get_glass_effects(), "saved preference must apply before showing the window");
    appearance.invoke_glass_effects_changed(true);
    assert!(client.glass_effects_enabled());
    assert!(appearance.get_glass_effects());
}

#[test]
fn settings_read_receipts_toggle_reaches_core() {
    require_element_queries();
    let (_rt, _tmp, client, w) = wired_offline();
    // Default is true (get_bool_setting.unwrap_or(true)).
    assert_eq!(client.read_receipts_enabled().unwrap(), true, "precondition");
    // Fire the exact callback the read-receipts Toggle invokes.
    w.invoke_settings_read_receipts_changed(false);
    assert_eq!(
        client.read_receipts_enabled().unwrap(),
        false,
        "read-receipts toggle did NOT reach core (dead/stub wire)"
    );
    w.invoke_settings_read_receipts_changed(true);
    assert_eq!(client.read_receipts_enabled().unwrap(), true, "toggle did not flip back on");
}

// ============================================================================
// ELEMENT-QUERY (accessible-label) — proves the labels we added make the
// settings toggles queryable AND that the switch's accessible-checked state
// tracks the bound property. This is the query layer the mission asks for
// (find_by_accessible_label → read state).
// ============================================================================

/// Crown-jewel end-to-end proof. Uses the INTEGRATION testing backend (has an
/// event-loop provider) and pumps the loop once so the conditional SettingsScreen
/// child (gated by settings-open) actually MATERIALIZES — a plain
/// `init_no_event_loop` never renders, so conditional/repeated sub-trees never
/// instantiate and the toggle can't be found. Then:
///   1. the accessible-label we added resolves to exactly one `switch` element,
///   2. its accessible-checked tracks the bound property (display wire), and
///   3. a REAL pointer click traverses the whole chain
///      (Toggle.toggled → SettingsScreen.read-receipts-changed → App →
///       MainWindow → Rust on_settings_read_receipts_changed) into chatter-core.
///
/// Routing wire, proven dynamically. Pushing "settings" on the modal stack
/// must instantiate the conditional `SettingsScreen` sub-tree
/// (`ui/app.slint  if root.settings-open: SettingsScreen`); closing it must
/// tear the sub-tree down. If that `if`-binding were dead, the element
/// count would never change. The built-in negative control is the pre-set state
/// (count 0).
///
/// NOTE ON HEADLESS LIMITATION: the testing backend instantiates the top of a
/// conditional sub-tree but does NOT lay out its deep interactive descendants
/// without a real render pass — `find_by_element_type_name("SettingsScreen")`
/// returns 1 while the nested `Toggle` widgets are not yet in the tree. So a
/// real `mock_single_click` on the deep toggle is not reachable here; the
/// read-receipts handler→core wire is instead proven by
/// `settings_read_receipts_toggle_reaches_core` (invoke→core), and the full
/// TouchArea→callback chain is verified statically. Single #[test] because the
/// integration backend is once-per-process.
#[test]
fn settings_modal_routing_wire_instantiates_conditional() {
    require_element_queries();
    use slint::ComponentHandle;

    testing::init_integration_test_with_system_time();
    let w = MainWindow::new().expect("MainWindow::new");

    let count_settings =
        || testing::ElementHandle::find_by_element_type_name(&w, "SettingsScreen").count();

    // Force a layout pass (a headless `if` needs one to (de)materialise).
    let pump = |w: &MainWindow| {
        let scale = w.window().scale_factor();
        w.window().dispatch_event(slint::platform::WindowEvent::Resized {
            size: slint::LogicalSize::new(1000.0, 800.0).to_physical(scale).to_logical(scale),
        });
        slint::Timer::single_shot(std::time::Duration::from_millis(80), || {
            slint::quit_event_loop().unwrap();
        });
        w.run().unwrap();
    };

    // Negative control: modal not open → SettingsScreen absent.
    pump(&w);
    assert_eq!(count_settings(), 0, "SettingsScreen present before the modal was opened");

    // Push Settings → conditional must appear.
    chatter_ui::bin_ui::features::back::push(&w, "settings");
    pump(&w);
    assert_eq!(
        count_settings(),
        1,
        "pushing 'settings' did NOT instantiate SettingsScreen — the \
         `if root.settings-open` routing wire is dead"
    );

    // Something Settings opens keeps it mounted underneath.
    chatter_ui::bin_ui::features::back::push(&w, "sessions");
    pump(&w);
    assert_eq!(count_settings(), 1, "Sessions over Settings unmounted Settings");

    // Close it → conditional must tear down (with what it opened).
    chatter_ui::bin_ui::features::back::close(&w, "settings");
    pump(&w);
    assert_eq!(count_settings(), 0, "closing Settings did NOT remove SettingsScreen");
}

// ============================================================================
// AVATAR EXCHANGE — display hop (decode → Slint Image → bound property)
//
// The full avatar pipeline is set→encrypt→upload→friend-fetch→decrypt→bind.
// The first four hops are core/server (deferred to live-server). This proves
// the LAST, previously-uncovered hop end-to-end at the Slint boundary: raw
// decoded bytes (exactly what `decrypt` yields) → `avatar_bytes_to_image` →
// the `user-avatar-source` in-property the Avatar element renders. If the
// converter dropped the image or the property alias were dead, this reads back
// a zero-size image.
// ============================================================================

/// A valid 2x2 RGB PNG (red/green pixels). Stands in for decrypted avatar bytes.
const TINY_PNG: &[u8] = &[
    137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 2, 0, 0, 0, 2, 8, 2, 0,
    0, 0, 253, 212, 154, 115, 0, 0, 0, 17, 73, 68, 65, 84, 120, 156, 99, 248, 207, 192, 192, 240,
    31, 140, 128, 24, 0, 29, 240, 3, 253, 211, 208, 125, 38, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66,
    96, 130,
];

#[test]
fn avatar_bytes_decode_and_bind_to_display_property() {
    use chatter_ui::bin_ui::converters::avatar_bytes_to_image;
    let w = boot();

    // Positive control: garbage decodes to None (converter is not a passthrough).
    assert!(
        avatar_bytes_to_image(&[1, 2, 3, 4]).is_none(),
        "converter accepted non-image bytes — it isn't actually decoding"
    );

    // Real decrypted-shaped bytes → a real Image.
    let img = avatar_bytes_to_image(TINY_PNG).expect("valid PNG must decode to an Image");
    assert!(img.size().width > 0 && img.size().height > 0, "decoded avatar has zero size");

    // Bind it to the display property the Avatar element reads.
    w.set_user_avatar_source(img);
    let bound = w.get_user_avatar_source();
    assert!(
        bound.size().width > 0 && bound.size().height > 0,
        "user-avatar-source did not retain the bound Image (dead property alias)"
    );
}

#[test]
fn settings_typing_indicators_toggle_reaches_core() {
    let (_rt, _tmp, client, w) = wired_offline();
    assert_eq!(client.typing_indicators_enabled().unwrap(), true, "precondition");
    w.invoke_settings_typing_indicators_changed(false);
    assert_eq!(
        client.typing_indicators_enabled().unwrap(),
        false,
        "typing-indicators toggle did NOT reach core (dead/stub wire)"
    );
    w.invoke_settings_typing_indicators_changed(true);
    assert_eq!(client.typing_indicators_enabled().unwrap(), true, "toggle did not flip back on");
}

// ============================================================================
// F-UI-7 (RESOLVED): the "Delete Account" button is no longer a dead wire.
//
// The delete flow can't be driven to completion headlessly (it is async +
// network + routes on the event loop, and `wired_offline` never spins the loop),
// so this proves the wire at two layers, and NAMES which:
//
//  L1 (DYNAMIC, MainWindow boundary): the generated
//      `invoke_settings_delete_account_confirmed` exists and, when invoked,
//      runs the REAL production handler (from wire_settings_callbacks) without
//      panicking. An undeclared/unwired callback would not compile / would be a
//      no-op; this confirms the callback chain main->app->SettingsScreen->Rust
//      is connected.
//
//  L2 (STATIC, reaches-core + confirm-gate): the production handler source wires
//      the CONFIRMED callback to `ChatterClient::delete_account`, and does NOT
//      delete on the first (unconfirmed) click — i.e. deletion is gated behind an
//      explicit confirmation step. The actual server-side deletion is proven
//      end-to-end by validated.rs::v28 (the wrapper really removes the users row).
// ============================================================================
#[test]
fn settings_delete_account_is_wired_behind_confirmation() {
    // ---- L1: callback is declared and reaches the real handler --------------
    let (rt, _tmp, _client, w) = wired_offline();
    // The confirmed handler spawns onto the tokio runtime; enter it so the wire
    // can run (the task is queued, not driven — we assert the wire, not delete).
    let _guard = rt.enter();
    // Must not panic: the confirmed-delete callback is wired to a Rust handler.
    // (Offline + no logged-in user → the handler's async delete resolves to a
    // NotLoggedIn error it surfaces on the loop; we only assert the wire runs.)
    w.invoke_settings_delete_account_confirmed("irrelevant-password".into());
    // The first-click (unconfirmed) callback must also be wired and safe to call.
    w.invoke_settings_delete_account();

    // ---- L2: static — confirmed handler reaches the wrapper; gated by confirm -
    let settings_src = include_str!("../src/bin_ui/features/modals/settings.rs");
    assert!(
        settings_src.contains("on_settings_delete_account_confirmed"),
        "settings.rs must register the confirmed-delete handler"
    );
    // The confirmed handler must reach the ChatterClient::delete_account wrapper.
    let confirmed_idx = settings_src
        .find("on_settings_delete_account_confirmed")
        .expect("confirmed handler present");
    let after_confirmed = &settings_src[confirmed_idx..];
    assert!(
        after_confirmed.contains(".delete_account(&password)")
            || after_confirmed.contains(".delete_account("),
        "the confirmed-delete handler must call ChatterClient::delete_account (dead wire otherwise)"
    );

    // The Slint layer must gate the confirmed callback behind an explicit confirm
    // reveal (`show-delete-confirm`) — no deletion without confirmation.
    let screen_src = include_str!("../../../ui/settings/settings_screen.slint");
    assert!(
        screen_src.contains("show-delete-confirm")
            && screen_src.contains("delete-account-confirmed(root.delete-password)"),
        "the Delete button must reveal a confirm step whose Confirm action carries the \
         password to delete-account-confirmed — deletion must be gated behind confirmation"
    );
}

// ============================================================================
// F-UI-1 (RESOLVED for mute): the channel-settings MUTE toggle now persists.
//
//  L1 (DYNAMIC): `on_channel_settings_toggle_mute` runs the real handler without
//      panicking. Offline (no logged-in user / membership) the persist resolves
//      to an error the handler logs; we assert the WIRE runs.
//  L2 (STATIC): the handler calls `mute_conversation` (not just a Slint setter).
//      The real membership persistence is proven by validated.rs::v33.
// ============================================================================
#[test]
fn channel_settings_mute_toggle_reaches_core() {
    no_keyring();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    let tmp = tempfile::tempdir().unwrap();
    testing::init_no_event_loop();
    let client = chatter_core::client::ChatterClient::builder()
        .data_dir(tmp.path().to_string_lossy().to_string())
        .build()
        .expect("offline ChatterClient::build");
    let w = MainWindow::new().expect("MainWindow::new");

    chatter_ui::bin_ui::features::modals::wire_channel_settings(&w, &client);
    // Must not panic: the toggle is wired to a Rust handler that spawns the
    // persist task (the bool-set + persist are deferred onto the event loop /
    // tokio, which this no-event-loop harness does not pump — so we assert the
    // WIRE runs, and leave the persisted-state assertion to validated.rs::v33).
    w.invoke_channel_settings_toggle_mute(true);

    // ---- L2 static: handler reaches the mute_conversation core method --------
    let src = include_str!("../src/bin_ui/features/modals/channel_settings.rs");
    let idx = src.find("on_channel_settings_toggle_mute").expect("mute handler present");
    let after = &src[idx..];
    assert!(
        after.contains(".mute_conversation("),
        "the mute toggle must call ChatterClient::mute_conversation (was a Slint-only bool)"
    );
}

// ============================================================================
// AVATAR CHANGE (RESOLVED): the picked bytes reach `set_avatar_bytes`, and they
// do it on SAVE rather than on PICK.
//
// The avatar picker is behind a native file dialog, so it can't be driven
// headlessly. L2 (STATIC): the picker stashes the bytes in
// `PENDING_AVATAR_BYTES`, and `on_edit_profile_save` persists them with
// `set_avatar_bytes` before `update_user_profile` re-reads `keys::AVATAR` to
// build the upload. Persisting at PICK time meant Cancel kept the new avatar —
// observed on a Galaxy S9, the bottom-nav avatar had already changed (§112v).
// The full upload+server-store+decrypt round-trip is proven by validated.rs::v32.
// ============================================================================
#[test]
fn edit_profile_avatar_change_persists_picked_bytes_on_save() {
    let src = include_str!("../src/bin_ui/features/modals/profile.rs");

    let pick = src.find("on_edit_profile_change_avatar").expect("avatar handler present");
    let pick_body = &src[pick..src[pick..].find("on_profile_view_safety_number").unwrap() + pick];
    assert!(
        pick_body.contains("PENDING_AVATAR_BYTES"),
        "the picker must stash the picked bytes for Save"
    );
    assert!(
        !pick_body.contains(".set_avatar_bytes("),
        "the picker must NOT persist the avatar — Cancel would keep it"
    );

    let save = src.find("on_edit_profile_save").expect("save handler present");
    let save_end = src[save..]
        .find("on_edit_profile_change_avatar")
        .map(|i| i + save)
        .expect("the avatar picker handler follows Save in this file");
    let save_body = &src[save..save_end];
    assert!(
        save_body.contains("PENDING_AVATAR_BYTES") && save_body.contains(".set_avatar_bytes("),
        "Save must persist the stashed bytes before update_user_profile uploads them"
    );
}

// ============================================================================
// F-UI-1 (RESOLVED for name/description): the channel-settings modal now has a
// working Save. Was fully dead — the modal declared `save-settings` but NEVER
// invoked it, and the name/description Inputs' `edited()` handlers were empty
// stubs (`// Will be saved on explicit save action`), so editing persisted
// nothing. The Rust `on_channel_settings_save` handler existed and reached
// `update_channel` all along; only the Slint wire was cut.
//
//  Draft round-trip: the editable draft properties exist + bind (the Inputs
//    write here, Save reads them).
//  L1 (DYNAMIC): `on_channel_settings_save` runs the real handler without
//    panicking (offline it early-returns on the empty channel-id).
//  L2 (STATIC, Rust): the handler calls `update_channel` (real persistence,
//    proven end-to-end by validated.rs::v30 rename propagation).
//  L2 (STATIC, Slint): the Save button INVOKES `save-settings` with the drafts
//    and both Inputs WRITE the drafts on edit — the exact wire that was dead.
// ============================================================================
#[test]
fn channel_settings_save_reaches_core_and_drafts_are_wired() {
    no_keyring();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    let tmp = tempfile::tempdir().unwrap();
    testing::init_no_event_loop();
    let client = chatter_core::client::ChatterClient::builder()
        .data_dir(tmp.path().to_string_lossy().to_string())
        .build()
        .expect("offline ChatterClient::build");
    let w = MainWindow::new().expect("MainWindow::new");
    chatter_ui::bin_ui::features::modals::wire_channel_settings(&w, &client);

    // Draft properties exist and round-trip (Inputs write here; Save reads).
    w.set_channel_settings_name_draft("Renamed Channel".into());
    w.set_channel_settings_description_draft("New description".into());
    assert_eq!(w.get_channel_settings_name_draft().as_str(), "Renamed Channel");
    assert_eq!(w.get_channel_settings_description_draft().as_str(), "New description");

    // L1 dynamic: the save callback runs the real handler without panicking.
    w.invoke_channel_settings_save(
        w.get_channel_settings_name_draft(),
        w.get_channel_settings_description_draft(),
    );

    // L2 static (Rust): the handler reaches ChatterClient::update_channel.
    let src = include_str!("../src/bin_ui/features/modals/channel_settings.rs");
    let idx = src.find("on_channel_settings_save").expect("save handler present");
    assert!(
        src[idx..].contains(".update_channel("),
        "the save handler must call ChatterClient::update_channel (was Slint-only / dead)"
    );

    // L2 static (Slint): the Save button invokes save-settings with the drafts,
    // and both Inputs write the drafts on edit (was an empty stub + no button).
    let slint_src = include_str!("../../../ui/modals/channel_settings.slint");
    assert!(
        slint_src.contains("root.save-settings(root.channel-name-draft"),
        "the Save button must invoke save-settings with the name/description drafts"
    );
    assert!(
        slint_src.contains("root.channel-name-draft = value"),
        "the name Input must write its draft on edit (was an empty stub)"
    );
    assert!(
        slint_src.contains("root.channel-description-draft = value"),
        "the description Input must write its draft on edit (was an empty stub)"
    );
}

// ============================================================================
// #3b (RESOLVED): the channel-settings modal now has a working ICON picker.
// Was a triple-unwired scoped gap — facade hardcoded icon `None`,
// `upload_channel_icon` had zero UI callers, and the modal had no picker.
//
//  Property round-trip: `channel-settings-icon-path` exists + binds.
//  L1 (DYNAMIC): `on_channel_settings_pick_icon` runs without panicking.
//  L2 (STATIC, Rust): the pick handler stashes the chosen path via
//    `set_channel_settings_icon_path`, and Save reads it
//    (`get_channel_settings_icon_path`) and passes the bytes to update_channel.
//  L2 (STATIC, Slint): the modal has a Change-Icon button invoking
//    `pick-channel-icon`. Icon propagation to members proven by validated.rs::v30.
//  L2 (STATIC, core): the facade `ChatterClient::update_channel` routes the
//    icon through `fit_icon_bytes` (≤64KB inline-proto cap), same as creation.
// (The native file dialog itself can't run headless — same as the avatar picker.
//  The parallel dead `upload_channel_icon` CDN path was removed clean-break.)
// ============================================================================
#[test]
fn channel_settings_icon_picker_is_wired() {
    no_keyring();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    let tmp = tempfile::tempdir().unwrap();
    testing::init_no_event_loop();
    let client = chatter_core::client::ChatterClient::builder()
        .data_dir(tmp.path().to_string_lossy().to_string())
        .build()
        .expect("offline ChatterClient::build");
    let w = MainWindow::new().expect("MainWindow::new");
    chatter_ui::bin_ui::features::modals::wire_channel_settings(&w, &client);

    // The icon-path property exists and round-trips (Save reads it).
    w.set_channel_settings_icon_path("/tmp/some-icon.png".into());
    assert_eq!(w.get_channel_settings_icon_path().as_str(), "/tmp/some-icon.png");

    // L1 dynamic: the pick callback runs without panicking. Under
    // CHATTER_HEADLESS (set by the harness) FilePicker resolves to "cancelled"
    // rather than opening a real dialog, so this never waits for a human and
    // never pops a window mid-test-run.
    assert!(
        std::env::var_os("CHATTER_HEADLESS").is_some(),
        "the harness must force headless dialogs before invoking a picker"
    );
    w.invoke_channel_settings_pick_icon();

    // The picker resolved without a selection, so it must not have stashed a
    // path over the one set above.
    assert_eq!(
        w.get_channel_settings_icon_path().as_str(),
        "/tmp/some-icon.png",
        "a headless picker should cancel, not overwrite the stashed path"
    );

    // L2 static (Rust): pick handler stashes the path; Save reads it + uploads.
    let src = include_str!("../src/bin_ui/features/modals/channel_settings.rs");
    let pick_idx = src.find("on_channel_settings_pick_icon").expect("pick-icon handler present");
    assert!(
        src[pick_idx..].contains(".set_channel_settings_icon_path("),
        "the pick handler must stash the chosen path on channel-settings-icon-path"
    );
    let save_idx = src.find("on_channel_settings_save").expect("save handler present");
    let save_body = &src[save_idx..];
    assert!(
        save_body.contains(".get_channel_settings_icon_path(")
            && save_body.contains("icon_bytes.as_deref()"),
        "Save must read the picked icon path and pass its bytes to update_channel"
    );

    // L2 static (Slint): the modal has a Change-Icon button invoking pick-channel-icon.
    let slint_src = include_str!("../../../ui/modals/channel_settings.slint");
    assert!(
        slint_src.contains("root.pick-channel-icon()"),
        "the modal must have a picker affordance invoking pick-channel-icon"
    );

    // L2 static (core): the facade fits oversized icons before the RPC, so a
    // raw camera JPEG picked here can't blow the 64KB inline-proto cap.
    let core_src = include_str!("../../chatter-core/src/client/channels.rs");
    let upd_idx = core_src
        .find("pub async fn update_channel")
        .expect("facade update_channel present");
    assert!(
        core_src[upd_idx..].contains("fit_icon_bytes"),
        "ChatterClient::update_channel must route picked icon bytes through fit_icon_bytes"
    );
}

// ============================================================================
// THREAD REPLY (RESOLVED): you can now START a thread from any message.
//
// Was: the only thread entry was the "N replies" badge rendered
// `if reply-count > 0`, so a message with zero replies had NO way to open a
// thread — the send-into-thread handler + panel existed but were unreachable
// (reported: "channel reply works but no option to reply in thread").
//
//  L1 (DYNAMIC): `on_thread_clicked` runs the real handler without panicking
//    (offline it warns "no active channel" and returns).
//  L2 (STATIC, Rust): the handler opens the thread (`open_thread` +
//    `load_thread_data`) — proven to persist/round-trip by validated.rs::v44.
//  L2 (STATIC, Slint): a "Reply in thread" button lives in the message hover
//    action row (NOT gated behind reply-count>0) and fires thread-clicked.
// ============================================================================
#[test]
fn message_reply_in_thread_button_is_wired() {
    no_keyring();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    let tmp = tempfile::tempdir().unwrap();
    testing::init_no_event_loop();
    let client = chatter_core::client::ChatterClient::builder()
        .data_dir(tmp.path().to_string_lossy().to_string())
        .build()
        .expect("offline ChatterClient::build");
    let w = MainWindow::new().expect("MainWindow::new");
    chatter_ui::bin_ui::features::chat_actions::wire(&w, &client);

    // L1 dynamic: firing the thread callback runs the real handler (offline:
    // no active channel → warns + returns), no panic.
    w.invoke_thread_clicked("00000000-0000-0000-0000-000000000000".into());

    // L2 static (Rust): the handler opens a thread (state + panel load).
    let src = include_str!("../src/bin_ui/features/chat_actions/message_actions.rs");
    let idx = src.find("on_thread_clicked").expect("thread handler present");
    assert!(
        src[idx..].contains(".open_thread(") && src[idx..].contains("load_thread_data"),
        "the thread handler must open the thread state + load the thread panel"
    );

    // L2 static (Slint): a "Reply in thread" affordance exists in the message
    // action row (so ANY message can start a thread), firing thread-clicked.
    let slint_src = include_str!("../../../ui/chat/message_item.slint");
    assert!(
        slint_src.contains("thread-btn := IconAction {")
            && slint_src.contains(r#"label: "Reply in thread""#),
        "the message hover actions must include a Reply-in-thread button so a \
         message with zero replies can still START a thread"
    );
    let icon_action = include_str!("../../../ui/components/icon_action.slint");
    assert!(icon_action.contains("accessible-label: root.label;"));
    assert!(icon_action.contains("accessible-action-default => { if root.enabled { root.clicked(); } }"));
    assert!(
        slint_src.contains("root.thread-clicked(root.message-id)"),
        "the Reply-in-thread button must fire thread-clicked with the message id"
    );
}

// ============================================================================
// Reactions / edits / pins — the MESSAGE ACTION wires reach core.
//
// Core round-trip proven by validated.rs::v45 (channel) + v46 (DM). This proves
// the UI side: the emoji picker, the composer edit, and the pin action are
// wired to the ConversationHandle methods (not Slint-only no-ops).
//
//  L1 (DYNAMIC): each callback runs its real handler without panicking
//    (offline: no active conversation, the spawned task logs + returns).
//  L2 (STATIC, Rust): reaction -> toggle_reaction, edit -> send_edit,
//    pin -> send_pin.
//  L2 (STATIC, Slint): the picker invokes reaction-selected, the composer
//    invokes send-edit, the message actions invoke edit-message + pin-message.
// ============================================================================
#[test]
fn message_react_edit_pin_actions_are_wired() {
    no_keyring();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    let tmp = tempfile::tempdir().unwrap();
    testing::init_no_event_loop();
    let client = chatter_core::client::ChatterClient::builder()
        .data_dir(tmp.path().to_string_lossy().to_string())
        .build()
        .expect("offline ChatterClient::build");
    let w = MainWindow::new().expect("MainWindow::new");
    chatter_ui::bin_ui::features::chat::wire(&w, &client);

    // L1 dynamic: the four action callbacks run their real handlers, no panic.
    w.invoke_reaction_selected("s".into(), "1".into(), "m".into(), "\u{1F44D}".into());
    w.invoke_edit_message("s".into(), "1".into());
    w.invoke_send_edit("s|1".into(), "edited".into());
    w.invoke_pin_message("s".into(), "1".into());

    // L2 static (Rust): each handler reaches the ConversationHandle core method.
    let react_src = include_str!("../src/bin_ui/features/chat/reactions.rs");
    let ri = react_src.find("on_reaction_selected").expect("reaction handler present");
    assert!(react_src[ri..].contains(".toggle_reaction("),
        "the reaction picker must call ConversationHandle::toggle_reaction");

    let edit_src = include_str!("../src/bin_ui/features/chat/edit.rs");
    let ei = edit_src.find("on_send_edit").expect("send-edit handler present");
    assert!(edit_src[ei..].contains(".send_edit("),
        "the composer edit must call ConversationHandle::send_edit");

    let pin_src = include_str!("../src/bin_ui/features/chat/pin.rs");
    let pi = pin_src.find("on_pin_message").expect("pin handler present");
    assert!(pin_src[pi..].contains(".send_pin("),
        "the pin action must call ConversationHandle::send_pin");

    // L2 static (Slint): the affordances actually invoke the callbacks.
    let picker = include_str!("../../../ui/chat/reaction_picker.slint");
    assert!(picker.contains("root.reaction-selected("),
        "the reaction picker must invoke reaction-selected");
    let composer = include_str!("../../../ui/chat/message_composer.slint");
    assert!(composer.contains("root.send-edit("),
        "the composer must invoke send-edit when editing");
    let app = include_str!("../../../ui/app.slint");
    assert!(app.contains("root.edit-message(") && app.contains("root.pin-message("),
        "the message actions must invoke edit-message and pin-message");
}

// ============================================================================
// DEVICE-LINK WIRES — new-device page (ui/pages/link_device.slint) and the
// primary-side approve modal (ui/modals/link_device_approve.slint). Offline:
// the start callback's synchronous half (active/status flip), cancel routing,
// and the paste→core parse round-trip (`prepare_link_from_url`) need no server.
// The full linking round-trip against the live stack is proven server-side by
// validated.rs V8/V9/V11 and tests/integration_provisioning.rs.
// ============================================================================

#[test]
fn link_device_page_start_and_cancel_are_wired() {
    let (rt, _tmp, client, w) = wired_offline();
    chatter_ui::init_rt_handle(rt.handle().clone());
    chatter_ui::bin_ui::features::auth::link_device::wire_link_device(&w, &client);

    // Negative control: nothing active before the callback fires.
    assert!(!w.get_link_device_active(), "precondition: link flow inactive");

    // The "Generate QR" button fires start → the handler must flip the bound
    // state synchronously (QR generation itself continues async and offline
    // will error later — the wire is what's under test here).
    w.invoke_link_device_start();
    assert!(w.get_link_device_active(), "start did NOT reach the handler (dead wire)");
    assert_eq!(w.get_link_device_status().as_str(), "Generating QR code…");

    // Cancel routes back to login.
    w.set_current_view("link-device".into());
    w.invoke_link_device_cancel();
    assert_eq!(w.get_current_view().as_str(), "login", "cancel did not route back to login");
}

// ============================================================================
// LOCAL AI — the AI channel + settings wires (feature `jarvis`, default-on).
//
// Core round-trip (detect → pull with progress → chat) is proven live by
// chatter-core `live_detect_finds_local_ollama` / `live_pull_and_chat`
// (--ignored, needs Ollama). These prove the UI side.
// ============================================================================

/// The sidebar injects the virtual AI channel (nil-UUID) at the top when
/// local AI is enabled — pure core logic, provable offline.
///
/// This test asserted "local AI defaults on" and so had been failing since
/// 69fd7878 ("fix(client): local AI off by default", 2026-08-16) flipped
/// `LocalAISettings::default().enabled` to false
/// (`crates/chatter-core/src/local_ai/settings.rs:40`). Nothing ran the suite,
/// so nobody saw it; CI now does. The property under test is unchanged — it
/// just has to turn local AI on first, and the default-off case is now pinned
/// too, since that is the state a fresh install is actually in.
#[cfg(feature = "jarvis")]
#[test]
fn sidebar_injects_ai_channel_and_never_autoselects_it() {
    no_keyring();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().to_string_lossy().to_string();
    let client = chatter_core::client::ChatterClient::builder()
        .data_dir(data_dir.clone())
        .build()
        .expect("offline ChatterClient::build");

    // Default-off: a fresh install shows no AI channel at all.
    assert!(
        client.sidebar_channels().is_empty(),
        "local AI is off by default (settings.rs:40), so a fresh profile must \
         show no AI channel — if this fails the default flipped back"
    );

    client
        .set_local_ai_enabled(true)
        .expect("enabling local AI is a settings write, not a network call");

    // Rebuild the client rather than reusing it: LocalAIService holds a
    // settings SNAPSHOT taken at construction
    // (chatter-core/src/services/local_ai_service.rs:434), so a running client
    // does not observe its own write and the AI channel appears only after a
    // restart. That staleness is a separate bug — docs/OPEN-ITEMS.md §64 — and
    // is deliberately NOT asserted here, so fixing it does not turn this test
    // red. What is under test is the injection itself.
    drop(client);
    let client = chatter_core::client::ChatterClient::builder()
        .data_dir(data_dir)
        .build()
        .expect("offline ChatterClient::build (restart)");

    let channels = client.sidebar_channels();
    assert!(!channels.is_empty(), "AI channel missing from sidebar with local AI ON");
    let first = &channels[0];
    assert!(
        chatter_core::client::is_jarvis_channel(&first.id),
        "first sidebar channel must be the AI channel, got {:?}",
        first.id
    );
    assert_eq!(first.name, chatter_core::client::JARVIS_CHANNEL_NAME);

    // Never auto-selected on load.
    assert!(
        client.first_selectable_channel_id().is_none(),
        "the AI channel must be excluded from auto-select"
    );
}

/// Selecting the AI channel routes to the Jarvis loader (not the network
/// channel loader), and sending in it routes to Jarvis (not send_message_ui).
#[cfg(feature = "jarvis")]
#[test]
fn ai_channel_select_and_send_are_wired_to_jarvis() {
    no_keyring();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    let tmp = tempfile::tempdir().unwrap();
    testing::init_no_event_loop();
    let client = chatter_core::client::ChatterClient::builder()
        .data_dir(tmp.path().to_string_lossy().to_string())
        .build()
        .expect("offline ChatterClient::build");
    let w = MainWindow::new().expect("MainWindow::new");
    chatter_ui::bin_ui::features::chat::wire(&w, &client);

    // L1 dynamic: with the AI channel active, firing `send` runs the intercept
    // (spawns the Jarvis task; offline it errors later — the WIRE is under test).
    let nil: chatter_core::domain::ids::ChannelId = uuid::Uuid::nil().into();
    client.chat_state().set_active_channel(nil);
    client
        .chat_state()
        .set_active_conversation(chatter_core::state::ActiveConversation::Channel(nil));
    w.invoke_send("hello jarvis".into());

    // L2 static (Rust): the send handler branches on the AI channel BEFORE the
    // network pipeline and routes to the Jarvis channel handler.
    let send_src = include_str!("../src/bin_ui/features/chat/send_message.rs");
    let idx = send_src.find("is_jarvis_channel").expect("send must branch on the AI channel");
    assert!(
        send_src[idx..].contains("handle_jarvis_channel_send"),
        "AI-channel sends must route to handle_jarvis_channel_send, not send_message_ui"
    );
    assert!(
        idx < send_src.find("send_message_ui").unwrap(),
        "the AI-channel branch must come before the network send"
    );

    // L2 static (Rust): channel selection branches to the Jarvis history loader.
    let sel_src = include_str!("../src/bin_ui/features/sidebar/channel.rs");
    let sidx = sel_src.find("is_jarvis_channel_str").expect("selection must branch on the AI channel");
    assert!(
        sel_src[sidx..].contains("load_jarvis_channel_into_ui"),
        "selecting the AI channel must load Jarvis history, not the network channel loader"
    );

    // L2 static (Slint): Ctrl+J opens the channel (the old sidebar Jarvis
    // button is deleted — the channel-list row is the entry point).
    let app_src = include_str!("../../../ui/app.slint");
    assert!(
        app_src.contains(r#"root.channel-selected("00000000-0000-0000-0000-000000000000")"#),
        "Ctrl+J must open the AI channel"
    );
    // The floating panel is gone.
    assert!(!app_src.contains("JarvisPanel"), "the Jarvis floating panel must stay deleted");

    // The spawned Jarvis task parks on invoke_from_event_loop (this harness
    // never pumps the loop), so a plain Runtime::drop would hang waiting for
    // it. Shut down without waiting — production has a real event loop.
    drop(_guard);
    rt.shutdown_background();
}

/// Settings local-AI section: refresh/select/download callbacks are wired to
/// the real handlers, which reach the core façade.
#[cfg(feature = "jarvis")]
#[test]
fn settings_local_ai_section_is_wired_to_core() {
    no_keyring();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    let tmp = tempfile::tempdir().unwrap();
    testing::init_no_event_loop();
    let client = chatter_core::client::ChatterClient::builder()
        .data_dir(tmp.path().to_string_lossy().to_string())
        .build()
        .expect("offline ChatterClient::build");
    let w = MainWindow::new().expect("MainWindow::new");
    chatter_ui::bin_ui::features::modals::local_ai::wire(&w, &client);

    // L1 dynamic: callbacks run their real handlers without panicking
    // (refresh spawns the probe; select-model persists synchronously).
    w.invoke_settings_local_ai_refresh();
    w.invoke_settings_local_ai_select_model("qwen3:4b".into());
    assert_eq!(
        w.get_settings_local_ai_active_model().as_str(),
        "qwen3:4b",
        "select-model did not update the bound active-model property"
    );
    // ...and the choice reached core persistence (real settings row).
    assert_eq!(
        client.local_ai_chat_model(),
        "qwen3:4b",
        "select-model did NOT persist through the core façade (dead/stub wire)"
    );

    // L2 static (Rust): handlers reach the core façade methods.
    let src = include_str!("../src/bin_ui/features/modals/local_ai.rs");
    for needle in [
        "local_ai_detect_providers",
        "local_ai_apply_provider",
        "local_ai_apply_chat_model",
        "local_ai_pull_model",
        "local_ai_hardware_profile",
    ] {
        assert!(src.contains(needle), "local_ai.rs must call ChatterClient::{needle}");
    }

    // L2 static (Slint): the section fires the callbacks.
    let slint_src = include_str!("../../../ui/settings/settings_screen.slint");
    for needle in [
        "root.local-ai-refresh()",
        "root.local-ai-select-provider(",
        "root.local-ai-select-model(",
        "root.local-ai-download(",
    ] {
        assert!(slint_src.contains(needle), "settings screen must invoke {needle}");
    }
}

/// Memory browser: callbacks reach the real handlers, which reach the Jarvis
/// memory façade; the settings entry + modal routing exist in Slint.
#[cfg(feature = "jarvis")]
#[test]
fn jarvis_memory_browser_is_wired() {
    no_keyring();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    let tmp = tempfile::tempdir().unwrap();
    testing::init_no_event_loop();
    let client = chatter_core::client::ChatterClient::builder()
        .data_dir(tmp.path().to_string_lossy().to_string())
        .build()
        .expect("offline ChatterClient::build");
    let w = MainWindow::new().expect("MainWindow::new");
    chatter_ui::bin_ui::features::jarvis::memory_view::wire(&w, &client);

    // L1 dynamic: both callbacks run their real handlers without panicking
    // (offline: empty facts/entities; the refresh task queues a UI update).
    w.invoke_jarvis_memory_refresh();
    w.invoke_jarvis_memory_select_entity("nonexistent-entity".into());

    // L2 static (Rust): handlers reach the memory façade.
    let src = include_str!("../src/bin_ui/features/jarvis/memory_view.rs");
    for needle in ["jarvis_memory_facts", "jarvis_top_entities", "jarvis_entity_connections"] {
        assert!(src.contains(needle), "memory_view.rs must call ChatterClient::{needle}");
    }

    // L2 static (Slint): settings entry opens the modal; modal is routed.
    let settings_src = include_str!("../../../ui/settings/settings_screen.slint");
    assert!(
        settings_src.contains("root.local-ai-open-memory()"),
        "settings must have a View Memory button"
    );
    let app_src = include_str!("../../../ui/app.slint");
    assert!(
        app_src.contains(r#"if active-modal == "jarvis-memory": JarvisMemoryModal"#),
        "app must route active-modal='jarvis-memory' to the modal"
    );
    assert!(
        app_src.contains("root.jarvis-memory-select-entity(id)"),
        "modal entity clicks must reach the Rust handler"
    );
    rt.shutdown_background();
}

#[test]
fn link_approve_paste_reaches_core_parser() {
    let (rt, _tmp, client, w) = wired_offline();
    chatter_ui::init_rt_handle(rt.handle().clone());
    chatter_ui::bin_ui::features::modals::link_approve::wire(&w, &client);

    // Positive control: pre-set the preview flag so a dead wire cannot pass —
    // only the real handler resets it.
    w.set_link_approve_preview_ready(true);

    // Garbage (>16 chars, per the handler's error threshold) → core's
    // prepare_link_from_url must reject; the handler resets the preview and
    // surfaces the parse error.
    w.invoke_link_approve_paste_changed("chatter://not-a-real-provisioning-url".into());
    assert!(
        !w.get_link_approve_preview_ready(),
        "paste did NOT reach the core parser (dead wire)"
    );
    assert!(!w.get_link_approve_error().is_empty(), "parse error was not surfaced");

    // Clearing the box resets the error — the documented empty-string branch.
    w.invoke_link_approve_paste_changed("".into());
    assert!(w.get_link_approve_error().is_empty(), "clearing input did not clear error");
}

// ============================================================================
// CLOUD AI SETTINGS — proves the three cloud-AI affordances reach core.
//
// These matter more than a typical toggle: they decide whether prompt text
// leaves the device, and to whom. A dead wire here means the user thinks they
// turned cloud AI off when they did not.
// ============================================================================

#[test]
fn settings_ai_prefer_cloud_toggle_reaches_core() {
    use chatter_core::ai_policy::channel_ai::SiloPreference;

    require_element_queries();
    let (_rt, _tmp, client, w) = wired_offline();

    // Local-only until the user says otherwise: a default of PreferCloud would
    // send prompts off-device for someone who never asked.
    assert_eq!(
        client.ai_silo_preference(),
        SiloPreference::LocalOnly,
        "precondition: cloud must be opt-in"
    );

    w.invoke_settings_ai_prefer_cloud_changed(true);
    assert_eq!(
        client.ai_silo_preference(),
        SiloPreference::PreferCloud,
        "prefer-cloud toggle did NOT reach core (dead/stub wire)"
    );

    w.invoke_settings_ai_prefer_cloud_changed(false);
    assert_eq!(
        client.ai_silo_preference(),
        SiloPreference::LocalOnly,
        "toggle did not flip back to local-only"
    );
}

#[test]
fn settings_cloud_ai_mode_reaches_core() {
    use chatter_core::cloud_ai::CloudMode;

    require_element_queries();
    let (_rt, _tmp, client, w) = wired_offline();

    assert_eq!(
        client.cloud_ai_settings().mode,
        CloudMode::DirectAnthropic,
        "precondition: bring-your-own-key is the default"
    );

    w.invoke_settings_cloud_ai_mode_changed("server-broker".into());
    assert_eq!(
        client.cloud_ai_settings().mode,
        CloudMode::ServerBroker,
        "'Buy tokens' did NOT reach core (dead/stub wire)"
    );

    w.invoke_settings_cloud_ai_mode_changed("direct-anthropic".into());
    assert_eq!(
        client.cloud_ai_settings().mode,
        CloudMode::DirectAnthropic,
        "'My API key' did not switch the mode back"
    );
}

#[test]
fn settings_cloud_ai_mode_ignores_an_unknown_value() {
    use chatter_core::cloud_ai::CloudMode;

    require_element_queries();
    let (_rt, _tmp, client, w) = wired_offline();

    w.invoke_settings_cloud_ai_mode_changed("not-a-mode".into());

    // Must not fall through to some default - an unparseable mode leaves the
    // stored setting exactly as it was.
    assert_eq!(client.cloud_ai_settings().mode, CloudMode::DirectAnthropic);
}

#[test]
fn settings_cloud_ai_api_key_reaches_core_and_blank_clears_it() {
    require_element_queries();
    let (_rt, _tmp, client, w) = wired_offline();

    assert!(
        !client.cloud_ai_settings().is_available(),
        "precondition: no key configured"
    );

    w.invoke_settings_cloud_ai_api_key_changed("sk-ant-test-key".into());
    let saved = client.cloud_ai_settings();
    assert!(
        saved.is_available(),
        "API key input did NOT reach core (dead/stub wire)"
    );
    assert_eq!(saved.api_key.as_deref(), Some("sk-ant-test-key"));

    // Blank clears rather than storing an empty key that would 401 forever.
    w.invoke_settings_cloud_ai_api_key_changed("   ".into());
    assert!(
        !client.cloud_ai_settings().is_available(),
        "blank key did not clear the stored key"
    );
}

/// The UI records a *preference*; policy still decides. Toggling cloud on must
/// not make a DM resolve to the cloud silo.
#[test]
fn cloud_preference_does_not_override_the_dm_policy() {
    use chatter_core::ai_policy::channel_ai::Resolved;
    use chatter_core::ai_policy::policy::Silo;
    use chatter_core::domain::message::ConversationType;

    require_element_queries();
    let (_rt, _tmp, client, w) = wired_offline();

    // User asks for cloud everywhere and configures a key.
    w.invoke_settings_ai_prefer_cloud_changed(true);
    w.invoke_settings_cloud_ai_api_key_changed("sk-ant-test-key".into());
    client.set_ai_enabled_for("dm-1", true).expect("enable");
    client.set_ai_enabled_for("chan-1", true).expect("enable");

    assert_eq!(
        client.resolve_ai_for("dm-1", &ConversationType::ServerDm, false),
        Resolved::Use(Silo::Local),
        "a DM resolved to the cloud silo despite the policy clamp"
    );
    assert_eq!(
        client.resolve_ai_for("chan-1", &ConversationType::Channel, false),
        Resolved::Use(Silo::Cloud),
        "a channel should honour the cloud preference"
    );
    assert_eq!(
        client.resolve_ai_for("chan-1", &ConversationType::Channel, true),
        Resolved::Off,
        "mesh-delivered traffic must get no AI at all"
    );
}

/// The per-conversation AI switch must actually gate a Jarvis turn, not just
/// sit in settings. Before this, `conv.ai.enabled.<id>` was written by the UI
/// and never read at ask time — a channel with AI off would still answer.
#[test]
fn jarvis_ask_is_gated_by_the_per_conversation_switch() {
    use chatter_core::domain::jarvis::JarvisContext;
    use chatter_core::services::jarvis::JarvisError;

    require_element_queries();
    let (rt, _tmp, client, _w) = wired_offline();

    let channel = chatter_core::domain::ids::ChannelId::new();
    let ctx = JarvisContext::channel(channel.clone());

    // Off by default: refuse, and say *why* in a way the user can act on.
    let err = rt
        .block_on(client.jarvis_ask(&ctx, "hello", None))
        .expect_err("AI answered in a channel where it was never enabled");
    assert!(
        matches!(err, JarvisError::NotEnabledHere),
        "expected NotEnabledHere, got {err:?}"
    );

    // Enabled: the switch no longer blocks. It still fails offline (no model),
    // but it must fail for a *different* reason.
    client
        .set_ai_enabled_for(&channel.to_string(), true)
        .expect("enable AI for the channel");
    match rt.block_on(client.jarvis_ask(&ctx, "hello", None)) {
        Err(JarvisError::NotEnabledHere) => {
            panic!("the per-conversation switch still blocks after being enabled")
        }
        _ => {} // any other outcome means the gate let it through
    }
}

/// The dedicated AI channel is exempt — requiring it to be switched on would
/// be circular, since it exists only to talk to the model.
#[test]
fn the_dedicated_ai_channel_needs_no_opt_in() {
    use chatter_core::client::jarvis_channel_id;
    use chatter_core::domain::jarvis::JarvisContext;
    use chatter_core::services::jarvis::JarvisError;

    require_element_queries();
    let (rt, _tmp, client, _w) = wired_offline();

    let ctx = JarvisContext::channel(jarvis_channel_id());
    match rt.block_on(client.jarvis_ask(&ctx, "hello", None)) {
        Err(JarvisError::NotEnabledHere) => {
            panic!("the AI channel demanded an opt-in to use AI")
        }
        _ => {}
    }
}

/// The per-channel AI toggle in channel settings must reach core — otherwise
/// "addable to any channel" has settings but no affordance.
#[test]
fn channel_settings_ai_toggle_reaches_core() {
    require_element_queries();
    no_keyring();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    let tmp = tempfile::tempdir().unwrap();
    testing::init_no_event_loop();
    let client = chatter_core::client::ChatterClient::builder()
        .data_dir(tmp.path().to_string_lossy().to_string())
        .build()
        .expect("offline ChatterClient::build");
    let w = MainWindow::new().expect("MainWindow::new");

    // The AI toggle lives in the channel-settings modal, so wire that handler
    // set rather than the settings-screen one.
    chatter_ui::bin_ui::features::modals::wire_channel_settings(&w, &client);

    let channel = chatter_core::domain::ids::ChannelId::new();
    let channel_id = channel.to_string();

    // The handler reads the open channel from this property, exactly as the
    // mute toggle does.
    w.set_channel_settings_id(channel_id.clone().into());

    assert!(
        !client.ai_enabled_for(&channel_id),
        "precondition: AI is off in a channel until switched on"
    );

    w.invoke_channel_settings_toggle_ai(true);
    assert!(
        client.ai_enabled_for(&channel_id),
        "channel AI toggle did NOT reach core (dead/stub wire)"
    );

    w.invoke_channel_settings_toggle_ai(false);
    assert!(
        !client.ai_enabled_for(&channel_id),
        "channel AI toggle did not switch back off"
    );
}

/// A public channel must not claim end-to-end encryption.
///
/// Public channels are plaintext by design — the server creates them with
/// `Encrypted:false` and this client ships cleartext to them on purpose. The
/// message-list privacy line used to fall through to "Messages are end-to-end
/// encrypted" for everything that was not a mesh room or geohash cell, so a
/// world-readable channel was labelled private.
///
/// Static rather than dynamic: the privacy line lives inside the message list,
/// which is not instantiated in the headless harness (a dynamic
/// `ElementHandle` query finds no Text and would pass vacuously — verified).
/// This mirrors the "L2 static" checks used elsewhere in this file.
#[test]
fn public_channels_do_not_claim_end_to_end_encryption() {
    let src = include_str!("../../../ui/chat/chat_layout.slint");

    let binding = src
        .split("privacy-line:")
        .nth(1)
        .expect("privacy-line binding present in chat_layout.slint");
    // Only look at the ternary chain, not the rest of the file.
    let binding = &binding[..binding.find(';').expect("privacy-line binding terminated")];

    assert!(
        binding.contains("root.conversation-kind == ConversationKind.channel && !root.private-channel"),
        "the privacy line does not branch on a public channel, so a plaintext \
         public channel is labelled end-to-end encrypted: {binding}"
    );

    // The warning styling (globe + amber) must follow the same rule, or the
    // text says public while the icon says locked.
    let broadcast = src
        .split("public-broadcast:")
        .nth(1)
        .expect("public-broadcast binding present");
    let broadcast = &broadcast[..broadcast.find(';').expect("public-broadcast terminated")];
    assert!(
        broadcast.contains("root.conversation-kind == ConversationKind.channel && !root.private-channel"),
        "public-broadcast does not include public channels, so the lock icon \
         contradicts the privacy text: {broadcast}"
    );
}

// USERNAME LINK (L6) — share-link modal wire.
//
// The full create→upload→resolve loop needs the live server (validated.rs
// V48). Offline, this proves the modal wiring at two layers, and NAMES which:
//
//  L1 (DYNAMIC, MainWindow boundary): the generate/reset/copy callbacks are
//     declared, wired by the REAL `wire_profile_actions`, and safe to invoke;
//     and the share-link display properties round-trip through the
//     main.slint <=> app.slint aliases (a dead alias would read back empty).
//
//  L2 (STATIC, reaches-core): the production handlers wire generate/reset to
//     `create_username_link` (core) and the Slint modal fires the callbacks
//     from the self-view section.
// ============================================================================
#[test]
fn profile_share_username_link_is_wired() {
    // ---- L1: callbacks wired + property aliases live ------------------------
    let (rt, _tmp, client, w) = wired_offline();
    chatter_ui::bin_ui::features::modals::profile::wire_profile_actions(&w, &client);
    let _guard = rt.enter();

    // Property alias liveness: set through MainWindow, read back.
    w.set_profile_share_link("chatter://u/test-id#AAAA".into());
    assert_eq!(
        w.get_profile_share_link().as_str(),
        "chatter://u/test-id#AAAA",
        "profile-share-link alias is dead (main.slint <=> app.slint)"
    );
    w.set_profile_share_status("status-probe".into());
    assert_eq!(w.get_profile_share_status().as_str(), "status-probe");

    // The three wires must run the real handlers without panicking. (Offline +
    // unregistered → the async create resolves to an error surfaced on the
    // loop; we assert the wire runs, like the delete-account test.)
    w.invoke_profile_generate_share_link();
    w.invoke_profile_reset_share_link();
    // Copy with a non-empty link set: handler reads the property and sets the
    // "copied" status synchronously on the UI thread — assert THAT hop.
    w.set_profile_share_status("".into());
    w.invoke_profile_copy_share_link();
    assert!(
        !w.get_profile_share_status().is_empty(),
        "copy-share-link did NOT run the real handler (status not set)"
    );

    // ---- L2: static — handlers reach core; Slint fires the callbacks --------
    let profile_src = include_str!("../src/bin_ui/features/modals/profile.rs");
    assert!(
        profile_src.contains("on_profile_generate_share_link")
            && profile_src.contains("on_profile_reset_share_link")
            && profile_src.contains("on_profile_copy_share_link"),
        "profile.rs must register all three share-link handlers"
    );
    assert!(
        profile_src.contains(".create_username_link()"),
        "generate/reset handlers must reach ChatterClient::create_username_link (dead wire otherwise)"
    );
    let modal_src = include_str!("../../../ui/modals/user_profile.slint");
    assert!(
        modal_src.contains("root.generate-share-link()")
            && modal_src.contains("root.copy-share-link()")
            && modal_src.contains("root.reset-share-link()"),
        "user_profile.slint self-view section must fire the share-link callbacks"
    );
}

// PROFILE SHARED CONTENT — the Media/Files/Links/Voice/GIFs/Music drawer.
//
// The categorisation, ordering and paging are proven in core
// (`storage::repositories::shared_content_repo` tests). This pins the UI half,
// and NAMES which layer:
//
//  L1 (DYNAMIC, MainWindow boundary): the four drawer callbacks are declared,
//     wired by the REAL `wire_profile_actions`, and safe to invoke; and the
//     tab/loading properties round-trip through the main.slint <=> app.slint
//     aliases (a dead alias would read back empty).
//
//  L2 (STATIC, reaches-core): the handlers reach the core façade methods, and
//     the Slint modal fires the callbacks from the tab strip and the grid.
// ============================================================================
#[test]
fn profile_shared_content_drawer_is_wired() {
    let (rt, _tmp, client, w) = wired_offline();
    chatter_ui::bin_ui::features::modals::profile::wire_profile_actions(&w, &client);
    let _guard = rt.enter();

    // Property alias liveness through MainWindow.
    w.set_profile_shared_tab("media".into());
    assert_eq!(
        w.get_profile_shared_tab().as_str(),
        "media",
        "profile-shared-tab alias is dead (main.slint <=> app.slint)"
    );
    w.set_profile_shared_loading(true);
    assert!(w.get_profile_shared_loading());
    w.set_profile_last_seen("last seen recently".into());
    assert_eq!(w.get_profile_last_seen().as_str(), "last seen recently");

    // The wires must run the real handlers without panicking. No profile is
    // open, so each one takes its early-return path — which is exactly the
    // guard being asserted.
    w.invoke_profile_shared_tab_selected("file".into());
    w.invoke_profile_shared_load_more();
    w.invoke_profile_open_shared_item("msg-1".into(), "".into(), "media".into());
    w.invoke_profile_toggle_mute();

    // L2: static — handlers reach core; Slint fires the callbacks.
    let profile_src = include_str!("../src/bin_ui/features/modals/profile.rs");
    for needle in [
        "on_profile_shared_tab_selected",
        "on_profile_shared_load_more",
        "on_profile_open_shared_item",
        "on_profile_toggle_mute",
        "on_profile_start_call",
    ] {
        assert!(
            profile_src.contains(needle),
            "profile.rs must register {needle}"
        );
    }
    for needle in [
        ".shared_content_tabs(",
        ".shared_content_page(",
        ".peer_last_seen_label(",
        ".mute_conversation(",
    ] {
        assert!(
            profile_src.contains(needle),
            "profile.rs must reach ChatterClient{needle} (dead wire otherwise)"
        );
    }
    let modal_src = include_str!("../../../ui/modals/user_profile.slint");
    assert!(
        modal_src.contains("root.shared-tab-selected(tab.id)")
            && modal_src.contains("root.shared-load-more()")
            && modal_src.contains("root.open-shared-item(")
            && modal_src.contains("root.start-call("),
        "user_profile.slint must fire the drawer callbacks from the tab strip and the grid"
    );
    // Full-screen on mobile must reuse ModalContainer's existing support, not
    // a second mechanism.
    assert!(
        modal_src.contains("ModalContainer {")
            && modal_src.contains("full-screen: root.mobile-mode")
            && modal_src.contains("bottom-inset: root.mobile-mode ? 0px : root.bottom-inset;")
            && modal_src.contains("padding-bottom: root.mobile-mode ? root.bottom-inset + Spacing.md : Spacing.none;"),
        "user_profile.slint must go full-screen through ModalContainer"
    );
}

// ============================================================================
// MESSAGE BACKUP (L7) — settings wires.
//
// The full encrypted round-trips are proven against the live stack by
// validated.rs V50 (local file) / V51 (server, PIN). Offline this proves the
// modal wiring at two layers, and NAMES which:
//
//  L1 (DYNAMIC, MainWindow boundary): the status property alias survives the
//     main.slint <=> app.slint hop, and the two server-mode callbacks are
//     declared, wired by the REAL wire_settings_callbacks, and safe to invoke
//     (their async half errors offline into the status property). The two
//     file-mode callbacks are NOT invoked here — their handlers open a native
//     file picker dialog, which must never pop during a test run — L2 pins
//     their wiring statically instead.
//
//  L2 (STATIC): each of the four handlers reaches its ChatterClient façade
//     method, and the settings screen fires all four callbacks.
// ============================================================================
#[test]
fn settings_message_backup_wires_reach_core() {
    let (rt, _tmp, _client, w) = wired_offline();
    let _guard = rt.enter(); // handlers tokio::spawn their async half

    // L1: status alias liveness through MainWindow.
    w.set_settings_message_backup_status("status-probe".into());
    assert_eq!(
        w.get_settings_message_backup_status().as_str(),
        "status-probe",
        "settings-message-backup-status alias is dead (main.slint <=> app.slint)"
    );

    // L1: server-mode wires run their real handlers without panicking.
    w.invoke_settings_server_message_backup("Va1idPin!x9".into());
    w.invoke_settings_restore_message_backup("Va1idPin!x9".into());

    // L2 static (Rust): every handler reaches the core façade.
    let src = include_str!("../src/bin_ui/features/modals/settings.rs");
    for (handler, core_call) in [
        ("on_settings_export_message_backup", ".export_message_backup("),
        ("on_settings_import_message_backup", ".import_message_backup("),
        ("on_settings_server_message_backup", ".enable_server_message_backup("),
        ("on_settings_restore_message_backup", ".restore_server_message_backup("),
    ] {
        let i = src
            .find(handler)
            .unwrap_or_else(|| panic!("{handler} not registered in settings.rs"));
        assert!(
            src[i..].contains(core_call),
            "{handler} must reach ChatterClient::{core_call} (dead/stub wire)"
        );
    }

    // L2 static (Slint): the settings screen fires all four callbacks.
    let screen = include_str!("../../../ui/settings/settings_screen.slint");
    for cb in [
        "root.export-message-backup(",
        "root.import-message-backup(",
        "root.server-message-backup(",
        "root.restore-message-backup(",
    ] {
        assert!(screen.contains(cb), "settings screen must fire {cb}");
    }
}

// WELCOME PAGE — first-run buttons land in main view (V49)
// ============================================================================

#[test]
fn welcome_page_buttons_are_wired() {
    let (_rt, _tmp, _client, w) = wired_offline();
    let client = _client.clone();
    chatter_ui::bin_ui::features::navigation::wire(&w, &client);

    // Registration parks the user on the welcome view; every button must exit it.
    w.set_current_view("welcome".into());
    w.invoke_welcome_get_started();
    assert_eq!(w.get_current_view().as_str(), "main", "get-started did not reach main view");

    w.set_current_view("welcome".into());
    w.invoke_welcome_add_friends();
    assert_eq!(w.get_current_view().as_str(), "main", "add-friends did not reach main view");
    assert_eq!(w.get_active_tab().as_str(), "contacts", "add-friends did not open the Contacts tab");
    assert_eq!(w.get_friends_modal_tab().as_str(), "find", "add-friends did not land on find tab");

    w.set_active_tab("chats".into());
    w.set_current_view("welcome".into());
    w.invoke_welcome_browse_channels();
    assert_eq!(w.get_current_view().as_str(), "main", "browse-channels did not reach main view");
    assert_eq!(w.get_active_modal().as_str(), "create-channel", "browse-channels did not open create-channel modal");

    // Registration must route THROUGH welcome, not skip it (static wire proof —
    // the async register flow needs a live server, so assert on source).
    let src = include_str!("../src/bin_ui/features/auth/register.rs");
    assert!(
        src.contains(r#"set_current_view("welcome""#),
        "register success no longer routes through the welcome view"
    );
}

// ============================================================================
// FRIENDS MODAL — tab clicks reach the Rust refresh hook (V50)
// ============================================================================

#[test]
fn friends_modal_tab_switch_is_wired() {
    let (_rt, _tmp, client, w) = wired_offline();
    chatter_ui::bin_ui::features::modals::wire_friends_modal(&w, &client);

    // The historical bug: FriendsModal set its local active-tab but never fired
    // switch-tab, so no Rust hook could ever run. Prove the .slint now fires it.
    let slint_src = include_str!("../../../ui/modals/friends.slint");
    assert!(
        slint_src.contains("root.switch-tab(tab.id)"),
        "FriendsModal tab click no longer fires switch-tab — Rust refresh hook is unreachable"
    );

    // And the production handler is registered + survives all tab values
    // (offline: refreshes no-op against empty stores, must not panic). The
    // handlers tokio::spawn their refreshes, so a reactor must be entered.
    let _guard = _rt.enter();
    for tab in ["find", "friends", "requests", "blocked"] {
        w.invoke_friends_modal_switch_tab(tab.into());
    }
}

// ============================================================================
// PRESENCE PICKER — the status row in the self-view profile modal
// (ui/modals/user_profile.slint STATUS section) fires `change-user-status`,
// which must reach the production handler chain
// (features/status.rs → presence_integration::set_user_status): the manual
// mode persists to core settings (survives restart) and pins the wire status.
// Offline: settings write + in-process presence state need no server.
// ============================================================================

#[test]
fn presence_status_picker_reaches_core() {
    let (rt, _tmp, client, w) = wired_offline();
    chatter_ui::init_rt_handle(rt.handle().clone());
    chatter_ui::bin_ui::features::status::wire(&w, &client);

    // Negative control: nothing persisted before the callback fires.
    assert_eq!(
        client.presence_mode().unwrap(),
        None,
        "precondition: no presence mode persisted"
    );

    // The picker's Away pill fires change-user-status("away").
    w.invoke_change_user_status("away".into());
    // Handler persists synchronously-adjacent on a spawned task; give it a beat.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let got = client.presence_mode().unwrap();
        if got.as_deref() == Some("away") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "picker did NOT reach core: presence mode never persisted (dead wire), got {got:?}"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    // Auto clears the manual override and persists as "auto".
    w.invoke_change_user_status("auto".into());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if client.presence_mode().unwrap().as_deref()
            == Some("auto")
        {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "auto mode did not persist");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    // Static half: the picker row actually invokes the callback chain.
    let modal = include_str!("../../../ui/modals/user_profile.slint");
    assert!(
        modal.contains("root.change-status(opt.mode)"),
        "STATUS pills must fire change-status"
    );
    let app = include_str!("../../../ui/app.slint");
    assert!(
        app.contains("root.change-user-status(mode)"),
        "app must forward modal change-status into change-user-status"
    );
}

// FORWARD MESSAGE — confirm handler reaches core (V51)
// ============================================================================

#[test]
fn forward_message_confirm_is_wired_to_core() {
    let (_rt, _tmp, client, w) = wired_offline();
    let _guard = _rt.enter();
    chatter_ui::bin_ui::features::modals::wire_forward_message(&w, &client);

    // The old handler closed the modal and logged "not yet implemented" —
    // regression-proof its absence.
    let src = include_str!("../src/bin_ui/features/modals/forward.rs");
    assert!(
        !src.contains("not yet implemented"),
        "forward-message confirm handler is still a no-op stub"
    );
    assert!(
        src.contains("forward_to"),
        "forward-message confirm no longer calls ConversationHandle::forward_to"
    );

    // Dynamic: populate a target + source identity, fire confirm. Offline the
    // send fails (no session), but the handler must parse, dispatch, and leave
    // the modal OPEN on failure (only success closes it).
    use slint::{ModelRc, VecModel};
    let peer = "3f2c8a90-1111-2222-3333-444455556666";
    let targets = vec![chatter_ui::ForwardTarget {
        id: peer.into(),
        name: "Test Peer".into(),
        target_type: "dm".into(),
    }];
    w.set_forward_targets(ModelRc::new(VecModel::from(targets)));
    w.set_forward_sources(ModelRc::new(VecModel::from(vec![slint::SharedString::from(
        format!("{peer}:123456789"),
    )])));
    chatter_ui::bin_ui::features::back::push(&w, "forward-message");
    w.invoke_forward_message(peer.into());
    assert_eq!(
        w.get_active_modal().as_str(),
        "forward-message",
        "modal must stay open until the async send reports success"
    );
}

// ============================================================================
// SESSIONS — revoke paths must rotate sender keys (V52)
// ============================================================================

#[test]
fn session_revoke_routes_through_sender_key_rotation() {
    // The UI's revoke buttons call client.revoke_session / revoke_all_other_sessions.
    // Historical bug: those routed to a server-only revoke that skipped sender-key
    // rotation, while the rotating path (revoke_and_rotate) was unreachable.
    let core_src = include_str!("../../chatter-core/src/client/mod.rs");
    let revoke_fn = core_src
        .split("pub async fn revoke_session")
        .nth(1)
        .expect("revoke_session facade exists");
    assert!(
        revoke_fn[..revoke_fn.find("pub async fn").unwrap_or(revoke_fn.len())]
            .contains("revoke_and_rotate"),
        "revoke_session no longer routes through revoke_and_rotate — revoked \
         devices would keep working sender keys"
    );
}

// ============================================================================
// CONNECTION BANNER DISMISSAL — the banner can be closed, the outage cannot.
//
// Dismissing is presentational only. If the dot on the sidebar's settings button
// ever stops being conditional on server-unreachable, closing the banner starts
// hiding the fact that the server is down, which is the one thing this must not
// do. STATIC, because both halves live in .slint.
// ============================================================================
#[test]
fn connection_banner_is_dismissible_without_hiding_the_outage() {
    let app = include_str!("../../../ui/app.slint");
    let banner = include_str!("../../../ui/components/connection_banner.slint");
    let sidebar = include_str!("../../../ui/sidebar/sidebar.slint");

    // The banner offers a way out...
    assert!(
        banner.contains("callback dismissed()"),
        "connection banner has no dismissed callback — nothing to close it with"
    );
    assert!(
        app.contains("dismissed => { root.connection-banner-dismissed = true; }"),
        "app does not handle the banner's dismissed callback (dead close button)"
    );
    assert!(
        app.contains("if !root.connection-banner-dismissed: ConnectionBanner"),
        "the banner is not gated on the dismissal flag, so closing it does nothing"
    );

    // ...that resets, so a later outage is not swallowed by an old dismissal.
    assert!(
        app.contains("root.connection-banner-dismissed = false;"),
        "dismissal is never cleared — one dismissal would silence every future outage"
    );

    // ...and the fact survives the dismissal.
    assert!(
        sidebar.contains("if root.server-unreachable: Rectangle"),
        "sidebar has no server-unreachable indicator, so a dismissed banner hides the outage"
    );
    assert_eq!(
        sidebar.matches("accessible-label: \"Server unreachable\"").count(),
        2,
        "both the expanded and collapsed sidebar footers must carry the dot"
    );
}

// ============================================================================
// ANDROID BACK KEY — the press must reach Rust, and must pop in-app layers
// before it is ever allowed to leave the app.
//
// The shipped defect: Back was a no-op. Nothing at the UI layer consumed it, so
// an open modal could not be closed with it, and (worse) a user in Settings —
// whose only other exit renders as a tofu box — was stranded.
// ============================================================================

/// LAYER 1: the decision itself. Back with a modal up closes the modal.
#[test]
fn back_press_closes_the_open_modal() {
    let w = boot();
    chatter_ui::bin_ui::features::back::wire(&w);

    chatter_ui::bin_ui::features::back::push(&w, "sessions");
    assert!(
        chatter_ui::bin_ui::features::back::handle_back(&w),
        "back with a modal open must report the press as consumed in-app"
    );
    assert_eq!(
        w.get_active_modal().as_str(),
        "none",
        "back did NOT close the open modal"
    );

    // Through the Slint callback too (that is what the FocusScope invokes).
    chatter_ui::bin_ui::features::back::push(&w, "create-channel");
    w.invoke_back_pressed();
    assert_eq!(
        w.get_active_modal().as_str(),
        "none",
        "the back-pressed callback is not wired to the modal-closing handler"
    );

    // With nothing open the press falls through to backgrounding the task
    // (a no-op off Android) and must not disturb modal state.
    assert!(
        !chatter_ui::bin_ui::features::back::handle_back(&w),
        "back with no modal open must fall through to backgrounding"
    );
    assert_eq!(w.get_active_modal().as_str(), "none");
}

/// Back with the message context menu open closes the menu and stays in the
/// conversation. It used to pop the conversation and leave the menu floating
/// over the conversation list.
#[test]
fn back_press_closes_the_message_context_menu_first() {
    let w = boot();
    chatter_ui::bin_ui::features::back::wire(&w);

    w.set_mobile_show_chat(true);
    w.set_context_menu_visible(true);
    assert!(chatter_ui::bin_ui::features::back::handle_back(&w));
    assert!(!w.get_context_menu_visible(), "back did not close the context menu");
    assert!(w.get_mobile_show_chat(), "back left the conversation instead of closing the menu");
}

/// Back inside a mobile conversation returns to the conversation list rather
/// than leaving the app. On a phone the list and the chat are separate screens,
/// and Back out of a chat backgrounding the whole app is the defect this pins.
#[test]
fn back_press_pops_the_mobile_conversation_pane() {
    use slint::ComponentHandle;
    let w = boot();
    // Compact (phone) layout: touch platform AND a window under 600 dp. On a
    // two-pane tablet Back has no chat pane to pop.
    w.set_is_mobile_platform(true);
    w.window().set_size(slint::LogicalSize::new(400.0, 800.0));
    chatter_ui::bin_ui::features::back::wire(&w);

    w.set_mobile_show_chat(true);
    assert!(
        chatter_ui::bin_ui::features::back::handle_back(&w),
        "back inside a conversation must be consumed in-app, not fall through"
    );
    assert!(
        !w.get_mobile_show_chat(),
        "back did NOT return to the conversation list"
    );

    // Back at the list falls through to backgrounding, as before.
    assert!(
        !chatter_ui::bin_ui::features::back::handle_back(&w),
        "back at the conversation list must fall through to backgrounding"
    );

    // A modal over a conversation pops the MODAL first — the conversation is
    // the layer underneath and must survive that press.
    w.set_mobile_show_chat(true);
    chatter_ui::bin_ui::features::back::push(&w, "forward-message");
    assert!(chatter_ui::bin_ui::features::back::handle_back(&w));
    assert_eq!(w.get_active_modal().as_str(), "none");
    assert!(
        w.get_mobile_show_chat(),
        "closing a modal must not also pop the conversation underneath it"
    );
}

/// The composer emoji picker stays open across insertions, so Back has to be
/// able to dismiss it — otherwise Back with the picker up backgrounds the app
/// (§112u).
#[test]
fn back_press_closes_the_composer_emoji_picker() {
    let w = boot();
    chatter_ui::bin_ui::features::back::wire(&w);

    w.set_composer_emoji_picker_visible(true);
    assert!(
        chatter_ui::bin_ui::features::back::handle_back(&w),
        "back with the emoji picker open must be consumed in-app"
    );
    assert!(!w.get_composer_emoji_picker_visible(), "back did NOT close the emoji picker");

    w.set_composer_gif_picker_visible(true);
    assert!(chatter_ui::bin_ui::features::back::handle_back(&w));
    assert!(!w.get_composer_gif_picker_visible(), "back did NOT close the GIF picker");
}

/// The unified search palette is an overlay, not a stacked screen, so
/// Back fell through it and backgrounded the app with the palette still up
/// (§112u).
#[test]
fn back_press_closes_the_unified_search_palette() {
    let w = boot();
    chatter_ui::bin_ui::features::back::wire(&w);

    w.set_unified_search_open(true);
    assert!(
        chatter_ui::bin_ui::features::back::handle_back(&w),
        "back with the search palette open must be consumed in-app"
    );
    assert!(!w.get_unified_search_open(), "back did NOT close the palette");
}

/// Back while a call rang popped the chat UNDER the full-screen ring (FX3).
/// The ring is drawn over everything, so Back is its: it hides the ring
/// (Signal) and the call keeps ringing — `incoming-call-visible` is what the
/// call code reads, so it must stay set — with nothing underneath popped.
#[test]
fn back_while_ringing_hides_the_ring_and_pops_nothing_under_it() {
    use slint::ComponentHandle;
    let w = boot();
    chatter_ui::bin_ui::features::back::wire(&w);
    w.set_is_mobile_platform(true);
    w.window().set_size(slint::LogicalSize::new(400.0, 800.0));
    assert!(w.get_compact_layout(), "precondition: a phone-width window");
    w.set_mobile_show_chat(true);
    w.set_incoming_call_visible(true);

    assert!(chatter_ui::bin_ui::features::back::handle_back(&w), "Back while ringing must be consumed");
    assert!(w.get_incoming_call_minimized(), "Back must hide the ring");
    assert!(w.get_incoming_call_visible(), "the call must keep ringing");
    assert!(w.get_mobile_show_chat(), "Back while ringing popped the chat underneath");

    // With the ring hidden, the next Back is the chat's again.
    assert!(chatter_ui::bin_ui::features::back::handle_back(&w));
    assert!(!w.get_mobile_show_chat());
}

/// The palette's query field takes focus on open; picking a result closes the
/// palette and destroys that field, leaving NOTHING focused — so the next Back
/// key reached no FocusScope and did nothing (RC20: search → open chat → Back
/// dead, 3/3). The real key event must still reach `back-pressed`.
#[test]
fn back_key_still_arrives_after_the_search_palette_closes() {
    use slint::ComponentHandle;
    let w = boot();
    chatter_ui::bin_ui::features::back::wire(&w);
    let scale = w.window().scale_factor();
    w.window().dispatch_event(slint::platform::WindowEvent::Resized {
        size: slint::LogicalSize::new(1000.0, 800.0).to_physical(scale).to_logical(scale),
    });

    w.set_unified_search_open(true);
    // A redraw instantiates the palette, whose field focuses itself on init.
    w.window().request_redraw();
    w.window().dispatch_event(slint::platform::WindowEvent::Resized {
        size: slint::LogicalSize::new(1000.0, 801.0).to_physical(scale).to_logical(scale),
    });
    slint::platform::update_timers_and_animations();
    w.window().dispatch_event(slint::platform::WindowEvent::KeyPressed { text: "r".into() });
    assert_eq!(
        w.get_unified_search_query().as_str(),
        "r",
        "precondition: the palette's field must hold focus, or this test proves nothing"
    );
    // A result was picked: the palette closes itself (app.slint's
    // `conversation-selected` handler).
    w.set_unified_search_open(false);
    slint::platform::update_timers_and_animations();

    // Any Back-consuming layer will do as the witness; selection mode needs
    // no core.
    w.set_selection_mode(true);
    w.window().dispatch_event(slint::platform::WindowEvent::KeyPressed {
        text: slint::platform::Key::Back.into(),
    });
    assert!(
        !w.get_selection_mode(),
        "Back after closing the search palette never reached back-pressed — the \
         palette's field took focus with it"
    );
}

/// On a phone the composer drops its focus when the emoji/GIF picker opens, so
/// the panel replaces the keyboard. That left NOTHING focused, and the Back
/// meant to close the picker went nowhere (S9 + S25). Opening a picker on a
/// phone must hand focus to back-scope instead. The palette's field stands in
/// for the composer as the focused text field.
#[test]
fn opening_a_composer_picker_on_a_phone_keeps_back_alive() {
    use slint::ComponentHandle;
    let w = boot();
    chatter_ui::bin_ui::features::back::wire(&w);
    w.set_is_mobile_platform(true);
    let scale = w.window().scale_factor();
    let resize = |h: f32| {
        w.window().dispatch_event(slint::platform::WindowEvent::Resized {
            size: slint::LogicalSize::new(1000.0, h).to_physical(scale).to_logical(scale),
        });
    };
    resize(800.0);

    w.set_unified_search_open(true);
    w.window().request_redraw();
    resize(801.0);
    slint::platform::update_timers_and_animations();
    w.window().dispatch_event(slint::platform::WindowEvent::KeyPressed { text: "r".into() });
    assert_eq!(
        w.get_unified_search_query().as_str(),
        "r",
        "precondition: a text field must hold focus, or this test proves nothing"
    );

    for gif in [false, true] {
        if gif {
            w.set_composer_gif_picker_visible(true);
        } else {
            w.set_composer_emoji_picker_visible(true);
        }
        slint::platform::update_timers_and_animations();
        w.window().dispatch_event(slint::platform::WindowEvent::KeyPressed { text: "x".into() });
        assert_eq!(
            w.get_unified_search_query().as_str(),
            "r",
            "the text field kept focus while a picker opened on a phone (gif={gif})"
        );
        w.window().dispatch_event(slint::platform::WindowEvent::KeyPressed {
            text: slint::platform::Key::Back.into(),
        });
        assert!(
            !w.get_composer_emoji_picker_visible() && !w.get_composer_gif_picker_visible(),
            "a real Back did not close the picker (gif={gif})"
        );
    }
}

/// New Channel on a phone: the keyboard could not be put away, covering
/// "Who can join?". The IME's Done (delivered as Return) on the name moves to
/// Description, Done there drops focus, and `release-text-focus` (what
/// Android's Back calls after hiding the keyboard) drops it too — each without
/// stranding the Back key.
#[test]
fn new_channel_done_and_back_put_the_keyboard_away() {
    use slint::ComponentHandle;
    require_element_queries();
    let w = boot();
    chatter_ui::bin_ui::features::back::wire(&w);
    w.set_is_mobile_platform(true);
    let scale = w.window().scale_factor();
    w.window().dispatch_event(slint::platform::WindowEvent::Resized {
        size: slint::LogicalSize::new(400.0, 800.0).to_physical(scale).to_logical(scale),
    });
    chatter_ui::bin_ui::features::back::push(&w, "create-channel");
    w.window().request_redraw();
    // Let the card's entrance animation settle so clicks land on the fields.
    testing::mock_elapsed_time(std::time::Duration::from_millis(1000));

    let key = |t: slint::SharedString| {
        w.window().dispatch_event(slint::platform::WindowEvent::KeyPressed { text: t.clone() });
        w.window().dispatch_event(slint::platform::WindowEvent::KeyReleased { text: t });
    };
    let ret: slint::SharedString = slint::platform::Key::Return.into();

    let name = testing::ElementHandle::find_by_accessible_label(&w, "Channel name")
        .next()
        .expect("the New Channel name field is not on screen");
    name.mock_single_click(slint::platform::PointerEventButton::Left);
    key("a".into());
    assert_eq!(w.get_create_channel_name().as_str(), "a", "precondition: the name field must take typing");

    key(ret.clone());
    key("d".into());
    assert_eq!(w.get_create_channel_name().as_str(), "a", "Done on the name did not leave the name field");
    assert_eq!(
        w.get_create_channel_description().as_str(),
        "d",
        "Done on the name did not move focus to Description"
    );

    key(ret.clone());
    key("z".into());
    assert_eq!(
        w.get_create_channel_description().as_str(),
        "d",
        "Done on Description did not drop focus — the keyboard stays up"
    );

    name.mock_single_click(slint::platform::PointerEventButton::Left);
    key("b".into());
    assert_eq!(w.get_create_channel_name().as_str(), "ab");
    w.invoke_release_text_focus();
    key("c".into());
    assert_eq!(
        w.get_create_channel_name().as_str(),
        "ab",
        "release-text-focus left the name field focused (Back hid the keyboard only on screen)"
    );

    w.window().dispatch_event(slint::platform::WindowEvent::KeyPressed {
        text: slint::platform::Key::Back.into(),
    });
    assert_eq!(w.get_active_modal().as_str(), "none", "Back stopped arriving after focus was released");
}

/// Back inside a thread returns to the conversation, not to the chat list.
/// The thread panel covers the chat pane on a phone, so skipping this layer
/// jumped two levels up (§112s).
#[test]
fn back_press_pops_the_thread_panel_before_the_conversation() {
    let w = boot();
    chatter_ui::bin_ui::features::back::wire(&w);

    let closed = std::rc::Rc::new(std::cell::Cell::new(false));
    let flag = closed.clone();
    w.on_close_thread_clicked(move || flag.set(true));

    w.set_mobile_show_chat(true);
    w.set_thread_panel_visible(true);

    assert!(
        chatter_ui::bin_ui::features::back::handle_back(&w),
        "back inside a thread must be consumed in-app"
    );
    assert!(closed.get(), "back did NOT close the thread panel");
    assert!(
        w.get_mobile_show_chat(),
        "closing a thread must not also pop the conversation underneath it"
    );
}


/// LAYER 2: the KEY EVENT actually reaches that decision.
///
/// This is the half the bug report was about — the backend already maps
/// `Keycode::Back` to `Key::Back` (vendor/i-slint-backend-android-activity/
/// androidwindowadapter.rs), but `WindowInner::process_key_input` only delivers
/// a key to the focus item and its ANCESTORS. With no focus item and no
/// top-level FocusScope, Back went nowhere. Dispatching the real event proves
/// `forward-focus: back-scope` + the wrapping FocusScope closed that gap.
#[test]
fn back_key_event_reaches_the_handler() {
    use slint::ComponentHandle;
    let w = boot();
    chatter_ui::bin_ui::features::back::wire(&w);
    chatter_ui::bin_ui::features::back::push(&w, "sessions");

    // Give the window a real geometry first. `process_key_input` drops the
    // focus item when `ItemRc::is_visible()` is false, and an unshown headless
    // window has a zero-size root — the key would be discarded before it ever
    // reached back-scope, for a reason that has nothing to do with the wire.
    let scale = w.window().scale_factor();
    w.window().dispatch_event(slint::platform::WindowEvent::Resized {
        size: slint::LogicalSize::new(1000.0, 800.0).to_physical(scale).to_logical(scale),
    });

    w.window().dispatch_event(slint::platform::WindowEvent::KeyPressed {
        text: slint::platform::Key::Back.into(),
    });

    assert_eq!(
        w.get_active_modal().as_str(),
        "none",
        "a real Key::Back window event never reached back-pressed — the top-level \
         FocusScope is not receiving key events (check `forward-focus: back-scope`)"
    );
}

/// The Back scope must not become a focus thief. It is an ancestor of the whole
/// app including the message composer, so if it grabbed focus on click or on Tab
/// the composer would lose the caret mid-typing. Both are switched off, and Back
/// still works because unhandled keys bubble UP the focus chain rather than
/// requiring the scope to hold focus itself.
#[test]
fn back_scope_does_not_steal_focus_from_text_input() {
    let main = include_str!("../../../ui/main.slint");
    assert!(
        main.contains("back-scope := FocusScope"),
        "the top-level back FocusScope is gone; Back cannot be observed"
    );
    assert!(
        main.contains("forward-focus: back-scope;"),
        "without forward-focus nothing holds focus at startup and Back is dropped"
    );
    assert!(
        main.contains("focus-on-click: false;"),
        "back-scope would steal focus from the composer on every stray tap"
    );
    assert!(
        main.contains("focus-on-tab-navigation: false;"),
        "back-scope would swallow a Tab stop over the whole window"
    );
    // Only Back is consumed; every other key is rejected so it keeps bubbling.
    assert!(
        main.contains("if (event.text == Key.Back) {") && main.contains("return reject;"),
        "back-scope must consume ONLY Key.Back and reject everything else, or it \
         would eat text input"
    );
    // The key-UP must be swallowed too. Android's default Back fires off
    // ACTION_UP, so consuming only the press would let one physical press both
    // close a modal AND finish the Activity.
    assert!(
        main.contains("key-released(event) => {")
            && main.contains("event.text == Key.Back ? accept : reject"),
        "back-scope does not consume the Back key-up; Android's default Back \
         handler can still fire behind us"
    );
}

/// Typing is unaffected: a plain character dispatched at the window is not
/// consumed by back-scope and does not fire the back handler.
#[test]
fn back_scope_ignores_ordinary_typing() {
    use slint::ComponentHandle;
    let w = boot();
    chatter_ui::bin_ui::features::back::wire(&w);
    chatter_ui::bin_ui::features::back::push(&w, "sessions");

    let scale = w.window().scale_factor();
    w.window().dispatch_event(slint::platform::WindowEvent::Resized {
        size: slint::LogicalSize::new(1000.0, 800.0).to_physical(scale).to_logical(scale),
    });

    for text in ["a", "Z", " "] {
        w.window().dispatch_event(slint::platform::WindowEvent::KeyPressed {
            text: text.into(),
        });
    }

    assert_eq!(
        w.get_active_modal().as_str(),
        "sessions",
        "an ordinary key press was treated as Back — back-scope is over-matching"
    );
}

/// ORDERING: selection mode is the innermost layer and must pop FIRST.
///
/// The regression this guards: one Back press while multi-selecting inside an
/// open modal jumping straight past the selection to close the modal (or worse,
/// to background the app), losing the user's selection AND their place.
#[test]
fn back_pops_selection_mode_before_the_modal() {
    let w = boot();
    chatter_ui::bin_ui::features::back::wire(&w);

    w.set_selection_mode(true);
    chatter_ui::bin_ui::features::back::push(&w, "sessions");

    w.invoke_back_pressed();

    assert!(
        !w.get_selection_mode(),
        "back did not exit message multi-select — it is the innermost layer and \
         must pop first"
    );
    assert_eq!(
        w.get_active_modal().as_str(),
        "sessions",
        "back popped TWO layers at once: selection mode AND the modal. One press, \
         one layer"
    );

    // Second press now takes the modal.
    w.invoke_back_pressed();
    assert_eq!(w.get_active_modal().as_str(), "none", "the second back press did not close the modal");
}

/// UX1 (R4 P0-1): the current tab and the screens pushed over it are separate
/// state. With one `active-modal` slot for both, Back always landed on Chats,
/// Sessions/Link device replaced Settings instead of stacking on it, and
/// someone else's profile lit the Profile tab.
#[test]
fn back_pops_the_modal_stack_before_leaving_the_tab() {
    use chatter_ui::bin_ui::features::back;
    let w = boot();
    back::wire(&w);
    let tab = |w: &MainWindow| w.get_active_tab().to_string();
    let top = |w: &MainWindow| w.get_active_modal().to_string();

    // Profile menu › Settings › Sessions › Link device, pushed through the
    // Slint callback. Settings is pushed, not a tab (UX3).
    w.set_active_tab("profile".into());
    w.invoke_nav_push("settings".into());
    assert!(w.get_settings_open(), "pushing Settings did not raise settings-open");
    w.invoke_nav_push("sessions".into());
    w.invoke_nav_push("link-device-approve".into());
    assert_eq!(top(&w), "link-device-approve");
    assert!(back::handle_back(&w));
    assert_eq!(top(&w), "sessions", "Back from Link device must return to Sessions");
    assert!(w.get_settings_open(), "Settings must stay mounted under Sessions");
    assert!(back::handle_back(&w));
    assert_eq!(top(&w), "settings", "Back from Sessions must return to Settings, not Chats");
    assert!(back::handle_back(&w));
    assert_eq!((tab(&w), top(&w)), ("profile".into(), "none".into()), "Back from Settings must return to the account menu");
    assert!(!w.get_settings_open(), "popping Settings left settings-open set");
    assert!(back::handle_back(&w), "Back on a non-Chats tab must be consumed");
    assert_eq!(tab(&w), "chats", "Back from a root tab must go to Chats");
    assert!(!back::handle_back(&w), "Back on Chats with nothing open backgrounds the app");

    // A profile opened from Contacts is pushed over Contacts, never the Profile tab.
    w.set_active_tab("contacts".into());
    back::push(&w, "user-profile");
    assert_eq!(tab(&w), "contacts", "opening a profile changed the tab");
    assert!(back::handle_back(&w));
    assert_eq!((tab(&w), top(&w)), ("contacts".into(), "none".into()), "Back from a profile must land on Contacts");

    // close() takes what the closed screen opened with it; closing a screen
    // that is not open (a late async completion) changes nothing.
    back::push(&w, "sessions");
    back::push(&w, "link-device-approve");
    back::close(&w, "forward-message");
    assert_eq!(top(&w), "link-device-approve");
    w.invoke_nav_close("sessions".into());
    assert_eq!(top(&w), "none");

    // Re-pushing an open screen moves it to the top instead of stacking a copy.
    back::push(&w, "sessions");
    back::push(&w, "jarvis-memory");
    back::push(&w, "sessions");
    assert_eq!(slint::Model::row_count(&w.get_modal_stack()), 2);
    w.invoke_nav_pop();
    assert_eq!(top(&w), "jarvis-memory");

    // Opening a conversation / logout: nothing stays over the Chats tab.
    back::reset_to_chats(&w);
    assert_eq!((tab(&w), top(&w)), ("chats".into(), "none".into()));
}

/// The bottom tab bar and the return-to-call bar are layered UNDER every
/// pushed modal (z -1, with the chat column at -2, the Chats FAB at -0.5
/// between the dock and the modals), so a dialog's backdrop
/// dims them and takes their taps; the tab screens sit at the same layer,
/// declared before the bar, so they never cover it. And the bar highlights
/// `active-tab`, not something derived from whichever modal is open.
#[test]
fn tab_bar_and_call_bar_sit_under_pushed_modals() {
    let app = include_str!("../../../ui/app.slint");
    for header in [
        "if root.bottom-nav-visible: MobileBottomNav {",
        "|| (root.compact-layout && !root.mobile-show-chat)): ReturnToCallBar {",
        r#"if root.active-tab == "calls": CallHistoryScreen {"#,
        r#"if root.active-tab == "contacts": FriendsModal {"#,
        r#"if root.active-tab == "profile": AccountMenu {"#,
        r#"if root.settings-open: SettingsScreen {"#,
        "if root.fab-visible: GlassSurface {",
        r#"if current-view == "main": VerticalLayout {"#,
    ] {
        let at = app.find(header).unwrap_or_else(|| panic!("app.slint lost `{header}`"));
        let body = &app[at..at + 400];
        assert!(
            body.contains("z: -1;") || body.contains("z: -2;") || body.contains("z: -0.5;"),
            "`{header}` is no longer layered under the modals"
        );
    }
    let bar = app.find("if root.bottom-nav-visible: MobileBottomNav {").unwrap();
    assert!(app[bar..bar + 300].contains("active-tab: root.active-tab;"));
    for screen in [
        r#"if root.active-tab == "calls": CallHistoryScreen {"#,
        r#"if root.active-tab == "contacts": FriendsModal {"#,
        r#"if root.active-tab == "profile": AccountMenu {"#,
        r#"if root.settings-open: SettingsScreen {"#,
    ] {
        assert!(
            app.find(screen).unwrap() < bar,
            "{screen} is declared after the tab bar, so it would cover it"
        );
    }
}

// ============================================================================
// Message multi-select — the SELECTION wires reach the selection store.
//
// The hold gesture's tier 2 and the menu's "Select" item both land here, so
// this is the wire that has to hold: without it the bubbles mark themselves and
// nothing else in the app ever learns a selection exists.
//
//  L1 (DYNAMIC): toggling a message runs the real handler, sets selection mode,
//    publishes the count, and marks the row in the model.
//  L2 (STATIC, Slint): the bubble's hold timers fire selection-toggled, the
//    menu carries a Select item, and a tap in selection mode toggles rather
//    than opening the message.
// ============================================================================
#[test]
fn message_selection_toggle_reaches_the_selection_store() {
    use slint::Model;
    no_keyring();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    let tmp = tempfile::tempdir().unwrap();
    testing::init_no_event_loop();
    let client = chatter_core::client::ChatterClient::builder()
        .data_dir(tmp.path().to_string_lossy().to_string())
        .build()
        .expect("offline ChatterClient::build");
    let w = MainWindow::new().expect("MainWindow::new");
    chatter_ui::bin_ui::features::chat::wire(&w, &client);

    // Two rows to select between, so "marks the right one" is a real assertion.
    let mut a = chatter_ui::MessageItemData::default();
    a.message_id = "msg-a".into();
    let mut b = chatter_ui::MessageItemData::default();
    b.message_id = "msg-b".into();
    w.set_messages(slint::ModelRc::new(slint::VecModel::from(vec![a, b])));

    // L1 dynamic: selecting one message turns the mode on and marks that row.
    w.invoke_message_selection_toggled("msg-a".into());
    assert!(w.get_selection_mode(), "toggling a message must enter selection mode");
    assert_eq!(w.get_selected_message_count(), 1, "the count must reach the selection bar");
    let rows = w.get_messages();
    assert!(rows.row_data(0).unwrap().selected, "the selected row must be marked");
    assert!(!rows.row_data(1).unwrap().selected, "an unselected row must NOT be marked");

    // Deselecting the last one leaves the mode entirely — a selection bar
    // reading "0 selected" with a dead delete button is a dead end.
    w.invoke_message_selection_toggled("msg-a".into());
    assert!(!w.get_selection_mode(), "deselecting the last message must exit selection mode");
    assert_eq!(w.get_selected_message_count(), 0);
    assert!(!w.get_messages().row_data(0).unwrap().selected, "the mark must be cleared");

    // L2 static (Slint): the hold gesture is what fires this on a touch screen.
    let item_src = include_str!("../../../ui/chat/message_item.slint");
    assert!(
        item_src.contains("tier1-timer"),
        "the press-and-hold timer must exist on the bubble — right-click is \
         unreachable from a finger (the Android backend only makes \
         PointerEventButton::Right from a real secondary mouse button)"
    );
    assert!(
        item_src.contains("root.selection-toggled(root.message-id)"),
        "tier 2 of the hold, and a tap while in selection mode, must fire \
         selection-toggled with the message id"
    );

    // L2 static (Slint): the discoverable route, for anyone who never finds the
    // longer hold.
    let menu_src = include_str!("../../../ui/chat/message_context_menu.slint");
    assert!(
        menu_src.contains(r#"label: "Select""#) && menu_src.contains("root.select-clicked(root.message-id)"),
        "the message menu must carry a Select item wired to select-clicked"
    );
}

/// The floating reply/react/thread/more bar over a message bubble is
/// POINTER-only.
///
/// `has-hover` is true while a finger is DOWN — Slint raises it from any
/// pointer event carrying a position — so on Android the bar blinked into
/// existence mid-press and vanished on release, including during a plain
/// scroll. It was never aimable, and press-and-hold already opens the same
/// actions as a real menu.
#[test]
fn the_message_hover_action_bar_never_appears_on_touch() {
    let item_src = include_str!("../../../ui/chat/message_item.slint");
    assert!(
        item_src.contains("property <bool> is-hovered: !root.mobile && touch.has-hover;"),
        "the message action bar's hover gate must exclude touch platforms"
    );
    assert!(
        item_src.contains("visible: root.is-hovered"),
        "the action bar is what that gate is for — if it stops reading \
         is-hovered, the gate is guarding nothing"
    );

    // A gate on `mobile` is only worth anything if `mobile` reaches the row.
    let list = include_str!("../../../ui/chat/message_list.slint");
    assert!(
        list.contains("mobile: root.mobile;"),
        "message_list must pass the touch flag down to each MessageItem"
    );
    let layout = include_str!("../../../ui/chat/chat_layout.slint");
    assert!(
        layout.contains("mobile: root.mobile;"),
        "chat_layout must pass the touch flag down to the message list"
    );
}

// ============================================================================
// FOLDERS (custom sections). Everything below was either dead or silently
// broken before: `pin_conversation` existed in core with no UI path at all,
// `reorder_sections` had zero callers, folders sorted by filing order forever,
// a foldered channel rendered in BOTH the folder and the Channels list, and
// the folder badges were frozen from app launch.
// ============================================================================

/// A pinned conversation had no route from the UI to core: `pin_conversation`
/// (services/conversation.rs) existed with no Slint callback and no handler.
///
///  L1 (DYNAMIC): the callback exists on MainWindow and the real handler runs.
///  L2 (STATIC, Rust): the handler reaches `set_conversation_pinned` on core.
///  L2 (STATIC, Slint): the conversation menu carries the entry that fires it,
///    and the three lists that render pin order pass the row's flag up so the
///    menu can say Pin or Unpin.
#[test]
fn pin_conversation_reaches_core_from_the_sidebar_menu() {
    no_keyring();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    let tmp = tempfile::tempdir().unwrap();
    testing::init_no_event_loop();
    let client = chatter_core::client::ChatterClient::builder()
        .data_dir(tmp.path().to_string_lossy().to_string())
        .build()
        .expect("offline ChatterClient::build");
    let w = MainWindow::new().expect("MainWindow::new");
    chatter_ui::bin_ui::features::sidebar::wire(&w, &client);

    // Must not panic: the persist runs on tokio, which this harness does not pump.
    w.invoke_set_conversation_pinned("00000000-0000-0000-0000-000000000001".into(), true);

    // L2 static (Rust): the handler is not a Slint-only bool.
    let src = include_str!("../src/bin_ui/features/sidebar/sections.rs");
    let idx = src.find("on_set_conversation_pinned").expect("pin handler present");
    assert!(
        src[idx..].contains(".set_conversation_pinned("),
        "the pin entry must call ChatterClient::set_conversation_pinned"
    );

    // L2 static (Slint): the menu item, and the flag the label depends on.
    let menu = include_str!("../../../ui/sidebar/section_context_menu.slint");
    assert!(
        menu.contains("root.set-pinned(root.conversation-id, !root.is-pinned)"),
        "the conversation menu must carry a Pin/Unpin entry firing set-pinned"
    );
    assert!(
        menu.contains(r#"label: root.is-pinned ? "Unpin" : "Pin to top""#),
        "the entry must name the action it will actually perform"
    );

    // Every chat-list row hands itself to the menu, pin state included.
    let list = include_str!("../../../ui/sidebar/chat_list.slint");
    assert!(
        list.contains("root.row-context-menu-requested(row, x, y)"),
        "the chat-list row must carry itself into the menu"
    );
    let sidebar = include_str!("../../../ui/sidebar/sidebar.slint");
    assert!(
        sidebar.contains("root.ctx-menu-is-pinned = row.is-pinned;"),
        "the menu must read the row's pin state"
    );
}

/// Pin state is settings-backed and local-only. This proves the round trip
/// through core (no UI involved) and that unpinning clears the set rather than
/// leaving a `false` behind.
#[test]
fn pinning_round_trips_through_core_and_is_local_only() {
    no_keyring();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    let tmp = tempfile::tempdir().unwrap();
    let client = chatter_core::client::ChatterClient::builder()
        .data_dir(tmp.path().to_string_lossy().to_string())
        .build()
        .expect("offline ChatterClient::build");

    let id = "11111111-1111-1111-1111-111111111111";
    assert!(!client.is_conversation_pinned(id), "nothing is pinned by default");

    assert!(client.set_conversation_pinned(id, true));
    assert!(client.is_conversation_pinned(id), "the pin must survive the write");
    assert!(
        client.pinned_conversation_ids().contains(id),
        "the bulk query the lists use must see the same pin as the single lookup"
    );

    assert!(client.set_conversation_pinned(id, false));
    assert!(!client.is_conversation_pinned(id));
    assert!(
        !client.pinned_conversation_ids().contains(id),
        "unpinning must remove the id from the set, not store a false"
    );
}

/// `reorder_sections` (section_service.rs) was fully implemented with ZERO
/// callers — the back half of a drag-to-reorder nobody finished.
///
/// Deliberately NOT drag-and-drop: there is no DropArea anywhere in `ui/`, and
/// dragging a row inside a scrolling list is the worst gesture to debut on a
/// phone. Move up / move down rides the press-and-hold menu that already exists.
#[test]
fn folder_reorder_is_wired_and_reaches_reorder_sections() {
    no_keyring();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    let tmp = tempfile::tempdir().unwrap();
    testing::init_no_event_loop();
    let client = chatter_core::client::ChatterClient::builder()
        .data_dir(tmp.path().to_string_lossy().to_string())
        .build()
        .expect("offline ChatterClient::build");
    let w = MainWindow::new().expect("MainWindow::new");
    chatter_ui::bin_ui::features::sidebar::wire(&w, &client);

    w.invoke_move_section("00000000-0000-0000-0000-000000000001".into(), -1);

    let src = include_str!("../src/bin_ui/features/sidebar/sections.rs");
    let idx = src.find("on_move_section").expect("move-section handler present");
    assert!(
        src[idx..].contains(".move_section("),
        "the folder menu's Move up/down must reach ChatterClient::move_section"
    );

    // The core method is what finally calls the orphaned reorder.
    let core = include_str!("../../chatter-core/src/services/section_service.rs");
    let core_idx = core.find("pub fn move_section").expect("move_section in core");
    assert!(
        core[core_idx..].contains("self.reorder_sections(&ids)"),
        "move_section must be the caller reorder_sections never had"
    );

    let menu = include_str!("../../../ui/sidebar/section_context_menu.slint");
    assert!(
        menu.contains("root.move-section(root.section-id, -1)")
            && menu.contains("root.move-section(root.section-id, 1)"),
        "the folder header menu must offer both directions"
    );
}

/// Folders sorted by `added_at ASC` forever, so filing a channel into one
/// silently swapped it from MRU to filing order. Order is now a per-folder
/// choice, defaulting to Recent so a folder matches the list it came from.
#[test]
fn folder_sort_mode_reaches_core_and_defaults_to_recent() {
    no_keyring();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    let tmp = tempfile::tempdir().unwrap();
    testing::init_no_event_loop();
    let client = chatter_core::client::ChatterClient::builder()
        .data_dir(tmp.path().to_string_lossy().to_string())
        .build()
        .expect("offline ChatterClient::build");
    let w = MainWindow::new().expect("MainWindow::new");
    chatter_ui::bin_ui::features::sidebar::wire(&w, &client);

    // L1 dynamic through core: a new folder is Recent, and the mode persists.
    let section = client.create_section("Work", "\u{1F4C1}").expect("create_section");
    assert_eq!(
        section.sort_mode,
        chatter_core::domain::section::SectionSortMode::Recent,
        "a new folder must sort like the rest of the sidebar, not by filing order"
    );

    client
        .set_section_sort_mode(&section.id, chatter_core::domain::section::SectionSortMode::Alphabetical)
        .expect("set_section_sort_mode");
    let reloaded = client.list_sections_with_conversations().expect("list sections");
    assert_eq!(
        reloaded[0].sort_mode,
        chatter_core::domain::section::SectionSortMode::Alphabetical,
        "the sort mode must survive a reload (migration v062 column)"
    );

    // L1 dynamic through the UI callback.
    w.invoke_set_section_sort_mode(section.id.to_string().into(), "unread".into());

    let src = include_str!("../src/bin_ui/features/sidebar/sections.rs");
    let idx = src.find("on_set_section_sort_mode").expect("sort-mode handler present");
    assert!(
        src[idx..].contains(".set_section_sort_mode("),
        "the sort picker must reach ChatterClient::set_section_sort_mode"
    );

    let menu = include_str!("../../../ui/sidebar/section_context_menu.slint");
    for mode in ["recent", "unread", "alphabetical"] {
        assert!(
            menu.contains(&format!(r#"root.set-sort-mode(root.section-id, "{mode}")"#)),
            "the folder menu must offer the {mode} sort"
        );
    }
}

/// A channel filed into a folder used to render in BOTH the folder and the
/// Channels list: `sidebar_channels` never excluded sectioned conversations,
/// and `all_sectioned_conversation_ids` / `is_in_section` — written for exactly
/// this — had zero callers.
#[test]
fn foldered_conversations_are_excluded_from_the_loose_lists() {
    let core = include_str!("../../chatter-core/src/client/mod.rs");
    let idx = core.find("pub fn sidebar_channels").expect("sidebar_channels present");
    let body = &core[idx..idx + 2000];
    assert!(
        body.contains("all_sectioned_conversation_ids"),
        "sidebar_channels must drop conversations that live in a folder"
    );
    assert!(
        body.contains("sort_pinned_first"),
        "sidebar_channels must put pinned channels first — ordering is core's job"
    );

    let messaging = include_str!("../../chatter-core/src/client/messaging.rs");
    let dm_idx = messaging.find("fn finish_dm_sidebar").expect("finish_dm_sidebar present");
    let dm_body = &messaging[dm_idx..dm_idx + 1200];
    assert!(
        dm_body.contains("all_sectioned_conversation_ids") && dm_body.contains("is_pinned"),
        "the DM sidebar must apply the same folder exclusion and pin order"
    );
}

/// `load_sections` ran once at startup and `refresh_sections_ui` only fired
/// after a folder MUTATION, so both the per-conversation badge and the header
/// aggregate were frozen from app launch. The aggregate was also an N+1 (one
/// `get_conversation` per membership), which is why it could not be refreshed
/// on a hot path — that is now a single unified-conversation query.
#[test]
fn folder_badges_refresh_on_message_arrival_and_on_read() {
    let sidebar = include_str!("../src/bin_ui/features/sidebar/mod.rs");
    let idx = sidebar
        .find("pub(crate) fn update_sidebar_preview")
        .expect("update_sidebar_preview present");
    assert!(
        sidebar[idx..].contains("refresh_section_badges(window, ui_ctx)"),
        "every sent and received message funnels through update_sidebar_preview; \
         the folder badges must be rebuilt there or they stay frozen from launch"
    );

    let channel = include_str!("../src/bin_ui/features/sidebar/channel.rs");
    let read_idx = channel.find("mark_as_read()").expect("mark_as_read present");
    assert!(
        channel[read_idx..].contains("refresh_section_badges"),
        "reading a foldered channel must clear its folder badge too"
    );

    // The DM path had no mark-as-read at all: the badge is rebuilt from
    // `messages.is_read`, so opening a DM cleared the Slint model and nothing
    // else — the count came straight back on the next refresh and after every
    // restart.
    let dm = include_str!("../src/bin_ui/features/sidebar/dm.rs");
    let dm_read_idx = dm.find("mark_as_read()").expect(
        "selecting a DM must mark it read in core, not just zero the sidebar model",
    );
    assert!(
        dm[dm_read_idx..].contains("refresh_section_badges"),
        "reading a foldered DM must clear its folder badge too"
    );

    // The N+1 that made this impossible is gone.
    let core = include_str!("../../chatter-core/src/services/section_service.rs");
    let list_idx = core
        .find("pub fn list_sections_with_conversations")
        .expect("list_sections_with_conversations present");
    let body = &core[list_idx..list_idx + 2500];
    assert!(
        body.contains("list_all()"),
        "folder metadata must come from ONE unified-conversation query, not a \
         get_conversation per membership"
    );
    assert!(
        !body.contains("self.conversation_repo.get_conversation("),
        "the per-membership get_conversation N+1 must not come back"
    );
}

/// Each section looped the WHOLE flat conversation list and hid non-matches
/// with `visible:` + `height: 0px` — ten folders over two hundred conversations
/// instantiated two thousand elements. Rows are now pre-grouped in Rust so a
/// section's model holds only its own.
#[test]
fn folder_rows_are_pre_grouped_per_section() {
    let list = include_str!("../../../ui/sidebar/custom_section_list.slint");
    assert!(
        list.contains("conversations: [ChatRowData]"),
        "each section must own its rows"
    );
    assert!(
        list.contains("for conv in section.conversations:"),
        "a section must loop only its own rows"
    );
    assert!(
        !list.contains("conv.section-id == section.section-id"),
        "the visible/height:0 filter over the whole flat list must be gone"
    );

    // Rust must be the thing that groups them.
    let src = include_str!("../src/bin_ui/features/sidebar/sections.rs");
    assert!(
        src.contains("conversations: ModelRc::new(VecModel::from(rows))"),
        "the per-section model must be built in Rust"
    );
}

/// `CustomSectionList` never received `mobile-mode`, so folder headers stayed
/// 32px and rows 36px against a 48px touch minimum, and 24px hover buttons sat
/// on a surface that has no hover.
#[test]
fn folder_ui_sizes_up_for_touch() {
    let sidebar = include_str!("../../../ui/sidebar/sidebar.slint");
    let idx = sidebar.find("CustomSectionList {").expect("CustomSectionList instantiated");
    assert!(
        // `touch`, not `mobile-mode`: a two-pane tablet is not in mobile
        // layout but is still driven by a finger (595b5031).
        sidebar[idx..idx + 600].contains("mobile-mode: root.touch"),
        "the folder list must be told whether it is being touched"
    );

    let list = include_str!("../../../ui/sidebar/custom_section_list.slint");
    // Folder rows are the chat list's own rows, sized by the same flag.
    let rows = &list[list.find("for conv in section.conversations: ChatRow {").expect("folder rows are ChatRows")..];
    assert!(
        rows[..200].contains("mobile-mode: root.mobile-mode;"),
        "folder conversation rows must meet the touch minimum"
    );
    assert!(
        list.contains("height: root.mobile-mode ? Spacing.touch-target-min : Spacing.button-height"),
        "folder headers must meet the touch minimum"
    );
    assert!(
        list.contains("if !root.mobile-mode && header-hover.has-hover:"),
        "the hover-reveal rename/delete buttons are desktop-only — hold opens \
         a menu carrying both, and a 24px button is unhittable by a finger"
    );

    // Which means the touch route to rename must exist in that menu.
    let menu = include_str!("../../../ui/sidebar/section_context_menu.slint");
    assert!(
        menu.contains("root.rename-section(root.section-id)"),
        "with the hover buttons desktop-only, Rename must live in the hold menu"
    );
}

/// FOUND ON DEVICE: press-and-hold on a folder header collapsed the folder and
/// never opened the menu. The header had TWO pointer surfaces — a full-size
/// `HoldArea` and, declared after it, a `TouchArea` over the name whose
/// `clicked` toggled expansion. The later one wins the grab, so the hold timer
/// was cancelled every time and every folder action was unreachable on touch.
///
/// One surface, both gestures. This is the exact trap `HoldArea`'s own comment
/// warns about ("replaces the row's TouchArea rather than nesting inside it").
#[test]
fn folder_header_has_one_pointer_surface_not_two() {
    let list = include_str!("../../../ui/sidebar/custom_section_list.slint");
    let start = list
        .find("component CustomSectionHeader")
        .expect("CustomSectionHeader present");
    let end = list[start..]
        .find("export component CustomSectionList")
        .map(|i| start + i)
        .unwrap_or(list.len());
    let header = &list[start..end];

    assert!(
        header.contains("if (!self.hold-fired) { root.toggle-expanded(); }"),
        "the header's tap-to-toggle must hang off the HoldArea, guarded by \
         hold-fired so the release of a hold is not also read as a tap"
    );
    assert!(
        !header.contains("clicked => { root.toggle-expanded(); }"),
        "a second TouchArea whose clicked toggles expansion steals the pointer \
         grab from the HoldArea — that is what made the folder menu unreachable \
         on touch (verified on an S25 Ultra: the hold just collapsed the folder)"
    );
}

/// Filing a DM into a folder made it impossible to open.
///
/// `wire_dm_selected` answered "is this anon, and who is the peer" by scanning
/// the `dm_conversations` Slint model — and a foldered DM is deliberately
/// REMOVED from that model (it renders inside the folder instead). The scan
/// came back empty, so no `ActiveConversation` was committed, the loader bailed
/// with "Could not determine peer ID", and the row opened into nothing while
/// every send silently dropped. The lookup lives in core now, where folder
/// membership is irrelevant.
#[test]
fn a_foldered_dm_still_resolves_to_its_conversation() {
    no_keyring();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    let tmp = tempfile::tempdir().unwrap();
    let client = chatter_core::client::ChatterClient::builder()
        .data_dir(tmp.path().to_string_lossy().to_string())
        .build()
        .expect("offline ChatterClient::build");

    let peer = "22222222-2222-2222-2222-222222222222";
    let section = client.create_section("Work", "\u{1F4C1}").expect("create_section");
    client
        .move_conversation_to_section(
            peer,
            chatter_core::domain::message::ConversationType::ServerDm,
            &section.id,
        )
        .expect("move_conversation_to_section");

    // The loose DM list drops the row ...
    assert!(
        client.sidebar_rows().iter().all(|d| d.id.as_str() != peer),
        "a foldered DM must not also render loose"
    );
    // ... and opening it must still work.
    match client.resolve_dm_conversation(peer) {
        Some(chatter_core::state::ActiveConversation::ServerDm { peer_id }) => {
            assert_eq!(peer_id.to_string(), peer);
        }
        other => panic!("a foldered DM must still resolve to its peer, got {other:?}"),
    }

    // L2 static: the handler asks core instead of the model it just emptied.
    let src = include_str!("../src/bin_ui/features/sidebar/dm.rs");
    assert!(
        src.contains("resolve_dm_conversation"),
        "DM selection must resolve the conversation through core"
    );
    assert!(
        !src.contains("get_chat_rows()"),
        "DM selection must not scan the sidebar model — foldered rows are not in it"
    );
}

/// A folder was a way around Anon Mode.
///
/// The loose lists are mode-exclusive: `sidebar_channels` returns nothing and
/// `list_dm_sidebar` drops friend DMs while the shield is on. Folders ignored
/// all of it, so an identified channel filed into "Work" stayed on screen — and
/// stayed clickable — in anon mode.
#[test]
fn folders_hide_identified_conversations_while_anon_mode_is_on() {
    no_keyring();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    let tmp = tempfile::tempdir().unwrap();
    let client = chatter_core::client::ChatterClient::builder()
        .data_dir(tmp.path().to_string_lossy().to_string())
        .build()
        .expect("offline ChatterClient::build");

    let channel = "33333333-3333-3333-3333-333333333333";
    let section = client.create_section("Work", "\u{1F4C1}").expect("create_section");
    client
        .move_conversation_to_section(
            channel,
            chatter_core::domain::message::ConversationType::Channel,
            &section.id,
        )
        .expect("move_conversation_to_section");

    let listed = client.list_sections_with_conversations().expect("list sections");
    assert_eq!(listed[0].conversations.len(), 1, "the folder holds the channel");

    client.set_anon_mode_enabled(true);
    let hidden = client.list_sections_with_conversations().expect("list sections");
    assert!(
        hidden[0].conversations.is_empty(),
        "an identified channel must not surface through a folder in anon mode"
    );
    assert_eq!(
        hidden[0].unread_count, 0,
        "the aggregate badge must count what the folder actually shows"
    );
}

/// A private group filed into a folder rendered TWICE, and stayed doubled after
/// a restart: `list_anon_groups` had no folder filter (the one sidebar list that
/// never got the `sidebar_channels` treatment) and no folder mutation redrew the
/// loose group list at all.
#[test]
fn foldered_private_groups_leave_the_loose_list() {
    let core = include_str!("../../chatter-core/src/client/anon_group.rs");
    let idx = core
        .find("pub fn sidebar_private_groups")
        .expect("sidebar_private_groups present");
    assert!(
        core[idx..].contains("all_sectioned_conversation_ids"),
        "the loose private-group list must drop groups that live in a folder"
    );

    let rows = include_str!("../../chatter-core/src/client/messaging.rs");
    let idx = rows.find("pub fn sidebar_rows").expect("sidebar_rows present");
    assert!(
        rows[idx..idx + 1500].contains("sidebar_private_groups()"),
        "the chat list's group rows must come from the folder-aware core list"
    );

    // Every folder mutation rebuilds the whole chat list, groups included.
    let sections = include_str!("../src/bin_ui/features/sidebar/sections.rs");
    assert!(
        sections.contains("super::rebuild_chat_list("),
        "filing a group into a folder must redraw the chat list too"
    );
}

/// FX3 (owner 2026-09-26): the chat list is ONE list — DMs, channels and
/// private groups interleaved by activity, pinned first (Signal/Telegram). It
/// used to be a DM block above a channel block, so a channel with newer
/// activity never rose above a DM. Core orders it; a live message moves its
/// row to the top of the unpinned rows — never above a pin, which the old
/// per-list `insert(0, ..)` did — and every other row keeps its badge.
#[test]
fn chat_list_is_one_list_by_activity_with_pins_first() {
    use chatter_core::domain::message::ConversationType as CT;
    use chatter_core::ui::sidebar_data::{order_chat_rows, ChatRowSendData};
    use chatter_ui::bin_ui::converters::ChatRowExt;
    use slint::Model;

    const DM: &str = "11111111-1111-1111-1111-111111111111";
    const PINNED: &str = "22222222-2222-2222-2222-222222222222";
    const CHANNEL: &str = "33333333-3333-3333-3333-333333333333";
    const OLD: &str = "44444444-4444-4444-4444-444444444444";
    let row = |id: &str, kind: CT, at: i64, pinned: bool, unread: u32| {
        let mut r = ChatRowSendData::new(kind, id, id);
        r.last_activity = chrono::DateTime::from_timestamp(at, 0);
        r.is_pinned = pinned;
        r.unread_count = unread;
        r
    };
    let mut rows = vec![
        row(DM, CT::ServerDm, 20, false, 2),
        row(OLD, CT::AnonGroup, 5, false, 0),
        row(PINNED, CT::ServerDm, 1, true, 0),
        row(CHANNEL, CT::Channel, 30, false, 0),
    ];
    order_chat_rows(&mut rows);
    let ids: Vec<_> = rows.iter().map(|r| r.id.clone()).collect();
    assert_eq!(ids, [PINNED, CHANNEL, DM, OLD], "pinned first, then a newer channel above an older DM");

    no_keyring();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    let tmp = tempfile::tempdir().unwrap();
    let client = std::sync::Arc::new(
        chatter_core::client::ChatterClient::builder()
            .data_dir(tmp.path().to_string_lossy().to_string())
            .build()
            .expect("offline ChatterClient::build"),
    );
    let w = boot();
    let slint_rows: Vec<_> = rows.into_iter().map(|r| r.to_slint()).collect();
    w.set_chat_rows(slint::ModelRc::new(slint::VecModel::from(slint_rows)));

    // A message lands in the oldest conversation, a private group.
    chatter_ui::bin_ui::features::sidebar::apply_received_to_sidebar(
        &w,
        &client,
        chatter_core::state::ActiveConversation::AnonGroup(OLD.parse().unwrap()),
        "hello",
        "12:00",
        chatter_core::client::UnreadBadges { row: 1, total: 2 },
    );
    let model = w.get_chat_rows();
    let ids: Vec<String> = model.iter().map(|r| r.conversation_id.to_string()).collect();
    assert_eq!(ids, [PINNED, OLD, CHANNEL, DM], "the row moves to the top of the unpinned rows, not above the pin");
    let moved = model.row_data(1).unwrap();
    assert_eq!((moved.last_message.as_str(), moved.unread_count), ("hello", 1));
    assert_eq!(model.row_data(3).unwrap().unread_count, 2, "other rows keep their unread counts");
}

/// A channel row in the sidebar showed the channel name and nothing else, while
/// the DM row beside it showed a preview and a time. The struct had carried
/// `last-message`/`timestamp` the whole time — three separate links were cut:
/// the converter hardcoded both to empty, `ChannelList` never forwarded them,
/// and `ChannelListItem` had no second line to put them on. The private-group
/// row had the last of those three (`timestamp` in, never rendered).
#[test]
fn channel_and_group_rows_show_their_latest_activity() {
    // 1. Population — core composes it, for every row kind, so they cannot
    // drift. A channel's messages live under `Channel::conversation_type()` —
    // `AnonGroup` for an encrypted channel, which is the default — so the
    // bucket is derived from the channel, never hardcoded `Channel` (an empty
    // bucket renders the row as a bare name). See
    // `integration_message_storage.rs` for the storage-level proof.
    let core = include_str!("../../chatter-core/src/client/messaging.rs");
    let idx = core.find("fn channel_row(").expect("channel_row present");
    assert!(
        core[idx..idx + 900].contains("&channel.conversation_type()"),
        "a channel row's preview must come from the channel's own bucket"
    );
    let idx = core.find("fn private_group_row(").expect("private_group_row present");
    assert!(
        core[idx..idx + 700].contains("sidebar_activity("),
        "private-group rows must take core's preview wording"
    );

    // 2. Forwarding — the row must hand each visual what it was given.
    let list = include_str!("../../../ui/sidebar/chat_list.slint");
    for prop in ["last-message: root.data.last-message", "timestamp: root.data.timestamp", "is-typing: root.data.is-typing"] {
        assert_eq!(list.matches(prop).count(), 3, "ChatRow must forward `{prop}` to all three row visuals");
    }

    // 3. Markup — the second line and the right-hand column exist on both rows.
    let channel_row = include_str!("../../../ui/sidebar/channel_list_item.slint");
    assert!(
        channel_row.contains("text: root.last-message;") && channel_row.contains("text: root.timestamp;"),
        "the channel row must render the preview and the timestamp"
    );
    assert!(
        channel_row.contains("root.dot-opacity(0)"),
        "the channel row must animate the typing indicator like the DM row does"
    );
    let group_row = include_str!("../../../ui/sidebar/private_group_list_item.slint");
    assert!(
        group_row.contains("text: root.timestamp;"),
        "the private-group row declared `timestamp` but never rendered it"
    );

    // 4. Typing reaches every row kind, not just DMs: one model, matched on
    // the conversation id or (DMs) the peer.
    let typing = include_str!("../src/bin_ui/features/auth/initial_data.rs");
    let t_idx = typing.find("pub fn update_sidebar_typing").expect("update_sidebar_typing present");
    let t_body = &typing[t_idx..t_idx + 700];
    assert!(
        t_body.contains("row.conversation_id.as_str() == id"),
        "a typing event must animate a channel or group row too, not only a DM row"
    );
}

// ============================================================================
// PEER IDENTITY CHANGE — the prompt and the accept.
//
// Detection of a changed peer identity key has worked for a while: the change
// is parked in `pending_identity_changes` and every send path refuses while it
// is there. It had ZERO production callers on the resolution side, so from the
// user's seat nothing was fixed — the conversation was dead and the only route
// out was clearing app data. These tests exist to keep it reachable.
// ============================================================================

/// Park an identity change for `peer` in the SAME database the offline client
/// opened, through the real `KeyStore` write the receive paths use.
///
/// A second SQLCipher connection rather than the client's own: `KeyStore`
/// access on `ChatterClient` is gated behind `chatter-core/testing`, and the UI
/// suites run without it (`--features chatter-core/dev-trust-root`). Both
/// handles hold the same file with the same passphrase, and this writes one row
/// before the assertions read it back through the façade.
///
/// The key is a well-formed Curve25519 identity key — type byte `0x05` then 32
/// bytes — because `accept_peer_identity_change` runs `IdentityKey::decode`
/// BEFORE pinning, on purpose: a malformed key would turn every later message
/// from the peer into a decode error, a worse wedge than the one being cleared.
fn park_identity_change(data_dir: &std::path::Path, peer: &str, source: &str) -> Vec<u8> {
    let db_path = chatter_core::system::Directories::with_base_dir(data_dir.to_path_buf())
        .expect("Directories::with_base_dir")
        .database_path();
    let db = std::sync::Arc::new(
        chatter_core::storage::Database::open(&db_path, "ui-wiring-harness-passphrase")
            .expect("open the client's database a second time"),
    );
    let key_store = chatter_core::storage::KeyStore::new(db);

    let mut new_identity_key = vec![0x05u8];
    new_identity_key.extend_from_slice(&[0x42u8; 32]);

    key_store
        .record_pending_identity_change(peer, &new_identity_key, source, 1)
        .expect("park the identity change");
    new_identity_key
}

/// Build an offline client wired with the REAL chat callbacks, with a server DM
/// open, so `identity-change-accept-clicked` has a peer to act on.
fn wired_dm_open(
    peer: chatter_core::domain::ids::UserId,
) -> (
    tempfile::TempDir,
    std::sync::Arc<chatter_core::client::ChatterClient>,
    MainWindow,
) {
    no_keyring();
    let rt = test_rt();
    let _guard = rt.enter();
    let tmp = tempfile::tempdir().unwrap();
    testing::init_no_event_loop();
    let client = chatter_core::client::ChatterClient::builder()
        .data_dir(tmp.path().to_string_lossy().to_string())
        .build()
        .expect("offline ChatterClient::build");
    let w = MainWindow::new().expect("MainWindow::new");
    chatter_ui::bin_ui::features::chat::wire(&w, &client);
    // The accept handler reads the peer from the ACTIVE conversation, never
    // from the callback, so the key it pins always belongs to the conversation
    // the prompt was drawn in.
    client
        .chat_state()
        .set_active_conversation(chatter_core::state::ActiveConversation::ServerDm {
            peer_id: peer,
        });
    (tmp, client, w)
}

/// THE test this whole change exists for: the Accept button reaches
/// `SignalService::accept_peer_identity_change` and actually unwedges the
/// conversation.
///
/// Asserted through the façade, not through a spy, so a handler that merely hid
/// the bar would fail: after the click the parked change must be GONE, which
/// only the core transaction (pin the new key, clear the stale verification,
/// drop every session, delete the parked row) can do.
#[test]
fn identity_change_accept_reaches_core_and_clears_the_pending_change() {
    let peer = chatter_core::domain::ids::UserId::new();
    let (tmp, client, w) = wired_dm_open(peer);

    // Nothing wrong yet: no prompt, and the façade agrees.
    assert_eq!(
        client.pending_identity_change(&peer).unwrap(),
        None,
        "precondition: a fresh client must have no parked identity change"
    );

    park_identity_change(tmp.path(), &peer.to_string(), "sender_certificate");

    // Core now reports the wedge, and it is attributed (sealed sender).
    assert_eq!(
        client.pending_identity_change(&peer).unwrap(),
        Some(true),
        "the parked change must be visible through the façade the UI reads"
    );

    // The prompt the user actually sees, refreshed exactly as the DM open path
    // and the PeerIdentityChanged event do it.
    chatter_ui::bin_ui::features::chat::identity_change::refresh(&w, &client, &peer);
    assert!(
        w.get_identity_change_visible(),
        "a parked identity change must raise the in-conversation prompt"
    );
    assert!(
        w.get_identity_change_attributed(),
        "a sealed-sender change is cryptographically attributed"
    );
    assert!(
        !w.get_identity_change_title().is_empty()
            && !w.get_identity_change_detail().is_empty(),
        "the prompt must carry core's wording, not an empty bar"
    );

    // Fire the exact callback the Accept button invokes.
    w.invoke_identity_change_accept_clicked();

    // THE assertion: core ran the accept transaction. A UI-only stub cannot
    // make this true.
    assert_eq!(
        client.pending_identity_change(&peer).unwrap(),
        None,
        "Accept did NOT reach core — the change is still parked, so the \
         conversation is still wedged in both directions and the button is a lie"
    );
    assert!(
        !w.get_identity_change_visible(),
        "the prompt must come down once the change has been accepted"
    );
    assert!(w.get_toast_visible(), "accepting must confirm itself — a bar that \
        merely disappears does not distinguish 'accepted' from 'did nothing'");
}

/// A server-claimed change must NOT be presented as though the peer had been
/// cryptographically attributed. `IdentityChangeSource::is_authenticated`
/// exists for exactly this, and collapsing the two would let a malicious server
/// launder its own key change through the user's trust in the peer's name.
#[test]
fn a_server_claimed_identity_change_is_not_presented_as_attributed() {
    let peer = chatter_core::domain::ids::UserId::new();
    let (tmp, client, w) = wired_dm_open(peer);

    park_identity_change(tmp.path(), &peer.to_string(), "server_claimed");

    assert_eq!(
        client.pending_identity_change(&peer).unwrap(),
        Some(false),
        "an identified-sender change is asserted by the server and proved by nobody"
    );

    chatter_ui::bin_ui::features::chat::identity_change::refresh(&w, &client, &peer);
    assert!(w.get_identity_change_visible(), "it must still be surfaced");
    assert!(
        !w.get_identity_change_attributed(),
        "the UI must not claim attribution the crypto never gave it"
    );

    // The peer's own display name resolves to their uuid on an offline client
    // with no profile row, so this also proves the unattributed copy does not
    // name them.
    let title = w.get_identity_change_title().to_string();
    assert!(
        !title.contains(&peer.to_string()),
        "an unattributed change must not name the peer: the only thing that \
         named them was the server. Got: {title}"
    );
}

/// L2 static: the affordance exists, is wired to the callback, and cannot be
/// dismissed. A dismiss button here would leave the user with a DM that
/// silently refuses to send and no route back.
#[test]
fn the_identity_change_bar_is_wired_and_not_dismissable() {
    let bar = include_str!("../../../ui/chat/identity_change_bar.slint");
    assert!(
        bar.contains("root.accept-clicked()"),
        "the Accept control must fire accept-clicked"
    );
    assert!(
        bar.contains(r#"accessible-role: button"#)
            && bar.contains(r#"accessible-label: "Accept new safety number""#),
        "the control that pins a new identity key must be reachable by \
         assistive tech and must say what it does"
    );
    // No dismiss AFFORDANCE. Matched against declarations rather than the raw
    // text, because the file's own header comment explains why it has none.
    assert!(
        !bar.contains("callback dismiss") && !bar.contains("-dismiss"),
        "the prompt is the only route back to a working conversation — a dismiss \
         control would leave the user with a DM that silently refuses to send"
    );
    assert!(
        bar.matches("TouchArea").count() == 1,
        "the only pointer surface in the bar must be Accept — a second one is \
         either a hidden dismiss or a mis-wire"
    );
    // Wording is core's, so the bar must not hardcode any of it beyond the
    // button label.
    assert!(
        bar.contains("in property <string> title") && bar.contains("in property <string> detail"),
        "title and detail must come in from chatter-core, not be written here"
    );

    let layout = include_str!("../../../ui/chat/chat_layout.slint");
    assert!(
        layout.contains(r#"&& root.conversation-kind == ConversationKind.server-dm: IdentityChangeBar"#),
        "the bar must render inside the conversation, gated on conversation-kind so a \
         prompt cannot survive a switch into a channel, group or mesh room"
    );
    // The whole four-hop chain, because a break at any hop leaves a button that
    // looks alive and reaches nothing:
    // IdentityChangeBar -> ChatLayout -> App -> MainWindow -> Rust.
    assert!(
        layout.contains("root.identity-change-accept-clicked()"),
        "ChatLayout must forward the bar's accept-clicked onwards"
    );
    let app = include_str!("../../../ui/app.slint");
    assert!(
        app.contains("identity-change-accept-clicked => { root.identity-change-accept-clicked(); }"),
        "App must forward accept-clicked from ChatLayout"
    );
    let main = include_str!("../../../ui/main.slint");
    assert!(
        main.contains("callback identity-change-accept-clicked <=> app.identity-change-accept-clicked;"),
        "MainWindow must alias the callback, or `on_identity_change_accept_clicked` \
         is wired to something nothing can fire"
    );
}

/// The send-side toast must stop telling the user to retry something that
/// cannot succeed. `ensure_session` refuses for as long as the change is
/// parked, so "Encryption failed — try again" is advice to loop forever.
#[test]
fn the_send_toast_points_at_the_identity_change_instead_of_saying_retry() {
    use chatter_core::services::message::MessageError;

    let peer = chatter_core::domain::ids::UserId::new();
    let (_tmp, client, _w) = wired_dm_open(peer);

    let err = chatter_core::services::gateway::SendError::Message(
        MessageError::RecipientIdentityChanged { peer_id: peer.to_string() },
    );
    let msg = client.format_send_error(&err, "send");

    assert!(
        !msg.to_lowercase().contains("try again"),
        "retrying cannot ever succeed here — only accepting the new key can. Got: {msg}"
    );
    assert!(
        msg.to_lowercase().contains("safety number"),
        "the toast must point at the identity change, which is the thing the \
         user can actually act on. Got: {msg}"
    );
}

/// Opening a conversation marks it read, and that write is what destroys the
/// unread boundary. So each open path must take the anchor *from*
/// `mark_as_read` — the one call that still sees it — and hand it to the list.
/// Ask core for the boundary anywhere after that and it truthfully answers
/// "nothing unread": the chat then silently always opens at the bottom, which
/// is exactly the bug this replaced and is invisible in every screenshot.
#[test]
fn conversation_open_anchors_on_the_unread_boundary() {
    for (name, src) in [
        ("channel", include_str!("../src/bin_ui/features/sidebar/channel.rs")),
        ("dm", include_str!("../src/bin_ui/features/sidebar/dm.rs")),
        (
            "private_group",
            include_str!("../src/bin_ui/features/sidebar/private_group.rs"),
        ),
    ] {
        let read_idx = src
            .find("mark_as_read()")
            .unwrap_or_else(|| panic!("{name} open path must mark the conversation read"));
        // The anchor is bound from the mark_as_read expression itself, not
        // fetched afterwards from a conversation that is now fully read.
        let bind = src[..read_idx]
            .rfind("let anchor")
            .unwrap_or_else(|| panic!("{name}: no `let anchor` before mark_as_read"));
        assert!(
            read_idx - bind < 300,
            "{name}: the unread anchor must be captured from mark_as_read's return \
             value — nothing else can still see the boundary"
        );
        assert!(
            src.contains("apply_unread_anchor"),
            "{name}: the captured anchor must reach the message list, or the \
             conversation still opens wherever the last one was scrolled to"
        );
    }

    // Core owns which row the boundary is; the UI only forwards the number.
    let sidebar = include_str!("../src/bin_ui/features/sidebar/mod.rs");
    assert!(
        sidebar.contains("anchor.divider_index(page)"),
        "the index must come from chatter-core's UnreadAnchor, not be recomputed \
         in the presentation layer"
    );
    assert!(
        sidebar.contains("set_is_at_bottom(index.is_none())"),
        "is_at_bottom is sticky across conversations; the open has to reset it or \
         a chat left scrolled up decides where the next one opens"
    );

    // The list has to actually consume it, all the way down the alias chain.
    let list = include_str!("../../../ui/chat/message_list.slint");
    assert!(
        list.contains("in-out property <int> unread-index"),
        "MessageList must expose the open position"
    );
    assert!(
        list.contains("Unread messages"),
        "MessageList must draw the unread divider at the boundary"
    );
    assert!(
        list.contains("new messages"),
        "MessageList must offer the jump-to-newest pill with its count"
    );
    // Rows are created on demand by Slint's ListView runtime, which the
    // compiler enables only for a `for` whose parent element is named
    // `ListView` with the visible-*/viewport-* length properties. Lose that
    // name and every row of the conversation is instantiated and walked on
    // every frame again (the 10-20 fps phone scroll this replaced).
    assert!(
        list.contains("component ListView inherits Flickable")
            && list.contains("out property <length> visible-height")
            && list.contains("list := ListView {"),
        "MessageList must keep its rows behind the ListView runtime"
    );
    // That runtime rewrites viewport-y after every layout pass; an animation
    // on it would be cancelled mid-flight and fight the fling.
    assert!(
        !list.contains("animate viewport-y"),
        "MessageList must not animate viewport-y"
    );
    let layout = include_str!("../../../ui/chat/chat_layout.slint");
    assert!(
        layout.contains("unread-index <=> root.unread-index"),
        "ChatLayout must forward the open position to MessageList"
    );
    let app = include_str!("../../../ui/app.slint");
    assert!(
        app.contains("unread-index <=> root.unread-index"),
        "App must forward the open position to ChatLayout"
    );
    assert!(
        app.contains("is-at-bottom <=> root.is-at-bottom"),
        "App must bind is-at-bottom to ChatLayout, or every set_is_at_bottom call \
         in Rust writes to a property nothing reads"
    );
    let main = include_str!("../../../ui/main.slint");
    assert!(
        main.contains("unread-index <=> app.unread-index"),
        "MainWindow must alias unread-index, or set_unread_index is inert"
    );
}

// ============================================================================
// H4 (Consistency and Standards) + H3 (User Control and Freedom): DISMISSAL.
//
// "The close button in some places is on left, other on right, others it's X,
// others it's the back button." It was four spellings of one action — X
// top-right (the shared `Modal`), X top-left (Settings, compact), chevron
// top-left (Profile, mobile), a "Close" text button (Jarvis memory) — in three
// geometries (18-in-40px, 16-in-32px, 14-in-44px).
//
// The rule now lives in exactly one component,
// `ui/components/dismiss_button.slint`: a full-screen PAGE gets a back chevron
// on the leading edge (you pushed into it, you pop out of it — same as
// `chat_header.slint`), a centered DIALOG gets an X on the trailing edge, and
// never both. `Modal` applies it for every screen it wraps; the four screens
// that build their own header instantiate the same component.
//
// This test is what stops the drift coming back. It fails on the tree as it
// stood before this change with four "defines its own dismissal glyph"
// findings (settings_screen, user_profile, device_picker, thread_panel) and
// one "closeable but routes through neither" (jarvis_memory).

/// Every Slint screen that can present a dismissal control, paired with its
/// source. `ui/components/` is excluded on purpose: that is where the shared
/// control is DEFINED, and a toast or a search-field clear-X in there is not a
/// screen exit.
fn dismissable_screens() -> Vec<(String, String)> {
    let ui = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../ui");
    let mut out = Vec::new();
    for dir in ["modals", "settings", "calls"] {
        let mut entries: Vec<_> = std::fs::read_dir(ui.join(dir))
            .unwrap_or_else(|e| panic!("read ui/{dir}: {e}"))
            .map(|e| e.expect("dir entry").path())
            .filter(|p| p.extension().is_some_and(|x| x == "slint"))
            .collect();
        entries.sort();
        for path in entries {
            let name = path.file_name().expect("file name").to_string_lossy().into_owned();
            let src = std::fs::read_to_string(&path).expect("read slint source");
            out.push((format!("ui/{dir}/{name}"), src));
        }
    }
    // The one screen outside those directories that owns a dismissal: a docked
    // side panel on a desktop, a full-width pushed page on a phone.
    let thread = ui.join("chat/thread_panel.slint");
    out.push((
        "ui/chat/thread_panel.slint".into(),
        std::fs::read_to_string(thread).expect("read thread_panel.slint"),
    ));
    out
}

#[test]
fn no_screen_defines_its_own_dismissal_glyph() {
    // The `\u{...}` escapes are the Unicode close glyphs: they are BOTH an
    // inconsistency and an Android bug — the font has no glyph, so they render
    // as a tofu box.
    const OWN_GLYPH: [&str; 4] = [
        "Icons.x",
        "Icons.chevron-left",
        "\\u{00D7}",
        "\\u{2715}",
    ];
    // A screen is "dismissable" if it exposes any of these outward.
    const DISMISS_CALLBACK: [&str; 4] =
        ["callback close(", "callback closed(", "close-clicked", "close-settings"];

    let mut findings = Vec::new();
    for (name, src) in dismissable_screens() {
        for needle in OWN_GLYPH {
            if src.contains(needle) {
                findings.push(format!(
                    "{name} draws its own dismissal glyph ({needle}) — use \
                     DismissButton from ui/components/dismiss_button.slint"
                ));
            }
        }
        if DISMISS_CALLBACK.iter().any(|c| src.contains(c))
            && !src.contains("DismissButton")
            && !src.contains("Modal {")
        {
            findings.push(format!(
                "{name} can be closed but routes through neither DismissButton \
                 nor Modal — its exit is a one-off"
            ));
        }
    }
    assert!(findings.is_empty(), "dismissal drifted:\n  {}", findings.join("\n  "));
}

#[test]
fn dismissal_glyph_and_edge_follow_the_screen_shape() {
    let dismiss = include_str!("../../../ui/components/dismiss_button.slint");
    // The glyph is chosen by shape, in one expression, in one place.
    assert!(
        dismiss.contains("icon: root.back ? Icons.chevron-left : Icons.x;"),
        "DismissButton must pick chevron for a pushed page and X for a dialog"
    );
    // Its geometry is HeaderButton's, so a dismissal control is never a
    // different size from the rest of its header row.
    assert!(
        dismiss.contains("inherits HeaderButton"),
        "DismissButton must be a HeaderButton, not a second header geometry"
    );

    // The EDGE is a layout position, so it is pinned by ORDER inside Modal's
    // header row: chevron before the title, X after it. `full-screen` is the
    // only thing that selects between them, and the two are mutually exclusive
    // `if`s so no screen can ever show both.
    // A tab's root screen (`root-screen`) drops the chevron — it was not
    // pushed, so there is nothing to pop — and a confirm dialog with its own
    // Cancel drops the X (`show-close`). Neither ever swaps one for the other.
    let modal = include_str!("../../../ui/components/modal.slint");
    let leading = modal
        .find("if root.full-screen && !root.root-screen: DismissButton")
        .expect(
            "Modal must place a back chevron on the LEADING edge when full-screen, \
             except on a tab's root screen",
        );
    let title = modal.find("text: root.title;").expect("Modal must render a title");
    let trailing = modal
        .find("if !root.full-screen && root.show-close: DismissButton")
        .expect("Modal must place an X on the TRAILING edge when not full-screen");
    assert!(
        leading < title && title < trailing,
        "Modal's dismissal edges are reversed: the chevron must precede the \
         title and the X must follow it"
    );
    for glyph in ["Icons.x", "Icons.chevron-left"] {
        assert!(
            !modal.contains(glyph),
            "Modal must not name a dismissal glyph ({glyph}) — DismissButton owns that"
        );
    }
}

// FX6 (owner, 2026-09-26): "buttons in headers must be aligned on axis and be
// of same size. This must be applied to all places." Every icon control in a
// title row — back, close, ⋮, search, call, video, +, Shield, fullscreen — is
// `ui/components/header_button.slint` (DismissButton is one), so one box, one
// glyph, one hover and no stroke. Screens had grown their own: chat_header's
// `IconButton`, selection_bar's `BarButton`, a 24px "+" and Shield in the chat
// list, a 32px fullscreen toggle on calls, 40px "×" text glyphs in the media
// viewers, 32px emoji buttons in the file previews.

/// Every `.slint` file under ui/, as (path relative to ui/, source).
fn all_slint_sources() -> Vec<(String, String)> {
    fn walk(dir: &std::path::Path, base: &std::path::Path, out: &mut Vec<(String, String)>) {
        for entry in std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read {dir:?}: {e}")) {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                walk(&path, base, out);
            } else if path.extension().is_some_and(|x| x == "slint") {
                let rel = path.strip_prefix(base).expect("under ui/").to_string_lossy().into_owned();
                out.push((rel, std::fs::read_to_string(&path).expect("read slint source")));
            }
        }
    }
    let ui = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../ui");
    let mut out = Vec::new();
    walk(&ui, &ui, &mut out);
    out.sort();
    out
}

/// The element block (`{ … }`) that is `levels` braces out from `pos`.
fn block_around(src: &str, pos: usize, levels: usize) -> &str {
    let bytes = src.as_bytes();
    let mut open = pos;
    for _ in 0..levels {
        let mut depth = 0usize;
        loop {
            if open == 0 {
                return src;
            }
            open -= 1;
            match bytes[open] {
                b'}' => depth += 1,
                b'{' if depth > 0 => depth -= 1,
                b'{' => break,
                _ => {}
            }
        }
    }
    let mut depth = 0usize;
    for (i, b) in bytes.iter().enumerate().skip(open) {
        match b {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return &src[open..=i];
                }
            }
            _ => {}
        }
    }
    &src[open..]
}

#[test]
fn headers_use_the_shared_header_button() {
    let sources = all_slint_sources();
    let mut findings = Vec::new();

    // 1. No screen keeps a private header-button component.
    for (name, src) in &sources {
        for local in ["component IconButton", "component BarButton"] {
            if src.contains(local) {
                findings.push(format!("{name} defines `{local}` — use HeaderButton"));
            }
        }
    }

    // 2. A back / close / ⋮ / fullscreen glyph drawn next to its own TouchArea
    //    is a hand-rolled header button. The allowlist is the uses that are
    //    NOT in a title row, each with where it lives.
    const HEADER_GLYPHS: [&str; 8] = [
        "Icons.chevron-left",
        "Icons.x",
        "Icons.more-vertical",
        "Icons.maximize",
        "Icons.minimize",
        "\\u{00D7}",
        "\\u{2715}",
        "\\u{22EE}",
    ];
    const NOT_A_HEADER: [(&str, &str); 10] = [
        ("chat/message_composer.slint", "Icons.x"),       // cancel reply / edit strip
        ("chat/message_composer.slint", "\\u{00D7}"),     // remove a pending attachment
        ("chat/pinned_bar.slint", "Icons.x"),             // dismiss the pinned banner
        ("components/connection_banner.slint", "Icons.x"), // dismiss the banner
        ("components/toast.slint", "Icons.x"),            // dismiss a toast
        ("components/emoji_picker.slint", "Icons.x"),     // clear the search field
        ("components/gif_picker.slint", "Icons.x"),       // clear the search field
        ("modals/channel_settings.slint", "Icons.more-vertical"), // a member row's menu
        ("modals/friends.slint", "Icons.more-vertical"),  // a friend row's menu
        ("sidebar/sidebar.slint", "Icons.chevron-left"),  // the rail's collapse control
    ];
    for (name, src) in &sources {
        if name.ends_with("header_button.slint") || name.ends_with("dismiss_button.slint") {
            continue;
        }
        for line_start in src.match_indices("source:").chain(src.match_indices("text:")).map(|(at, _)| at) {
            let line = &src[line_start..src[line_start..].find(['\n', ';']).map_or(src.len(), |n| line_start + n)];
            for glyph in HEADER_GLYPHS {
                // `Icons.x` must not match a longer icon name.
                let hit = line.match_indices(glyph).any(|(at, _)| {
                    !line[at + glyph.len()..].starts_with(|c: char| c.is_ascii_alphanumeric() || c == '-')
                });
                if !hit || NOT_A_HEADER.contains(&(name.as_str(), glyph)) {
                    continue;
                }
                // The glyph's element is one brace out; its button two.
                if block_around(src, line_start, 2).contains("TouchArea") {
                    findings.push(format!(
                        "{name} hand-rolls a header button around {glyph} — use HeaderButton \
                         (or DismissButton for back/close) from ui/components/"
                    ));
                }
            }
        }
    }

    // 3. Every title row with an icon control goes through the shared one.
    for header in [
        "components/modal.slint",
        "chat/chat_header.slint",
        "chat/selection_bar.slint",
        "chat/thread_panel.slint",
        "sidebar/sidebar.slint",
        "calls/device_picker.slint",
        "calls/active_call_view.slint",
        "modals/user_profile.slint",
        "modals/jarvis_memory.slint",
        "settings/settings_screen.slint",
        "components/code_preview_panel.slint",
        "components/pdf_preview_panel.slint",
        "components/image_viewer.slint",
        "components/video_player.slint",
        "components/gif_picker.slint",
        "pages/register.slint",
        "app.slint",
    ] {
        let src = &sources
            .iter()
            .find(|(n, _)| n == header)
            .unwrap_or_else(|| panic!("ui/{header} is gone — update this list"))
            .1;
        if !src.contains("HeaderButton {") && !src.contains("DismissButton {") {
            findings.push(format!("ui/{header} has a header with no HeaderButton in it"));
        }
    }

    // 4. The shared control owns geometry; GlassSurface owns its optical edge.
    let button = include_str!("../../../ui/components/header_button.slint");
    for token in [
        "Spacing.header-action-box-touch",
        "Spacing.header-action-box",
        "Spacing.header-glyph-touch",
        "Spacing.header-glyph",
        "accessible-label: root.label;",
    ] {
        if !button.contains(token) {
            findings.push(format!("HeaderButton lost `{token}`"));
        }
    }
    if button.contains("border-width") {
        findings.push("HeaderButton draws its own stroke instead of using the shared material".into());
    }
    // 5. It centres itself on the row's axis (RC24 D1). A fixed root height
    // leaves the button at the TOP of any taller HorizontalLayout; the root
    // must stretch to the row (floor: the box) and centre the box inside.
    let root_body = &button[button.find("export component HeaderButton").unwrap()..];
    let root_body = &root_body[..root_body.find("box := GlassSurface").expect("HeaderButton lost its inner box")];
    if root_body.lines().any(|l| l.starts_with("    height:")) {
        findings.push("HeaderButton's root fixes its height — it will ride the top of a taller row".into());
    }
    if !root_body.contains("min-height: self.width;") {
        findings.push("HeaderButton's root lost `min-height: self.width;`".into());
    }
    if !button.contains("y: (parent.height - self.height) / 2;") {
        findings.push("HeaderButton no longer centres its box vertically".into());
    }

    assert!(findings.is_empty(), "header buttons drifted:\n  {}", findings.join("\n  "));
}

/// H3 (User Control and Freedom): every irreversible action in the app sat one
/// click from done, in the same row rhythm as reversible ones ("Archive",
/// "Unmute"). Each now takes the two-step reveal Settings already used for
/// Delete Account — ONE destructive vocabulary, not one per screen.
///
/// Leave Channel is gated unconditionally rather than only for private
/// channels: a control that is sometimes destructive and sometimes not, with
/// no visual difference, is worse than one that always asks.
///
/// FAILS BEFORE: all three fired their callback straight from the button's
/// `clicked`, and none of the three `show-*-confirm` properties existed.
#[test]
fn destructive_actions_need_a_second_click() {
    // (screen, source, confirm flag, the callback it gates)
    let screens: [(&str, &str, &str, &str); 3] = [
        (
            "channel_settings/delete",
            include_str!("../../../ui/modals/channel_settings.slint"),
            "show-delete-confirm",
            "root.delete-channel();",
        ),
        (
            "channel_settings/leave",
            include_str!("../../../ui/modals/channel_settings.slint"),
            "show-leave-confirm",
            "root.leave-channel();",
        ),
        (
            "call_history/clear",
            include_str!("../../../ui/calls/call_history_screen.slint"),
            "show-clear-confirm",
            "root.clear-history();",
        ),
    ];

    for (name, src, flag, callback) in screens {
        assert!(
            src.contains(&format!("property <bool> {flag}: false;")),
            "{name}: must gate the destructive action behind an explicit confirm reveal"
        );
        // The FIRST button may only reveal the confirm — never fire. Where the
        // reveal lives is the screen's business (call history's is in a header
        // ⋮ menu at the end of the file), so this is not a source-order check:
        // every place the action fires must sit inside a conditional that
        // tests the flag, and the reveal must not be inside one.
        let reveal = src
            .find(&format!("root.{flag} = true;"))
            .unwrap_or_else(|| panic!("{name}: the button must reveal the confirm step"));
        assert!(
            !gated_on(&enclosing_if(src, reveal), flag),
            "{name}: the reveal is itself behind the confirm — nothing can open it"
        );
        let fires: Vec<usize> = src.match_indices(callback).map(|(at, _)| at).collect();
        assert!(!fires.is_empty(), "{name}: the confirm step must still be able to act");
        for fire in fires {
            assert!(
                gated_on(&enclosing_if(src, fire), flag),
                "{name}: {callback} is reachable without the confirm reveal — the gate is bypassed"
            );
        }
        // Reopening must not land on the confirm step (possibly for a
        // different channel), so the reveal resets whenever the screen is
        // shown or hidden.
        let reset = src
            .find("changed show-modal =>")
            .unwrap_or_else(|| panic!("{name}: no reset-on-open handler"));
        assert!(
            src[reset..reset + 200].contains(&format!("self.{flag} = false;")),
            "{name}: {flag} must reset on open, or a screen closed mid-confirm \
             reopens already asking to do the irreversible thing"
        );
    }
}

/// The conditions of every `if …:` element enclosing `pos`, innermost first,
/// joined with ` && ` — the text between `if` and the end of each element
/// header, found by walking out through the unmatched `{` braces. Empty when
/// no conditional encloses it.
fn enclosing_if(src: &str, pos: usize) -> String {
    let bytes = src.as_bytes();
    let mut conds = Vec::new();
    let mut depth = 0usize;
    let mut i = pos;
    while i > 0 {
        i -= 1;
        match bytes[i] {
            b'}' => depth += 1,
            b'{' if depth > 0 => depth -= 1,
            b'{' => {
                let line_start = src[..i].rfind('\n').map_or(0, |n| n + 1);
                if let Some(cond) = src[line_start..i].trim().strip_prefix("if ") {
                    conds.push(cond);
                }
            }
            _ => {}
        }
    }
    conds.join(" && ")
}

/// `cond` requires `root.{flag}` to be TRUE — a `!root.{flag}` does not count.
fn gated_on(cond: &str, flag: &str) -> bool {
    let needle = format!("root.{flag}");
    cond.match_indices(&needle)
        .any(|(at, _)| at == 0 || cond.as_bytes()[at - 1] != b'!')
}

/// Touch-target minimums are an accessibility floor, not a style preference.
/// The profile's shared-content pill was a 40px shell with a 3px inset, so the
/// chip you actually tap was 34px. The shell is now sized FROM the chip.
#[test]
fn shared_content_pill_tabs_clear_the_touch_floor() {
    let src = include_str!("../../../ui/modals/user_profile.slint");
    let strip = src
        .find("Shared-content tab strip")
        .expect("profile must have a shared-content tab strip");
    // Up to the next section, so the assertion cannot pass on some other
    // element's geometry further down the page.
    let end = src[strip..]
        .find("Media / GIF grid")
        .expect("the grid follows the tab strip");
    let block = &src[strip..strip + end];
    assert!(
        block.contains("height: Spacing.touch-target-min;") && block.contains("padding: Spacing.xxs;"),
        "the pill shell must be Spacing.touch-target-min with a 2px inset, so each \
         chip lands on the 44px touch floor — a literal here is how 34px happened"
    );
}

// ============================================================================
// DROPPED-P1s — findings that a completeness review found were never assigned
// to any work package. See docs/OPEN-ITEMS.md §58, §59, §63.
// ============================================================================

/// FEAT-8: the empty-state "Browse channels" CTA — the first control a
/// brand-new account ever sees — was dead.
///
/// `ChannelList` (now `ChatList`, `ui/sidebar/chat_list.slint`) emitted
/// `join-channel-clicked` from its empty-state CTA, and the empty state rendered exactly when
/// `channels.length == 0`. Its ONLY instantiation, in
/// `ui/sidebar/sidebar.slint`, bound `channel-selected` and
/// `channel-context-menu-requested` and not this one, and no Rust `on_*`
/// existed either — so the button did nothing at all.
///
/// This pins the whole chain: emitter -> ChatList instantiation -> sidebar
/// root callback -> app.slint, which opens the join tab of CreateChannelModal.
#[test]
fn sidebar_empty_state_join_cta_is_wired_end_to_end() {
    let list_src = include_str!("../../../ui/sidebar/chat_list.slint");
    let sidebar_src = include_str!("../../../ui/sidebar/sidebar.slint");
    let app_src = include_str!("../../../ui/app.slint");

    // The emitter exists and the empty state's "Browse channels" fires it.
    assert!(
        list_src.contains("callback join-channel-clicked();"),
        "ChatList must declare join-channel-clicked"
    );
    let cta = list_src
        .find("text: \"Browse channels\";")
        .expect("the empty state must still offer Browse channels");
    assert!(
        list_src[cta..cta + 400].contains("root.join-channel-clicked()"),
        "the empty state's Browse channels must emit join-channel-clicked"
    );

    // The instantiation binds it. This is the assertion that was missing.
    let inst = sidebar_src
        .find("ChatList {")
        .expect("sidebar must instantiate ChatList");
    let block = &sidebar_src[inst..];
    let block_end = block.find("\n                }\n").unwrap_or(block.len());
    assert!(
        block[..block_end].contains("join-channel-clicked() => { root.join-channel-clicked(); }"),
        "the ChatList instantiation in sidebar.slint must bind \
         join-channel-clicked — without it the empty-state CTA a brand-new \
         account sees first is dead (FEAT-8)"
    );

    // The sidebar re-emits it, and app.slint turns it into the join modal.
    assert!(
        sidebar_src.contains("callback join-channel-clicked();"),
        "Sidebar must declare the callback it forwards to"
    );
    let handler = app_src
        .find("join-channel-clicked =>")
        .expect("app.slint must handle the sidebar's join-channel-clicked");
    let tail = &app_src[handler..handler + 220];
    assert!(
        tail.contains(r#"root.create-channel-initial-tab = 1"#)
            && tail.contains(r#"root.nav-push("create-channel")"#),
        "join-channel-clicked must open the JOIN tab of CreateChannelModal"
    );
}

/// STUB-1a: the Tor status dot must not render "Off" as amber "connecting".
///
/// `TorStatus.state == 0` means Off — not on onion, traffic going direct
/// (`ui/anon_mode/anon_mode_global.slint`;
/// `chatter-core/src/client/mod.rs` sets it). The dot's ternary fell through to
/// `Colors.warning` for every state that was not 2 or 3, and the file's own
/// comment defined amber as "connecting" — so a build with no Tor in it showed
/// a reassuring amber dot forever, and
/// `ui/components/connection_banner.slint` only banners states 1 and 3, so
/// nothing else told the user.
#[test]
fn tor_status_dot_does_not_report_off_as_connecting() {
    let src = include_str!("../../../ui/sidebar/sidebar.slint");

    // Both shield entry points (expanded footer + collapsed rail) carry a dot.
    let dots: Vec<_> = src.match_indices("TorStatus.state == 2 ? Colors.success").collect();
    assert_eq!(
        dots.len(),
        2,
        "expected the Tor dot at both shield entry points (expanded footer and \
         collapsed rail); found {}",
        dots.len()
    );

    for (at, _) in dots {
        let ternary = &src[at..at + 260];
        assert!(
            ternary.contains("TorStatus.state == 1 ? Colors.warning"),
            "amber must be reserved for state 1 (Connecting); this dot still \
             uses it as the fallback, which renders state 0 (Off — traffic is \
             going direct) as 'connecting' (STUB-1)"
        );
        // The fallback arm must not be warning.
        let fallback = ternary
            .split(": ")
            .last()
            .unwrap_or("");
        assert!(
            !fallback.starts_with("Colors.warning"),
            "the fallback arm of the Tor dot must not be amber"
        );
    }
}

/// Shield is offered in every build; only the Tor promise is gated.
///
/// STUB-1b once hid every Shield control in a build without the `onion`
/// feature, so the Android build (no onion) had no Shield entry point at all
/// (RC20). Shield is the anonymous-mailbox mode and works without Tor: core
/// sets TorStatus Unavailable and ShieldNetworkBanner says the network
/// location is not hidden. What must stay gated is the Tor status dot, which
/// without Tor would be a permanent red.
#[test]
fn shield_toggle_is_offered_without_tor_but_the_tor_dot_is_not() {
    let global_src = include_str!("../../../ui/anon_mode/anon_mode_global.slint");
    assert!(
        global_src.contains("in-out property <bool> onion-built"),
        "AnonMode must carry the mirrored `onion` Cargo feature flag"
    );

    let sidebar_src = include_str!("../../../ui/sidebar/sidebar.slint");
    let settings_src = include_str!("../../../ui/settings/settings_screen.slint");
    assert!(
        !sidebar_src.contains("if AnonMode.onion-built:")
            && !settings_src.contains("if AnonMode.onion-built:"),
        "a Shield entry point is gated on the `onion` feature again — the \
         Android build has no onion, so it would have no Shield toggle"
    );
    let shields = sidebar_src.matches("root.anon-mode-clicked()").count();
    let dots = sidebar_src.matches("visible: AnonMode.enabled && AnonMode.onion-built;").count();
    assert_eq!(dots, shields, "every shield's Tor dot must be gated on onion-built");

    // Rust mirrors the Cargo feature, exactly as it does for jarvis.
    let lib_src = include_str!("../src/lib.rs");
    assert!(
        lib_src.contains(r#"set_onion_built(cfg!(feature = "onion"))"#),
        "lib.rs must mirror the `onion` Cargo feature into AnonMode.onion-built"
    );

    // Dynamic: under this test binary's feature set the global reflects reality.
    use slint::ComponentHandle;
    let w = boot();
    assert_eq!(
        w.global::<AnonMode>().get_onion_built(),
        cfg!(feature = "onion"),
        "AnonMode.onion-built must equal the compiled-in `onion` feature"
    );
}

/// FEAT-5: the "AI assistant" channel template always produced an empty
/// channel, and said nothing.
///
/// `integration_client.rs` hardcodes `secrets: Default::default()`, the server
/// requires `secrets["api_key"]` unconditionally
/// (`internal/services/ai/providers/factory.go:85`) and validates BEFORE it
/// writes (`integration/service/service.go:89` runs `handler.Initialize` ahead
/// of `CreateIntegration` at `:96`), so the integration was never created — and
/// the client swallowed the error with a `tracing::error!`.
///
/// The honest resolution is removal: the template is gated on the
/// off-by-default `ai-channel-template` Cargo feature, mirrored into
/// `AiIntegrationGlobal.channel-template-enabled`, so it is not rendered at
/// all in a default build. The error is surfaced rather than logged for the
/// build that turns it on.
#[test]
fn ai_assistant_channel_template_is_off_and_its_failure_is_surfaced() {
    let global_src = include_str!("../../../ui/ai/ai_global.slint");
    assert!(
        global_src.contains("in-out property <bool> channel-template-enabled: false"),
        "the AI-assistant template flag must default to false"
    );

    // The whole template block is behind the gate.
    let modal_src = include_str!("../../../ui/modals/create_channel.slint");
    assert!(
        modal_src
            .contains("if AiIntegrationGlobal.channel-template-enabled: VerticalLayout {"),
        "the AI-assistant block in create_channel.slint must be gated on \
         AiIntegrationGlobal.channel-template-enabled (FEAT-5)"
    );

    // Rust mirrors the Cargo feature.
    let lib_src = include_str!("../src/lib.rs");
    assert!(
        lib_src
            .contains(r#"set_channel_template_enabled(cfg!(feature = "ai-channel-template"))"#),
        "lib.rs must mirror the `ai-channel-template` Cargo feature"
    );

    // The failure is reported, not swallowed. The old code's whole response to
    // a failed integration was a tracing::error!.
    let handler_src = include_str!("../src/bin_ui/features/modals/create_channel.rs");
    let at = handler_src
        .find("Failed to create AI integration")
        .expect("the AI integration failure path must still exist");
    let tail = &handler_src[at..];
    assert!(
        tail.contains("set_toast_message") && tail.contains(r#"set_toast_variant("error""#),
        "a failed AI integration must surface an error toast, not only a log \
         line — the channel is created either way and the user was told nothing"
    );
    assert!(
        tail.contains("the AI assistant was not added"),
        "the toast must say what actually happened"
    );

    // Dynamic: default build renders no AI template.
    use slint::ComponentHandle;
    let w = boot();
    assert_eq!(
        w.global::<AiIntegrationGlobal>().get_channel_template_enabled(),
        cfg!(feature = "ai-channel-template"),
        "AiIntegrationGlobal.channel-template-enabled must equal the compiled-in feature"
    );
    assert!(
        !cfg!(feature = "ai-channel-template"),
        "the ai-channel-template feature must stay OFF in the default test build: \
         nothing on that path can supply the provider API key the server requires"
    );
}

/// §124 1b: the Calls screen was reachable only from the phone tab bar, so a
/// two-pane tablet (touch, >= 600 dp) had no way in. The sidebar now carries a
/// Calls row on every wide layout; on a phone it stays hidden because the tab
/// bar is the entry there.
#[test]
fn tablet_sidebar_opens_the_calls_screen() {
    use slint::ComponentHandle;
    require_element_queries();
    let w = boot();
    w.set_current_view("main".into());
    w.set_is_mobile_platform(true);

    // Two-pane tablet.
    w.window().set_size(slint::LogicalSize::new(1000.0, 800.0));
    assert!(!w.get_compact_layout(), "1000 dp must be the two-pane layout");
    let row = testing::ElementHandle::find_by_accessible_label(&w, "Calls")
        .next()
        .expect("no Calls entry in the tablet sidebar");
    row.invoke_accessible_default_action();
    assert_eq!(
        w.get_active_tab().as_str(),
        "calls",
        "the sidebar Calls row did not raise the Calls screen"
    );

    // Missed calls surface on the row itself.
    w.set_active_tab("chats".into());
    w.set_missed_call_count(3);
    assert!(
        testing::ElementHandle::find_by_accessible_label(&w, "Calls, 3 missed").next().is_some(),
        "missed-call count is not on the sidebar Calls row"
    );

    // Phone: the tab bar owns the entry; the sidebar must not duplicate it.
    w.set_missed_call_count(0);
    w.window().set_size(slint::LogicalSize::new(400.0, 800.0));
    assert!(w.get_compact_layout());
    // The tab bar's own Calls tab carries the same label, so exactly one.
    assert_eq!(
        testing::ElementHandle::find_by_accessible_label(&w, "Calls").count(),
        1,
        "phone layout shows a sidebar Calls row next to the tab bar's Calls tab"
    );
}

// §124 1b: channel member rows showed initials even for a member with an
// avatar — `ChannelMemberData` had no image field at all. The row converter
// must carry the member's decoded avatar into the settings member list, and
// leave it empty (initials fallback) when there is none.
#[test]
fn channel_member_row_carries_avatar() {
    use chatter_core::domain::channel::{MemberRole, MemberWithProfile, Membership};
    use chatter_ui::bin_ui::converters::member_to_slint;
    use slint::{Model, ModelRc, VecModel};
    let w = boot();

    let member = |avatar: Option<Vec<u8>>| MemberWithProfile {
        membership: Membership {
            channel_id: uuid::Uuid::new_v4().into(),
            user_id: uuid::Uuid::new_v4().into(),
            role: MemberRole::Member,
            joined_at: chrono::Utc::now(),
            last_read_message_id: None,
            is_muted: false,
            muted_until: None,
            notification_preference: Default::default(),
        },
        display_name: "P2".into(),
        username: String::new(),
        avatar_bytes: avatar,
        moderation: Default::default(),
        unverified: false,
    };

    let with = member_to_slint(&member(Some(TINY_PNG.to_vec())));
    let without = member_to_slint(&member(None));
    assert_eq!(without.avatar_source.size().width, 0, "no avatar must fall back to initials");

    w.set_channel_settings_members(ModelRc::new(VecModel::from(vec![with, without])));
    let rows = w.get_channel_settings_members();
    let bound = rows.row_data(0).expect("row 0").avatar_source;
    assert!(
        bound.size().width > 0,
        "member avatar did not reach the channel-settings member row"
    );
}

/// Tap the visible Text labelled `label` at its centre, as a finger would.
/// Text takes no input, so the tap lands on whatever TouchArea is under it.
fn tap_text(w: &MainWindow, label: &str) {
    let text = testing::ElementHandle::find_by_accessible_label(w, label)
        .find(|e| e.accessible_role() == Some(testing::AccessibleRole::Text))
        .unwrap_or_else(|| panic!("no Text labelled {label:?} on screen"));
    text.mock_single_click(slint::platform::PointerEventButton::Left);
}

fn tap_switch(w: &MainWindow, label: &str) {
    let switch = testing::ElementHandle::find_by_accessible_label(w, label)
        .find(|e| e.accessible_role() == Some(testing::AccessibleRole::Switch))
        .unwrap_or_else(|| panic!("no switch labelled {label:?} on screen"));
    switch.mock_single_click(slint::platform::PointerEventButton::Left);
}

/// §124: the Mute and AI toggles in Channel Settings (and the app-settings
/// toggle rows, which share `ToggleRow`) responded only to the 44x24 switch.
/// Tapping the row's label must flip the state, tapping the switch must flip
/// it exactly once (not once for the switch and again for the row under it),
/// and a state pushed from Rust after a tap must still reach the switch — a
/// one-way `checked:` binding would have been dropped by the first tap.
#[test]
fn toggle_rows_flip_from_anywhere_on_the_row() {
    use slint::ComponentHandle;
    require_element_queries();
    let w = boot();
    w.set_current_view("main".into());
    w.window().set_size(slint::LogicalSize::new(1000.0, 800.0));

    // ---- Channel Settings > Notifications: Mute + AI ------------------------
    let muted = std::rc::Rc::new(std::cell::RefCell::new(Vec::<bool>::new()));
    let m = muted.clone();
    w.on_channel_settings_toggle_mute(move |v| m.borrow_mut().push(v));
    let ai = std::rc::Rc::new(std::cell::RefCell::new(Vec::<bool>::new()));
    let a = ai.clone();
    w.on_channel_settings_toggle_ai(move |v| a.borrow_mut().push(v));

    chatter_ui::bin_ui::features::back::push(&w, "channel-settings");
    tap_text(&w, "Notifications");

    tap_text(&w, "Mute Channel");
    assert!(w.get_channel_settings_muted(), "tapping the Mute label did not mute");
    assert_eq!(*muted.borrow(), vec![true], "the Mute row tap did not reach the handler once");

    tap_switch(&w, "Mute Channel");
    assert!(!w.get_channel_settings_muted(), "tapping the Mute switch did not unmute");
    assert_eq!(
        *muted.borrow(),
        vec![true, false],
        "one tap on the switch fired more than one toggle (switch + row underneath)"
    );

    // Core snapping the value back (as the AI handler does on a failed save)
    // must still reach the switch after the user has tapped it.
    tap_text(&w, "AI assistant");
    assert!(w.get_channel_settings_ai_enabled(), "tapping the AI label did not enable it");
    assert_eq!(*ai.borrow(), vec![true]);
    w.set_channel_settings_ai_enabled(false);
    let ai_switch = testing::ElementHandle::find_by_accessible_label(&w, "AI assistant")
        .find(|e| e.accessible_role() == Some(testing::AccessibleRole::Switch))
        .expect("AI switch");
    assert_eq!(
        ai_switch.accessible_checked(),
        Some(false),
        "a value set from Rust after a tap no longer reaches the switch"
    );

    // ---- App settings > Privacy: a ToggleRow outside the modal --------------
    chatter_ui::bin_ui::features::back::close(&w, "channel-settings");
    chatter_ui::bin_ui::features::back::push(&w, "settings");
    tap_text(&w, "Privacy");
    let before = w.get_settings_read_receipts();
    tap_text(&w, "Read Receipts");
    assert_eq!(
        w.get_settings_read_receipts(),
        !before,
        "tapping the Read Receipts label did not flip it"
    );
}

/// Disappearing messages: the picker's choices come from core in Signal's
/// order, and the DM-settings row is offered for a friend only — core announces
/// the timer over the friend DM and has no DM to announce it on otherwise.
#[test]
fn disappearing_picker_offers_presets_to_friends_only() {
    use chatter_ui::DisappearingGlobal;
    use slint::{ComponentHandle, Model};
    let (rt, _tmp, client, w) = wired_offline();
    let _guard = rt.enter();
    chatter_ui::bin_ui::features::chat::disappearing::wire(&w, &client);

    let g = w.global::<DisappearingGlobal>();
    let labels: Vec<String> = g.get_options().iter().map(|o| o.label.to_string()).collect();
    assert_eq!(
        labels,
        ["Off", "30 seconds", "5 minutes", "1 hour", "8 hours", "1 day", "1 week", "4 weeks"]
    );
    let seconds: Vec<i32> = g.get_options().iter().map(|o| o.seconds).collect();
    assert_eq!(seconds, [0, 30, 300, 3_600, 28_800, 86_400, 604_800, 2_419_200]);

    // No conversation open: no timer beside the header's name.
    assert_eq!(g.get_header_label().as_str(), "");

    let peer = chatter_core::domain::ids::UserId::new();
    chatter_ui::bin_ui::features::chat::disappearing::show_for_profile(&w, &client, &peer, false);
    assert!(!g.get_dm_can_set(), "a non-friend has no DM to carry a timer");

    chatter_ui::bin_ui::features::chat::disappearing::show_for_profile(&w, &client, &peer, true);
    assert!(g.get_dm_can_set(), "a friend's DM settings offer the picker");
    assert_eq!(g.get_dm_seconds(), 0);
    assert_eq!(g.get_dm_label().as_str(), "Off");
}

// ============================================================================
// F29: channel moderation (ban / unban / mute / unmute) reaches core.
//
//  L1 (DYNAMIC): the members-pane callbacks run their real handlers offline
//      without panicking (the RPCs fail with no server; the wire is what runs).
//  L2 (STATIC): each action calls its ChatterClient method, and the Slint rows
//      gate Ban/Mute on core's per-member permission, not on a UI-side rule.
// ============================================================================
#[test]
fn channel_settings_moderation_reaches_core() {
    no_keyring();
    let rt = test_rt();
    let _guard = rt.enter();
    let _tmp = tempfile::tempdir().unwrap();
    testing::init_no_event_loop();
    let client = chatter_core::client::ChatterClient::builder()
        .data_dir(_tmp.path().to_string_lossy().to_string())
        .build()
        .expect("offline ChatterClient::build");
    let w = MainWindow::new().expect("MainWindow::new");
    chatter_ui::bin_ui::features::modals::wire_channel_settings(&w, &client);
    w.set_channel_settings_id(uuid::Uuid::new_v4().to_string().into());
    let target: slint::SharedString = uuid::Uuid::new_v4().to_string().into();
    w.invoke_channel_settings_ban_member(target.clone());
    w.invoke_channel_settings_unban_member(target.clone());
    w.invoke_channel_settings_set_member_muted(target.clone(), true, 0);
    w.invoke_channel_settings_set_member_muted(target, false, 0);

    let src = include_str!("../src/bin_ui/features/modals/channel_settings.rs");
    for call in [".ban_user(", ".unban_user(", ".mute_user(", ".unmute_user(", ".list_banned_members("] {
        assert!(src.contains(call), "channel settings must call ChatterClient{call}");
    }
    let conv = include_str!("../src/bin_ui/converters/channel.rs");
    assert!(
        conv.contains("can_ban: m.moderation.can_ban") && conv.contains("can_mute: m.moderation.can_mute"),
        "Ban/Mute visibility must come from core's MemberModeration"
    );
    let slint = include_str!("../../../ui/modals/channel_settings.slint");
    assert!(slint.contains("if member.can-ban: Button") && slint.contains("if member.can-mute && member.muted: Button"));
    assert!(slint.contains("root.unban-member(banned.user-id)"));
}

/// UX3 (owner decisions 2026-09-26): Settings left the tab bar for the
/// Profile tab's account menu, and the chat list's create actions moved from
/// in-list rows to a '+' (a FAB on a phone).
#[test]
fn ux3_account_menu_pushes_settings_and_fab_opens_create_menu() {
    use chatter_ui::bin_ui::features::back;
    use chatter_ui::Appearance;
    use slint::ComponentHandle;
    require_element_queries();
    let w = boot();
    back::wire(&w);
    w.set_current_view("main".into());

    // The bar is Chats, Calls, Contacts, Profile; nothing routes to a
    // Settings tab any more.
    let nav = include_str!("../../../ui/components/mobile_bottom_nav.slint");
    assert!(!nav.contains(r#"tab-selected("settings")"#), "Settings is back in the tab bar");
    assert_eq!(nav.matches("\n    NavTab {").count(), 4, "the tab bar is not four tabs");
    let app = include_str!("../../../ui/app.slint");
    assert!(!app.contains(r#"active-tab = "settings""#), "something still opens Settings as a tab");

    // Account menu › Settings pushes Settings over the Profile tab.
    let opened = std::rc::Rc::new(std::cell::Cell::new(0));
    let o = opened.clone();
    w.on_settings_clicked(move || o.set(o.get() + 1));
    w.window().set_size(slint::LogicalSize::new(1000.0, 800.0));
    w.set_active_tab("profile".into());
    let settings_row = testing::ElementHandle::find_by_accessible_label(&w, "Settings")
        .find(|e| e.accessible_role() == Some(testing::AccessibleRole::Button))
        .expect("the account menu has no Settings row");
    settings_row.mock_single_click(slint::platform::PointerEventButton::Left);
    assert_eq!(w.get_active_modal().as_str(), "settings", "the Settings row did not push Settings");
    assert!(w.get_settings_open());
    assert_eq!(opened.get(), 1, "Settings opened without hydrating (settings-clicked)");
    assert!(back::handle_back(&w));
    assert_eq!((w.get_active_tab().as_str(), w.get_active_modal().as_str()), ("profile", "none"));

    // Night Mode shows the palette in force and pins the opposite one.
    let appearance = w.global::<Appearance>();
    appearance.set_theme("dark".into());
    tap_switch(&w, "Night Mode");
    assert_eq!(appearance.get_theme().as_str(), "light", "Night Mode off did not pick Light");
    tap_switch(&w, "Night Mode");
    assert_eq!(appearance.get_theme().as_str(), "dark", "Night Mode on did not pick Dark");

    // Phone: the FAB is on the Chats list and opens the create menu, and is
    // gone while anything is pushed over the list.
    w.set_is_mobile_platform(true);
    w.window().set_size(slint::LogicalSize::new(400.0, 800.0));
    w.set_active_tab("chats".into());
    let fab = || {
        testing::ElementHandle::find_by_accessible_label(&w, "Create")
            .find(|e| e.accessible_role() == Some(testing::AccessibleRole::Button))
    };
    fab().expect("no create FAB on the phone Chats list")
        .mock_single_click(slint::platform::PointerEventButton::Left);
    assert_eq!(w.get_active_modal().as_str(), "create-menu");
    assert!(fab().is_none(), "the FAB stayed up over its own menu");
    assert!(
        testing::ElementHandle::find_by_accessible_label(&w, "New Section").next().is_some(),
        "the create menu has no New Section"
    );
    assert!(back::handle_back(&w));
    assert_eq!(w.get_active_modal().as_str(), "none");
}

/// D1 / RC24 D5: a call answered from the phone's chat list raises the pane;
/// when the call ends the phone falls back to the list — whether or not a chat
/// was left behind the pane (opening a DM then Back leaves `channel-name` set,
/// which is what defeated the old `channel-name == ""` guard). A call over an
/// open chat returns to that chat. The steps below mirror `open_call_screen`
/// (calls/incoming.rs), which is private to its module.
#[test]
fn phone_call_end_without_chat_returns_to_list() {
    use slint::ComponentHandle;
    let w = boot();
    w.set_current_view("main".into());
    w.set_is_mobile_platform(true);
    w.window().set_size(slint::LogicalSize::new(400.0, 800.0));
    assert!(w.get_compact_layout());
    let tick = slint::platform::update_timers_and_animations;
    let answer = |w: &MainWindow| {
        // `changed` handlers run on the next tick, not inside the setter.
        w.set_active_call_visible(true);
        if !w.get_mobile_show_chat() {
            w.set_call_raised_pane(true);
        }
        w.set_mobile_show_chat(true);
        tick();
    };

    // Answered from the list, no chat ever opened.
    w.set_channel_name("".into());
    w.set_mobile_show_chat(false);
    tick();
    answer(&w);
    w.set_active_call_visible(false);
    tick();
    assert!(!w.get_mobile_show_chat(), "call end left the phone on the empty pane");

    // Opened a DM, went Back, then answered from the list.
    w.set_channel_name("Bob".into());
    w.set_mobile_show_chat(true);
    tick();
    assert!(chatter_ui::bin_ui::features::back::handle_back(&w));
    tick();
    assert!(!w.get_mobile_show_chat());
    answer(&w);
    w.set_active_call_visible(false);
    tick();
    assert!(!w.get_mobile_show_chat(), "call end opened the last DM instead of the list");

    // Answered over an open chat: stays in it.
    w.set_mobile_show_chat(true);
    tick();
    answer(&w);
    w.set_active_call_visible(false);
    tick();
    assert!(w.get_mobile_show_chat(), "call end closed the chat it was placed from");
}

/// Glass optics stay centralized and outside the scroll delegates fixed by PERF1/PERF2.
#[test]
fn glass_material_is_chrome_only_with_one_optical_edge() {
    let sources = all_slint_sources();
    let mut backdrops = Vec::new();
    for (name, src) in &sources {
        let code = src.lines().map(|line| line.split("//").next().unwrap_or("")).collect::<Vec<_>>().join("\n");
        if code.contains("Backdrop {") {
            backdrops.push(name.as_str());
        }
        if code.contains("Materials.glass-edge") {
            assert_eq!(name, "components/glass_surface.slint", "optical edges belong only to GlassSurface");
        }
        if name.ends_with("_list_item.slint") || name == "chat/message_item.slint" {
            assert!(!code.contains("GlassSurface"), "{name} puts glass in a scroll delegate");
            assert!(!code.contains("Backdrop"), "{name} samples the backdrop while scrolling");
            assert!(!code.contains("drop-shadow-"), "{name} restores a per-row shadow");
        }
    }
    assert_eq!(backdrops, ["components/glass_surface.slint"]);
    let glass = sources.iter().find(|(name, _)| name == "components/glass_surface.slint").unwrap().1.as_str();
    assert!(glass.contains("Appearance.glass-effects"));
    assert!(glass.contains("Appearance.reduce-motion"));
    assert!(!glass.contains("iteration-count"), "glass must settle without an idle animation loop");
    let modal = sources.iter().find(|(name, _)| name == "components/modal_container.slint").unwrap().1.as_str();
    assert!(modal.contains("material-enabled: !root.full-screen;"), "full-screen content is not a glass sheet");
}


/// Chrome must sample scrolling content, with clearance inside the scroll range.
#[test]
fn floating_chrome_keeps_content_insets_and_clear_materials() {
    let chat = include_str!("../../../ui/chat/chat_layout.slint");
    let list = include_str!("../../../ui/chat/message_list.slint");
    let sidebar = include_str!("../../../ui/sidebar/sidebar.slint");
    let app = include_str!("../../../ui/app.slint");
    let material = include_str!("../../../ui/theme/materials.slint");
    let nav = include_str!("../../../ui/components/mobile_bottom_nav.slint");
    assert!(chat.find("message-list := MessageList").unwrap() < chat.find("top-chrome := VerticalLayout").unwrap());
    assert!(chat.contains("top-inset: top-chrome.height;") && chat.contains("bottom-inset: bottom-chrome.height;"));
    assert!(chat.contains("y: root.height - self.height;"));
    assert!(list.contains("padding-top: idx == 0 ? root.top-inset + Spacing.md : Spacing.none;"));
    assert!(list.contains("? root.bottom-inset + Spacing.md + Spacing.touch-target-min"));
    assert!(list.contains("(self.viewport-height - root.bottom-inset) + self.viewport-y"));
    assert!(list.contains("changed bottom-inset =>") && list.contains("root.is-at-bottom && !root.scrolling && root.jump-row < 0"));
    assert!(list.contains("y: root.height - root.bottom-inset - 60px;"));
    assert!(sidebar.find("Flickable {").unwrap() < sidebar.find("if !root.collapsed: GlassSurface {").unwrap());
    assert!(sidebar.contains("root.bottom-inset + Spacing.fab-size"));
    assert!(!app.contains("// Space held for the bottom tab bar."));
    for src in [include_str!("../../../ui/modals/friends.slint"), include_str!("../../../ui/calls/call_history_screen.slint")] {
        assert!(src.contains("floating-content: root.mobile-mode;"));
        assert!(src.contains("height: root.mobile-mode ? root.height : parent.height;"));
        assert!(src.contains("padding-bottom: root.mobile-mode ? root.bottom-inset + Spacing.md : Spacing.none;"));
    }
    assert!(material.contains("Appearance.glass-effects"));
    assert!(material.contains("blur-regular:") && material.contains("blur-thick:"));
    assert!(material.contains("<physical-length> glass-edge-width:"));
    assert!(nav.contains("background: root.active ? Colors.text-primary.with-alpha(0.12) : transparent;"));
}


/// DESIGN3: the row wrappers are just as important as the leaf delegates.
#[test]
fn repeated_chat_rows_never_sample_glass() {
    for (name, src) in all_slint_sources() {
        if name.ends_with("_list_item.slint")
            || matches!(name.as_str(), "chat/message_item.slint" | "sidebar/chat_list.slint" | "sidebar/custom_section_list.slint")
        {
            let code = src.lines().map(|line| line.split("//").next().unwrap_or("")).collect::<Vec<_>>().join("\n");
            for forbidden in ["GlassSurface", "Backdrop", "drop-shadow-"] {
                assert!(!code.contains(forbidden), "{name} adds {forbidden} to a repeated row");
            }
        }
    }
}

/// Chrome over the message list stops re-blurring the moving rows every frame
/// while the list scrolls, and keeps its at-rest material unchanged.
#[test]
fn chrome_over_the_list_pauses_backdrop_while_scrolling() {
    let glass = squash(include_str!("../../../ui/components/glass_surface.slint"));
    // Neutral values at 0 (no backdrop layer), the designed values at 1.
    assert!(glass.contains("property <float> sampling: root.backdrop-paused ? 0 : 1;"));
    assert!(glass.contains(": Materials.blur-regular) : 0px) * root.sampling;"));
    assert!(glass.contains("saturation: 1.0 + ((Appearance.glass-effects ? Materials.saturation : 1.0) - 1.0) * root.sampling;"));
    assert!(glass.contains("refraction: (Appearance.glass-effects ? Materials.refraction : 0.0) * root.sampling;"));
    // Stops at once, fades back in at rest (no pop), honours reduce-motion.
    assert!(glass.contains("duration: root.backdrop-paused || Appearance.reduce-motion ? 0ms : 160ms;"));
    let list = include_str!("../../../ui/chat/message_list.slint");
    assert!(list.contains("out property <bool> scrolling: false;"));
    assert!(list.contains("backdrop-paused: root.scrolling;"), "jump pill");
    let layout = include_str!("../../../ui/chat/chat_layout.slint");
    assert_eq!(
        layout.matches("backdrop-paused: message-list.scrolling;").count(),
        4,
        "header, pinned bar, composer notice and composer"
    );
    let header = include_str!("../../../ui/chat/chat_header.slint");
    assert_eq!(
        header.matches("HeaderButton {").count(),
        header.matches("backdrop-paused: root.backdrop-paused;").count(),
        "every header button samples the header's backdrop too"
    );
}

#[test]
fn fine_glass_rim_preserves_shared_childless_material() {
    let materials = include_str!("../../../ui/theme/materials.slint");
    let glass = include_str!("../../../ui/components/glass_surface.slint");
    assert!(materials.contains("<physical-length> glass-edge-width: 1phx;"), "rim must stay one physical pixel at every DPI");
    let rim = materials.split("<brush> glass-edge:").nth(1).unwrap().split(';').next().unwrap();
    assert!(rim.contains("Appearance.dark"), "rim must adapt to the theme");
    assert!(rim.contains("#ffffff00"), "rim must fade away between its highlights");
    assert!(!materials.contains("inner-edge") && !glass.contains("inner-edge"));
    assert_eq!(glass.matches("border-width:").count(), 1, "a second stroke makes the rim look thick");
    let private = glass.split("export component GlassSurface").next().unwrap();
    assert!(private.contains("component GlassMaterial inherits Rectangle"));
    assert!(!private.lines().any(|line| line.trim() == "@children"), "MEM1: material must remain childless");
    assert!(squash(glass).contains("GlassMaterial { x: 0; y: 0;"));
}

#[test]
fn secondary_chrome_exposes_its_shared_material() {
    let pinned = include_str!("../../../ui/chat/pinned_bar.slint");
    assert!(pinned.contains("inherits GlassSurface"));
    assert!(!pinned.contains("drop-shadow-"));
    for src in [
        include_str!("../../../ui/components/emoji_picker.slint"),
        include_str!("../../../ui/components/gif_picker.slint"),
        include_str!("../../../ui/chat/message_composer.slint"),
    ] {
        assert!(!src.contains("background: Colors.background;"), "opaque chrome masks its glass shell");
    }
    let reaction = include_str!("../../../ui/chat/reaction_picker.slint");
    assert!(reaction.contains("material-enabled: false;"), "embedded emoji picker must not stack glass");
    let list = include_str!("../../../ui/chat/message_list.slint");
    assert_eq!(list.matches("GlassSurface {").count(), 1, "only the jump capsule floats over the list");
    // The date and unread chips are row content: a plain tinted fill, never a
    // backdrop sample per row (glass_surface.slint:1).
    assert_eq!(list.matches("background: Materials.regular;").count(), 2, "date and unread chips");
    assert!(!list.contains("Colors.action-fill.with-alpha(0.97)"));
    let profile = include_str!("../../../ui/modals/user_profile.slint");
    assert!(profile.contains("height: parent.height - self.y;"));
    assert!(profile.contains("padding-top: root.mobile-mode ? profile-header.height + Spacing.sm * 2 : Spacing.none;"));
    let account = include_str!("../../../ui/settings/account_menu.slint");
    assert!(account.contains("floating-content: root.mobile-mode;"));
    assert!(account.contains("y: root.mobile-mode ? -menu-body.chrome-inset : 0px;"));
    assert!(!account.contains("absolute-position"), "content-sized menus must not derive their padding from their own layout position");
}

/// `src` with every whitespace run collapsed to one space, so a source pin
/// survives reindentation and rewrapping.
fn squash(src: &str) -> String {
    src.split_whitespace().collect::<Vec<_>>().join(" ")
}

// POLISH1: keep layout rhythm and ordinary controls in the shared primitives.
// These are source checks so they also cover screens absent from the headless
// fixture, without needing every account/channel/call state to be constructed.
fn raw_layout_spacing(src: &str) -> Vec<String> {
    let code = src.lines().map(|line| line.split("//").next().unwrap_or("")).collect::<Vec<_>>().join("\n");
    let mut findings = Vec::new();
    for (at, _) in code.match_indices(':') {
        let property = code[..at].trim_end().rsplit(|c: char| !(c.is_ascii_alphanumeric() || c == '-')).next().unwrap_or("");
        if !matches!(property, "spacing" | "padding" | "padding-left" | "padding-right" | "padding-top" | "padding-bottom") {
            continue;
        }
        let value = code[at + 1..].split(';').next().unwrap_or("");
        if value.match_indices("px").any(|(i, _)| i > 0 && value.as_bytes()[i - 1].is_ascii_digit()) {
            findings.push(format!("{property}: {}", value.trim()));
        }
    }
    findings
}

#[test]
fn screen_layout_spacing_uses_tokens() {
    assert!(!raw_layout_spacing("HorizontalLayout { spacing: 6px; padding-left: wide ? 8px : 0px; }").is_empty());
    assert!(raw_layout_spacing("// spacing: 8px;\nspacing: Spacing.sm; letter-spacing: 1px;").is_empty());
    let mut findings = Vec::new();
    for (name, src) in all_slint_sources() {
        if name.starts_with("theme/") { continue; }
        for property in raw_layout_spacing(&src) {
            findings.push(format!("ui/{name}: {property}"));
        }
    }
    assert!(findings.is_empty(), "Layout spacing must use Spacing tokens:\n{}", findings.join("\n"));
}

#[test]
fn screens_do_not_define_private_button_families() {
    let mut findings = Vec::new();
    for (name, src) in all_slint_sources() {
        if name.starts_with("components/") || name.starts_with("theme/") { continue; }
        let code = src.lines().map(|line| line.split("//").next().unwrap_or("")).collect::<Vec<_>>().join("\n");
        let words: Vec<_> = code.split_whitespace().collect();
        for pair in words.windows(2) {
            if pair[0] == "component" && pair[1].ends_with("Button") {
                findings.push(format!("ui/{name}: private {} — use Button, HeaderButton or IconAction", pair[1]));
            }
        }
        for import in code.split(';').filter(|part| part.contains("std-widgets.slint")) {
            if import.split(|c: char| !c.is_ascii_alphanumeric()).any(|word| word == "Button") {
                findings.push(format!("ui/{name}: platform Button bypasses the shared family"));
            }
        }
    }
    assert!(findings.is_empty(), "{}", findings.join("\n"));
}

#[test]
fn polish_shared_controls_and_rc28_regressions() {
    let sources = all_slint_sources();
    let source = |name: &str| sources.iter().find(|(path, _)| path == name).unwrap().1.as_str();
    let button = source("components/button.slint");
    assert!(button.contains("pressed: touch.pressed && !root.disabled && !root.loading"));
    assert!(button.contains("height: Spacing.button-height;") && button.contains("border-radius: self.height / 2;"));
    assert!(button.contains("overflow: elide;") && button.contains("accessible-action-default"));
    let toggle = source("components/toggle.slint");
    assert!(toggle.contains("height: Spacing.button-height;") && toggle.contains("background: Colors.on-action;"));
    let icons = source("components/icon_action.slint");
    assert!(icons.contains("max(Spacing.button-height, root.size)") && icons.contains("enabled: root.enabled;"));
    let checkbox = source("components/checkbox.slint");
    assert!(checkbox.contains("width: Spacing.button-height;") && checkbox.contains("height: Spacing.button-height;"));
    assert!(source("components/emoji_picker.slint").contains("viewport-width: categories.preferred-width;"));
    let picker = source("chat/reaction_picker.slint");
    assert!(picker.contains("max-picker-width: picker-shell.width;"));
    assert!(picker.contains("max-picker-height: max(0px, picker-shell.height - Spacing.touch-target-min - Spacing.stroke);"));
    // D3 and the notification-inbox portion of D9 were already migrated to
    // the shared shell before this pass; protect the title and that route.
    assert!(source("settings/account_menu.slint").contains("title: \"Profile\";"));
    assert!(source("server_records/screen.slint").contains("Modal {"));
    assert!(source("components/modal.slint").contains("title: root.title;"));
    // D4: a modal scrim must sort above the conversation's floating controls.
    assert!(source("chat/message_context_menu.slint").contains("z: 10;"));
    // D6: the narrow DM column must retain the word that conveys the status.
    assert!(source("chat/chat_header.slint").contains("root.mobile && root.is-dm ? \"Typing…\" : root.typing-indicator"));
    // D7: both cut edges are faded, and tabs share the ordinary Button family.
    let settings = source("settings/settings_screen.slint");
    assert!(settings.contains("if tabs.viewport-x < 0px: Rectangle"));
    assert!(settings.contains("if tabs.viewport-width + tabs.viewport-x > tabs.width: Rectangle"));
    assert!(settings.contains("variant: root.active-section == item.id ? \"primary\" : \"ghost\";"));
}

/// D10: every call entry point delegates duration to the core connected clock.
#[test]
fn call_duration_uses_core_connection_time() {
    let timers = include_str!("../src/bin_ui/features/calls/timers.rs");
    assert!(timers.contains("ui_ctx.call_active_call().await"));
    assert!(timers.contains("call.duration_seconds()"));
    assert!(!timers.contains("SystemTime") && !timers.contains("started_at_ns"));
    assert!(timers.contains("stop.load(Ordering::Relaxed)"), "a late snapshot must not restart a stopped timer");
    for source in [
        include_str!("../src/bin_ui/features/calls/outgoing.rs"),
        include_str!("../src/bin_ui/features/calls/incoming.rs"),
        include_str!("../src/bin_ui/features/calls/key_exchange.rs"),
    ] {
        assert!(source.contains("start_call_timer(&w, ui_ctx.clone())"));
        assert!(!source.contains("start_call_timer(&w, Some(") && !source.contains("start_call_timer(&w, None)"));
    }
    let active = include_str!("../src/bin_ui/features/calls/active.rs");
    assert!(active.contains("ctx.end_active_call().await"), "hangup duration must also come from core");
    let worker = include_str!("../../chatter-core/src/services/call_worker.rs");
    assert!(squash(worker).contains("if encryption_root_key == &[0u8; 32] { call.ring();"), "a DM room alone is not an answered call");
    assert!(worker.contains("derive_call_handle(state) == call && state.call.connected_at.is_none()"), "only the matching first answer starts the core clock");
}

/// Owner decision 2026-10-04: a Shield toggle on the sign-in and create-account
/// screens, so an account can be made and used over the onion service without
/// the server ever seeing the device's IP. It must be the chat list's Shield
/// control (HeaderButton + shield glyph), disabled where the build cannot
/// route over Tor, and must reach core's fail-closed `set_shield_before_login`.
#[test]
fn shield_toggle_on_sign_in_and_create_account() {
    use slint::ComponentHandle;
    use std::{cell::Cell, rc::Rc};

    for (page, src) in [
        ("login", include_str!("../../../ui/pages/login.slint")),
        ("register", include_str!("../../../ui/pages/register.slint")),
        ("link-device", include_str!("../../../ui/pages/link_device.slint")),
    ] {
        assert!(src.contains("ShieldOnboardingToggle {"), "the {page} page lost its Shield toggle");
    }
    let toggle = include_str!("../../../ui/anon_mode/shield_onboarding.slint");
    assert!(
        toggle.contains("HeaderButton {") && toggle.contains("icon: Icons.shield;"),
        "the sign-in Shield must reuse the chat-list Shield control"
    );
    let glue = include_str!("../src/bin_ui/features/auth/shield.rs");
    assert!(glue.contains("set_shield_available(ui_ctx.shield_available())"));
    assert!(glue.contains("ui_ctx.set_shield_before_login("));

    require_element_queries();
    let w = boot();
    let hits = Rc::new(Cell::new(0));
    let counter = hits.clone();
    w.global::<AnonMode>().on_toggle_before_login(move || counter.set(counter.get() + 1));

    for view in ["login", "register", "link-device"] {
        w.set_current_view(view.into());
        // No onion service in the build: offered, but disabled.
        w.global::<AnonMode>().set_shield_available(false);
        let off = testing::ElementHandle::find_by_accessible_label(
            &w,
            "Shield unavailable: this build has no Tor onion service",
        )
        .next()
        .unwrap_or_else(|| panic!("no Shield control on the {view} screen"));
        let before = hits.get();
        off.invoke_accessible_default_action();
        assert_eq!(hits.get(), before, "a disabled Shield must not toggle ({view})");

        w.global::<AnonMode>().set_shield_available(true);
        testing::ElementHandle::find_by_accessible_label(&w, "Shield off - tap to connect over Tor")
            .next()
            .unwrap_or_else(|| panic!("Shield not offered on the {view} screen"))
            .invoke_accessible_default_action();
        assert_eq!(hits.get(), before + 1, "the {view} Shield did not reach the toggle callback");
    }
}

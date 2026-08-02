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
/// `SLINT_EMIT_DEBUG_INFO=1` is set at build time. Without it every query
/// returns nothing, and the assertions here then report a UI that looks
/// structurally broken -- "ThreadPanel was never instantiated", "there are no
/// Rectangles" -- when the only thing wrong is the command line. Check the real
/// precondition first.
fn require_element_queries() {
    assert_eq!(
        env!("CHATTER_UI_SLINT_DEBUG_INFO"),
        "1",
        "the ElementHandle query API is inert: chatter-ui was built without \
         SLINT_EMIT_DEBUG_INFO=1, so every element query returns nothing and \
         nothing asserted below can mean anything. Run the UI suites with:\n\n    \
         ./scripts/test-ui.sh\n\n  or directly:\n\n    \
         SLINT_EMIT_DEBUG_INFO=1 cargo test -p chatter-ui --test ui_wiring --test ui_layout"
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
/// child (gated by active-modal=="settings") actually MATERIALIZES — a plain
/// `init_no_event_loop` never renders, so conditional/repeated sub-trees never
/// instantiate and the toggle can't be found. Then:
///   1. the accessible-label we added resolves to exactly one `switch` element,
///   2. its accessible-checked tracks the bound property (display wire), and
///   3. a REAL pointer click traverses the whole chain
///      (Toggle.toggled → SettingsScreen.read-receipts-changed → App →
///       MainWindow → Rust on_settings_read_receipts_changed) into chatter-core.
///
/// Modal-routing wire, proven dynamically. Setting the `active-modal` property
/// to "settings" must instantiate the conditional `SettingsScreen` sub-tree
/// (`ui/app.slint:2038  if active-modal == "settings": SettingsScreen`); clearing
/// it must tear the sub-tree down. If that `if`-binding were dead, the element
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

    // Open the settings modal via the routing property → conditional must appear.
    w.set_active_modal("settings".into());
    pump(&w);
    assert_eq!(
        count_settings(),
        1,
        "setting active-modal='settings' did NOT instantiate SettingsScreen — the \
         `if active-modal == \"settings\"` routing wire is dead"
    );

    // Close it → conditional must tear down.
    w.set_active_modal("none".into());
    pump(&w);
    assert_eq!(count_settings(), 0, "clearing active-modal did NOT remove SettingsScreen");
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
// F-UI-3 (RESOLVED): friends-modal user search is no longer a dead wire.
//
//  L1 (DYNAMIC): `on_friends_search_users` is now a registered handler — invoking
//      the generated callback runs the REAL production handler (from
//      wire_friends_modal) without panicking. Before, the callback fired into the
//      void (no `.on_friends_search_users`). Offline (no server) the lookup yields
//      nothing; we assert the WIRE runs, not a server result.
//  L2 (STATIC): the handler reaches `ChatterClient::search_users` and populates the
//      bound `friends-search-results` model. The real taken-vs-unknown effect is
//      proven end-to-end by validated.rs::v31.
// ============================================================================
#[test]
fn friends_search_users_is_wired_to_core() {
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

    // Register the REAL friends-modal handlers, then fire the search callback.
    chatter_ui::bin_ui::features::modals::wire_friends_modal(&w, &client);
    // Must not panic: the callback is wired to a Rust handler (dead wire otherwise).
    w.invoke_friends_search_users("someone.4242".into());

    // ---- L2 static: handler reaches core + populates the results model -------
    let src = include_str!("../src/bin_ui/features/modals/friends/friends_list.rs");
    assert!(
        src.contains("on_friends_search_users"),
        "friends_list.rs must register the search handler"
    );
    let idx = src.find("on_friends_search_users").unwrap();
    let after = &src[idx..];
    assert!(
        after.contains(".search_users("),
        "the search handler must call ChatterClient::search_users (dead wire otherwise)"
    );
    assert!(
        after.contains("set_friends_search_results"),
        "the search handler must populate the bound friends-search-results model"
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
// AVATAR CHANGE (RESOLVED for desktop): the picker now persists the picked bytes.
//
// The avatar picker is behind a native file dialog, so it can't be driven
// headlessly. L2 (STATIC): `on_edit_profile_change_avatar` calls
// `set_avatar_bytes` on the picked data, so Save (`update_user_profile`) uploads
// THOSE bytes as `encrypted_avatar`. The full upload+server-store+decrypt
// round-trip is proven by validated.rs::v32.
// ============================================================================
#[test]
fn edit_profile_avatar_change_persists_picked_bytes() {
    let src = include_str!("../src/bin_ui/features/modals/profile.rs");
    let idx = src.find("on_edit_profile_change_avatar").expect("avatar handler present");
    let after = &src[idx..];
    assert!(
        after.contains(".set_avatar_bytes(&data)") || after.contains(".set_avatar_bytes("),
        "the avatar picker must persist the picked bytes via set_avatar_bytes \
         (previously only wrote a write-never-read PENDING_AVATAR_PATH + preview)"
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
        slint_src.contains(r#"accessible-label: "Reply in thread""#),
        "the message hover actions must include a Reply-in-thread button so a \
         message with zero replies can still START a thread"
    );
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
#[cfg(feature = "jarvis")]
#[test]
fn sidebar_injects_ai_channel_and_never_autoselects_it() {
    no_keyring();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    let tmp = tempfile::tempdir().unwrap();
    let client = chatter_core::client::ChatterClient::builder()
        .data_dir(tmp.path().to_string_lossy().to_string())
        .build()
        .expect("offline ChatterClient::build");

    let channels = client.sidebar_channels();
    assert!(!channels.is_empty(), "AI channel missing from sidebar (local AI defaults on)");
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
        binding.contains("channel-type == \"public\""),
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
        broadcast.contains("channel-type == \"public\""),
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
    assert_eq!(w.get_active_modal().as_str(), "friends", "add-friends did not open friends modal");
    assert_eq!(w.get_friends_modal_tab().as_str(), "find", "add-friends did not land on find tab");

    w.set_active_modal("none".into());
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
    w.set_forward_source_sender_id(peer.into());
    w.set_forward_source_timestamp("123456789".into());
    w.set_active_modal("forward-message".into());
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

    w.set_active_modal("settings".into());
    assert!(
        chatter_ui::bin_ui::features::back::handle_back(&w),
        "back with a modal open must report the press as consumed in-app"
    );
    assert_eq!(
        w.get_active_modal().as_str(),
        "none",
        "back did NOT close the open modal — the Settings trap is still live"
    );

    // Through the Slint callback too (that is what the FocusScope invokes).
    w.set_active_modal("create-channel".into());
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
    w.set_active_modal("settings".into());

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
    w.set_active_modal("settings".into());

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
        "settings",
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
    w.set_active_modal("settings".into());

    w.invoke_back_pressed();

    assert!(
        !w.get_selection_mode(),
        "back did not exit message multi-select — it is the innermost layer and \
         must pop first"
    );
    assert_eq!(
        w.get_active_modal().as_str(),
        "settings",
        "back popped TWO layers at once: selection mode AND the modal. One press, \
         one layer"
    );

    // Second press now takes the modal.
    w.invoke_back_pressed();
    assert_eq!(w.get_active_modal().as_str(), "none", "the second back press did not close the modal");
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

    let channels = include_str!("../../../ui/sidebar/channel_list.slint");
    let dms = include_str!("../../../ui/sidebar/dm_list.slint");
    assert!(
        channels.contains("root.channel-context-menu-requested(item.channel-id, item.is-pinned, x, y)"),
        "the channel row must carry its pin state into the menu"
    );
    assert!(
        dms.contains("root.dm-context-menu-requested(item.channel-id, item.is-pinned, x, y)"),
        "the DM row must carry its pin state into the menu"
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
        list.contains("conversations: [SectionConversationData]"),
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
        sidebar[idx..idx + 600].contains("mobile-mode: root.mobile-mode"),
        "the folder list must be told whether it is being touched"
    );

    let list = include_str!("../../../ui/sidebar/custom_section_list.slint");
    assert!(
        list.contains("height: root.mobile-mode ? Spacing.touch-target-min : 36px"),
        "folder conversation rows must meet the touch minimum"
    );
    assert!(
        list.contains("height: root.mobile-mode ? Spacing.touch-target-min : 32px"),
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
        layout.contains(r#"if root.identity-change-visible && root.channel-type == "dm": IdentityChangeBar"#),
        "the bar must render inside the conversation, gated on channel-type so a \
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

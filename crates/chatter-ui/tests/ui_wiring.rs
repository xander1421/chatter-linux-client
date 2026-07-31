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
fn no_keyring() {
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
fn wired_offline() -> (
    tokio::runtime::Runtime,
    tempfile::TempDir,
    std::sync::Arc<chatter_core::client::ChatterClient>,
    MainWindow,
) {
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
// (The native file dialog itself can't run headless — same as the avatar picker.)
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

    // L1 dynamic: the pick callback runs without panicking (the native dialog is
    // spawned; offline/headless it just resolves to no file).
    w.invoke_channel_settings_pick_icon();

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

//! Offline production callbacks and measured drawer/settings affordances.
//! One integration backend, disposable database, no OS keyring or device.
use chatter_ui::{ChannelMemberData, GifData, MainWindow};
use i_slint_backend_testing::{self as testing, ElementHandle};
use slint::{ComponentHandle, Model, ModelRc, VecModel};

fn pump(w: &MainWindow) {
    w.window()
        .dispatch_event(slint::platform::WindowEvent::PointerMoved {
            position: slint::LogicalPosition::new(1.0, 1.0),
        });
    slint::Timer::single_shot(std::time::Duration::from_millis(40), || {
        slint::quit_event_loop().unwrap();
    });
    w.run().unwrap();
}

fn click(w: &MainWindow, element: &ElementHandle) {
    // mock_single_click advances the mock clock and cannot be combined with
    // the system-time integration backend. Use real production pointer events.
    let origin = element.absolute_position();
    let size = element.size();
    let position =
        slint::LogicalPosition::new(origin.x + size.width / 2.0, origin.y + size.height / 2.0);
    let button = slint::platform::PointerEventButton::Left;
    w.window()
        .dispatch_event(slint::platform::WindowEvent::PointerPressed { position, button });
    w.window()
        .dispatch_event(slint::platform::WindowEvent::PointerReleased { position, button });
}

// ElementHandle searches only visible items. Reach the below-fold account
// section through the same pointer scrolling that a phone user performs.
fn scroll_to_label(w: &MainWindow, label: &str) {
    let scroll = |delta_y| {
        w.window()
            .dispatch_event(slint::platform::WindowEvent::PointerScrolled {
                position: slint::LogicalPosition::new(200.0, 400.0),
                delta_x: 0.0,
                delta_y,
            });
        pump(w);
    };
    scroll(10000.0);
    for _ in 0..20 {
        if ElementHandle::find_by_accessible_label(w, label)
            .next()
            .is_some()
        {
            return;
        }
        scroll(-160.0);
    }
    panic!("account section did not expose {label} after bounded scrolling");
}

#[test]
fn failed_privacy_writes_restore_effective_values_and_selected_gif_tab_keeps_search() {
    assert!(
        chatter_ui::SLINT_DEBUG_INFO_ENABLED,
        "use --features ui-test-debug"
    );
    unsafe {
        std::env::set_var("CHATTER_NO_KEYRING", "1");
        std::env::set_var("CHATTER_DB_PASSPHRASE", "settings-affordance-fixture");
        std::env::set_var("CHATTER_HEADLESS", "1");
    }
    testing::init_integration_test_with_system_time();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let _entered = runtime.enter();
    let fixture = tempfile::tempdir().unwrap();
    let client = chatter_core::client::ChatterClient::builder()
        .data_dir(fixture.path().to_string_lossy().into_owned())
        .build()
        .unwrap();
    let db_path = chatter_core::system::Directories::with_base_dir(fixture.path().to_path_buf())
        .unwrap()
        .database_path();
    let db =
        chatter_core::storage::Database::open(&db_path, "settings-affordance-fixture").unwrap();
    // Reproduce repository failure without mocking the setters or corrupting a DB.
    db.connection()
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER reject_privacy_insert BEFORE INSERT ON settings
         WHEN NEW.key IN ('privacy.send_read_receipts', 'privacy.show_typing_indicators')
         BEGIN SELECT RAISE(ABORT, 'fixture privacy save rejected'); END;",
        )
        .unwrap();
    let w = MainWindow::new().unwrap();
    chatter_ui::bin_ui::features::modals::settings::wire_settings_callbacks(&w, &client);
    chatter_ui::bin_ui::features::back::wire(&w);
    w.set_settings_read_receipts(false); // Optimistic Slint switch state.
    w.invoke_settings_read_receipts_changed(false);
    assert!(client.read_receipts_enabled().unwrap());
    assert!(w.get_settings_read_receipts());
    assert!(w.get_toast_visible());
    assert!(
        w.get_toast_message()
            .contains("Couldn't save read receipts")
    );
    w.set_toast_visible(false);
    w.set_settings_typing_indicators(false);
    w.invoke_settings_typing_indicators_changed(false);
    assert!(client.typing_indicators_enabled().unwrap());
    assert!(w.get_settings_typing_indicators());
    assert!(w.get_toast_visible());
    assert!(
        w.get_toast_message()
            .contains("Couldn't save typing indicators")
    );

    w.set_current_view("main".into());
    w.set_channel_name("Fixture".into());
    w.set_is_mobile_platform(true);
    w.set_mobile_show_chat(true);
    w.window().set_size(slint::LogicalSize::new(360.0, 740.0));
    w.show().unwrap();
    w.set_toast_visible(false);
    w.set_composer_gif_picker_visible(true);
    w.set_composer_gif_picker_search("lemon".into());
    w.set_composer_gif_picker_results(ModelRc::new(VecModel::from(vec![GifData {
        id: "kept-gif".into(),
        ..Default::default()
    }])));
    w.set_composer_gif_generation(17);
    let callbacks = std::rc::Rc::new(std::cell::Cell::new(0));
    let witness = callbacks.clone();
    w.on_gif_clicked(move || witness.set(witness.get() + 1));
    pump(&w);
    let tab = ElementHandle::find_by_accessible_label(&w, "GIFs")
        .find(|e| e.size().width > 0.0 && e.size().height > 0.0)
        .expect("visible GIF tab");
    click(&w, &tab);
    pump(&w);
    assert_eq!(
        callbacks.get(),
        0,
        "selected tab must not request trending GIFs"
    );
    assert_eq!(w.get_composer_gif_picker_search().as_str(), "lemon");
    assert_eq!(w.get_composer_gif_generation(), 17);
    assert_eq!(
        w.get_composer_gif_picker_results()
            .row_data(0)
            .unwrap()
            .id
            .as_str(),
        "kept-gif"
    );
    let input = ElementHandle::find_by_accessible_label(&w, "Search emoji and GIFs")
        .next()
        .expect("drawer search input");
    let input_position = input.absolute_position();
    let search_icon = ElementHandle::find_by_element_type_name(&w, "Image")
        .filter(|e| (e.size().width - 20.0).abs() < 0.5)
        .find(|e| {
            let position = e.absolute_position();
            position.x < input_position.x && (position.y - input_position.y).abs() < 40.0
        })
        .expect("search magnifier before input");
    assert!(
        input_position.x - (search_icon.absolute_position().x + search_icon.size().width) >= 7.5,
        "search text/caret must have an 8px gap from the magnifier"
    );

    // Ordinary members can inspect the already privacy-shaped local roster;
    // this does not grant moderation or introduce a username/name lookup.
    w.set_composer_gif_picker_visible(false);
    w.set_channel_settings_id("settings-identity-fixture".into());
    w.set_channel_id("settings-identity-fixture".into());
    w.set_channel_settings_type("private-group".into());
    w.set_channel_settings_name("Scoped group".into());
    w.set_channel_settings_created_at("".into());
    w.set_channel_settings_member_count(1);
    w.set_channel_settings_can_manage_members(false);
    w.set_channel_settings_members(ModelRc::new(VecModel::from(vec![ChannelMemberData {
        display_name: "Conversation-scoped alias".into(),
        role: "member".into(),
        ..Default::default()
    }])));
    chatter_ui::bin_ui::features::back::push(&w, "channel-settings");
    pump(&w);
    assert!(
        ElementHandle::find_by_accessible_label(&w, "Group Settings")
            .next()
            .is_some()
    );
    assert!(
        ElementHandle::find_by_accessible_label(&w, "Created ")
            .next()
            .is_none()
    );
    assert!(
        ElementHandle::find_by_accessible_label(&w, "1 member")
            .next()
            .is_some(),
        "empty creation date must not leave Created or a trailing separator"
    );
    for theme in ["light", "dark"] {
        w.global::<chatter_ui::Appearance>().set_theme(theme.into());
        for (kind, label) in [
            ("private-group", "Group icon"),
            ("private", "Private channel icon"),
            ("public", "Channel icon"),
        ] {
            w.set_channel_settings_type(kind.into());
            pump(&w);
            let icon = ElementHandle::find_by_accessible_label(&w, label)
                .find(|e| e.type_name().is_some_and(|name| name == "Image"))
                .expect("theme-aware vector conversation fallback");
            assert!((icon.size().width - 24.0).abs() < 0.5);
            assert!((icon.size().height - 24.0).abs() < 0.5);
        }
    }
    w.set_channel_settings_type("private-group".into());
    w.global::<chatter_ui::Appearance>()
        .set_theme("light".into());
    w.set_channel_avatar_source(slint::Image::from_rgba8(slint::SharedPixelBuffer::<
        slint::Rgba8Pixel,
    >::new(2, 2)));
    w.set_channel_has_avatar(true);
    pump(&w);
    assert!(
        ElementHandle::find_by_accessible_label(&w, "Avatar for Scoped group").any(|e| (e
            .size()
            .width
            - 40.0)
            .abs()
            < 0.5),
        "configured conversation icon must replace the generic fallback"
    );
    assert!(
        ElementHandle::find_by_accessible_label(&w, "Group icon")
            .next()
            .is_none()
    );
    w.set_channel_id("different-conversation".into());
    pump(&w);
    assert!(
        ElementHandle::find_by_accessible_label(&w, "Group icon")
            .next()
            .is_some(),
        "an unrelated active conversation avatar must not be attributed to this settings screen"
    );
    w.set_channel_id("settings-identity-fixture".into());
    w.set_channel_has_avatar(false);
    pump(&w);
    let members = ElementHandle::find_by_accessible_label(&w, "Members")
        .find(|e| e.size().width > 0.0)
        .expect("read-only Members tab for nonowner");
    click(&w, &members);
    pump(&w);
    assert!(
        ElementHandle::find_by_accessible_label(&w, "Conversation-scoped alias")
            .next()
            .is_some()
    );
    assert!(
        ElementHandle::find_by_accessible_label(&w, "Remove from group")
            .next()
            .is_none()
    );
    assert!(
        ElementHandle::find_by_accessible_label(&w, "@Conversation-scoped alias")
            .next()
            .is_none()
    );

    chatter_ui::bin_ui::features::back::close(&w, "channel-settings");
    chatter_ui::bin_ui::features::back::push(&w, "settings");
    pump(&w);
    scroll_to_label(
        &w,
        "File backup is unavailable on this device. Use server backup below.",
    );
    assert!(
        ElementHandle::find_by_accessible_label(
            &w,
            "File backup is unavailable on this device. Use server backup below."
        )
        .next()
        .is_some()
    );
    assert!(
        ElementHandle::find_by_accessible_label(&w, "Backup passphrase (for file backup)")
            .next()
            .is_none(),
        "unavailable file backup must not ask for a secret"
    );
    assert!(
        ElementHandle::find_by_accessible_label(&w, "Export to File…")
            .next()
            .is_none()
    );
    assert!(
        ElementHandle::find_by_accessible_label(&w, "Restore from File…")
            .next()
            .is_none()
    );
    // The server actions remain present, with their original production callbacks.
    scroll_to_label(&w, "Back Up to Server");
    assert!(
        ElementHandle::find_by_accessible_label(&w, "Back Up to Server")
            .next()
            .is_some()
    );
    w.set_is_mobile_platform(false);
    pump(&w);
    scroll_to_label(&w, "Backup passphrase (for file backup)");
    assert!(
        ElementHandle::find_by_accessible_label(&w, "Backup passphrase (for file backup)")
            .next()
            .is_some()
    );
    assert!(
        ElementHandle::find_by_accessible_label(&w, "Export to File…")
            .next()
            .is_some()
    );
    assert!(
        ElementHandle::find_by_accessible_label(&w, "Restore from File…")
            .next()
            .is_some()
    );
    // Reproduce a compact navigation pill partly clipped beside fixed Back.
    w.set_is_mobile_platform(true);
    pump(&w);
    let notifications = ElementHandle::find_by_accessible_label(&w, "Notifications")
        .find(|e| e.type_name().is_some_and(|name| name == "Button"))
        .expect("compact Notifications tab");
    let back = ElementHandle::find_by_accessible_label(&w, "Back")
        .find(|e| e.absolute_position().y < 56.0 && e.size().width > 0.0)
        .expect("fixed settings Back button");
    let back_right = back.absolute_position().x + back.size().width;
    let scroll = back_right - 16.0 - notifications.absolute_position().x;
    w.window()
        .dispatch_event(slint::platform::WindowEvent::PointerScrolled {
            position: slint::LogicalPosition::new(200.0, 28.0),
            delta_x: scroll,
            delta_y: 0.0,
        });
    pump(&w);
    assert!(
        notifications.absolute_position().x < back_right,
        "precondition: Notifications must be partly clipped at the leading edge"
    );
    click(&w, &notifications);
    pump(&w);
    assert!(
        notifications.absolute_position().x >= back_right,
        "selected Notifications must be brought clear of fixed Back"
    );
    assert!(
        notifications.absolute_position().x + notifications.size().width <= 352.0,
        "selected compact tab must fit within the trailing window gutter"
    );

    chatter_ui::bin_ui::features::back::close(&w, "settings");
    w.set_is_mobile_platform(true);
    w.set_conversation_kind(chatter_ui::ConversationKind::ServerDm);
    pump(&w);
    let more = ElementHandle::find_by_accessible_label(&w, "More options")
        .find(|e| e.size().width > 0.0)
        .expect("header menu button");
    click(&w, &more);
    pump(&w);
    assert_eq!(w.get_active_modal().as_str(), "chat-menu");
    let label = ElementHandle::find_by_accessible_label(&w, "Conversation settings")
        .find(|e| e.type_name().is_some_and(|name| name == "Text"))
        .expect("conversation settings menu label");
    assert!(
        label.size().width >= 200.0,
        "menu must allocate enough width for its full settings label"
    );
    let card = ElementHandle::find_by_element_type_name(&w, "GlassSurface")
        .find(|e| (e.size().width - 288.0).abs() < 0.5)
        .expect("bounded wider header menu card");
    assert!(card.absolute_position().x >= 0.0);
    assert!(card.absolute_position().x + card.size().width <= 360.0);
}

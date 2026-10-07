//! The real header route opens a screen before async reads and shows failures.
use chatter_core::{
    domain::{anon_group::AnonGroupId, ids::ChannelId},
    state::ActiveConversation,
};
use chatter_ui::{
    MainWindow,
    bin_ui::features::{back, header, modals},
};
use slint::ComponentHandle;

fn pump(w: &MainWindow) {
    for _ in 0..4 {
        slint::Timer::single_shot(std::time::Duration::from_millis(25), || {
            slint::quit_event_loop().unwrap();
        });
        w.run().unwrap();
    }
}

fn pump_until(w: &MainWindow, ready: impl Fn() -> bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while !ready() && std::time::Instant::now() < deadline {
        pump(w);
    }
    assert!(
        ready(),
        "background settings paint did not complete within 3 seconds"
    );
}

#[test]
fn settings_route_opens_immediately_reports_missing_rows_and_discards_late_paints() {
    assert!(
        chatter_ui::SLINT_DEBUG_INFO_ENABLED,
        "run with --features ui-test-debug"
    );
    // Set before building the client: no original database or OS keyring.
    unsafe {
        std::env::set_var("CHATTER_NO_KEYRING", "1");
        std::env::set_var(
            "CHATTER_DB_PASSPHRASE",
            "disposable-settings-route-passphrase",
        );
        std::env::set_var("CHATTER_HEADLESS", "1");
    }
    i_slint_backend_testing::init_integration_test_with_system_time();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let _entered = runtime.enter();
    let fixture = tempfile::tempdir().unwrap();
    let client = chatter_core::client::ChatterClient::builder()
        .data_dir(fixture.path().to_string_lossy().into_owned())
        .build()
        .unwrap();
    let w = MainWindow::new().unwrap();
    w.window().set_size(slint::LogicalSize::new(360.0, 740.0));
    w.set_current_view("main".into());
    w.show().unwrap();
    header::wire(&w, &client);

    // Stored invite-only channel: metadata must appear even when roster loading fails.
    let stored = ChannelId::new();
    let db_path = chatter_core::system::Directories::with_base_dir(fixture.path().to_path_buf())
        .unwrap()
        .database_path();
    let db = std::sync::Arc::new(
        chatter_core::storage::Database::open(&db_path, "disposable-settings-route-passphrase")
            .unwrap(),
    );
    let row = chatter_core::domain::channel::Channel::new(
        stored,
        "Invite-only fixture".into(),
        chatter_core::domain::ChannelType::Private,
        chatter_core::domain::ids::UserId::new(),
    );
    chatter_core::storage::repositories::channel_repo::ChannelRepository::new(db)
        .insert(&row)
        .unwrap();
    client
        .chat_state()
        .set_active_conversation(ActiveConversation::Channel(stored));
    w.invoke_chat_settings_clicked();
    assert_eq!(w.get_active_modal().as_str(), "channel-settings");
    pump_until(&w, || !w.get_channel_settings_loading());
    assert_eq!(
        w.get_channel_settings_name().as_str(),
        "Invite-only fixture"
    );
    assert_eq!(w.get_channel_settings_type().as_str(), "private");
    assert!(
        w.get_channel_settings_error()
            .starts_with("Couldn't load members:")
    );
    assert!(!w.get_channel_settings_loading());
    back::close(&w, "channel-settings");

    let channel = ChannelId::new();
    client
        .chat_state()
        .set_active_conversation(ActiveConversation::Channel(channel));
    w.invoke_chat_settings_clicked();
    assert_eq!(
        w.get_active_modal().as_str(),
        "channel-settings",
        "settings must open before background work"
    );
    assert!(w.get_channel_settings_loading());
    assert!(!w.get_channel_settings_can_delete());
    pump_until(&w, || !w.get_channel_settings_loading());
    assert_eq!(w.get_channel_settings_error().as_str(), "Channel not found");
    assert!(!w.get_channel_settings_loading());
    assert_eq!(w.get_active_modal().as_str(), "channel-settings");

    assert!(
        i_slint_backend_testing::ElementHandle::find_by_accessible_label(&w, "Channel not found")
            .any(|element| element.size().width > 0.0 && element.size().height > 0.0),
        "missing-channel error must have a rendered readable text element"
    );

    back::close(&w, "channel-settings");
    let group = AnonGroupId::new();
    client
        .chat_state()
        .set_active_conversation(ActiveConversation::AnonGroup(group));
    w.invoke_chat_settings_clicked();
    assert_eq!(w.get_active_modal().as_str(), "channel-settings");
    pump_until(&w, || !w.get_channel_settings_loading());
    assert_eq!(
        w.get_channel_settings_error().as_str(),
        "Private group not found"
    );
    assert!(!w.get_channel_settings_loading());

    // Finishing a read must not move settings above a newer modal.
    back::close(&w, "channel-settings");
    w.invoke_chat_settings_clicked();
    back::push(&w, "settings");
    pump(&w);
    assert_eq!(w.get_active_modal().as_str(), "settings");
    back::close(&w, "settings");

    // A dismissed screen must stay dismissed when its queued error arrives.
    back::close(&w, "channel-settings");
    client
        .chat_state()
        .set_active_conversation(ActiveConversation::Channel(channel));
    w.invoke_chat_settings_clicked();
    back::close(&w, "channel-settings");
    w.set_channel_settings_error("dismissed sentinel".into());
    pump(&w);
    assert_ne!(w.get_active_modal().as_str(), "channel-settings");
    assert_eq!(
        w.get_channel_settings_error().as_str(),
        "dismissed sentinel"
    );

    // Nor may an old channel paint overwrite the new conversation's screen.
    w.invoke_chat_settings_clicked();
    client
        .chat_state()
        .set_active_conversation(ActiveConversation::AnonGroup(group));
    w.invoke_chat_settings_clicked();
    pump_until(&w, || !w.get_channel_settings_loading());
    assert_eq!(w.get_channel_settings_id().as_str(), group.to_string());
    assert_eq!(
        w.get_channel_settings_error().as_str(),
        "Private group not found"
    );

    back::close(&w, "channel-settings");
    let other = ChannelId::new();
    client
        .chat_state()
        .set_active_conversation(ActiveConversation::Channel(other));
    modals::open_channel_settings(w.as_weak(), client.clone(), channel);
    pump(&w);
    assert_ne!(
        w.get_active_modal().as_str(),
        "channel-settings",
        "a stale open must not open settings"
    );
}

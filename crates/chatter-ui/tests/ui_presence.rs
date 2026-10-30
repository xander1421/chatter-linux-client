//! Live presence updates must refresh the profile modal's text and dot.
use chatter_ui::MainWindow;
use i_slint_backend_testing as testing;
use slint::ComponentHandle;

/// A live peer event must replace the text which Slint prefers over its
/// presence fallback, even when the profile is open outside a DM.
#[test]
fn profile_presence_label_follows_live_presence() {
    use chatter_core::{
        client::ChatterClient,
        domain::{
            ids::UserId,
            user::{PresenceStatus, Profile},
        },
        events::DomainEvent,
        network::connection::ConnectionState,
        storage::{Database, repositories::UserRepository},
    };

    // Keep this isolated fixture off the desktop keyring.
    unsafe {
        std::env::set_var("CHATTER_NO_KEYRING", "1");
        std::env::set_var("CHATTER_DB_PASSPHRASE", "ui-wiring-harness-passphrase");
    }
    testing::init_integration_test_with_system_time();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let _runtime = runtime.enter();
    let dir = tempfile::tempdir().unwrap();
    let client = ChatterClient::builder()
        .data_dir(dir.path().to_string_lossy().into_owned())
        .build()
        .unwrap();
    client.set_offline_connection_state_for_testing(ConnectionState::Connected);
    let db_path = chatter_core::system::Directories::with_base_dir(dir.path().to_path_buf())
        .unwrap()
        .database_path();
    let repo = UserRepository::new(std::sync::Arc::new(
        Database::open(&db_path, "ui-wiring-harness-passphrase").unwrap(),
    ));
    let peer = UserId::new();
    repo.ensure_user_exists(&peer).unwrap();
    repo.upsert_profile(&Profile::new(peer, "Presence fixture".into()))
        .unwrap();
    let w = MainWindow::new().unwrap();
    let modal = w.global::<chatter_ui::ModalHostState>();
    modal.set_profile_user_id(peer.to_string().into());
    modal.set_profile_scoped_member(false);
    modal.set_profile_last_seen("last seen 16 min ago".into());

    for (presence, expected) in [
        (PresenceStatus::Online, "online"),
        (PresenceStatus::Away, "away"),
        (PresenceStatus::DoNotDisturb, "do not disturb"),
        (PresenceStatus::Offline, "last seen just now"),
    ] {
        let now = chrono::Utc::now();
        // The stream persists before delivering the domain event.
        repo.set_presence(&peer, presence, Some(now.timestamp()))
            .unwrap();
        runtime.block_on(
            chatter_ui::bin_ui::features::auth::inbox::domain_handler::handle_domain_event(
                DomainEvent::UserPresenceChanged {
                    user_id: peer,
                    presence,
                    timestamp: now.timestamp_nanos_opt().unwrap() as u64,
                },
                &w.as_weak(),
                &client,
            ),
        );
        slint::Timer::single_shot(std::time::Duration::from_millis(10), || {
            slint::quit_event_loop().unwrap();
        });
        // Pump the queued handler without show/hide, which would enqueue an
        // extra Quit when the last visible window closes between transitions.
        slint::run_event_loop().unwrap();
        assert_eq!(modal.get_profile_presence(), presence.as_str());
        assert_eq!(modal.get_profile_last_seen(), expected);
    }
}

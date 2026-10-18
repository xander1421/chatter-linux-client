//! Persisted reaction presentation through the real production UI helper.
use chatter_core::client::ChatterClient;
use chatter_core::domain::anon_dm::AnonDmId;
use chatter_core::domain::anon_group::AnonGroupId;
use chatter_core::domain::ids::{ChannelId, UserId};
use chatter_core::domain::message::{ConversationType, Message};
use chatter_core::state::ActiveConversation;
use chatter_ui::bin_ui::features::chat::refresh_reaction_in_ui;
use chatter_ui::{MainWindow, MessageItemData, ReactionEntry};
use slint::{Model, ModelRc, VecModel};

#[test]
fn reaction_snapshot_repairs_stale_counts_and_ignores_duplicates_and_switched_completions() {
    // Run only with the coordinator's isolated no-keyring harness; the fixture
    // also opts out of native keyring access before client construction.
    unsafe {
        std::env::set_var("CHATTER_NO_KEYRING", "1");
        std::env::set_var("CHATTER_DB_PASSPHRASE", "reaction-fixture-passphrase");
    }
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _entered = rt.enter();
    i_slint_backend_testing::init_no_event_loop();
    let dir = tempfile::tempdir().unwrap();
    let client = ChatterClient::builder()
        .data_dir(dir.path().to_string_lossy().to_string())
        .build()
        .unwrap();
    let window = MainWindow::new().unwrap();
    let own = UserId::new();
    let author = UserId::new();
    let second_reactor = UserId::new();
    client
        .auth_state()
        .set_logged_in(own, 1, "reaction fixture".into());
    chatter_core::storage::repositories::settings_repo::SettingsRepository::new(
        client.messages().database(),
    )
    .set_string(
        chatter_core::storage::repositories::settings_repo::keys::CURRENT_USER_ID,
        &own.to_string(),
    )
    .unwrap();
    let scopes = [
        (
            ActiveConversation::Channel(ChannelId::new()),
            ConversationType::Channel,
        ),
        (
            ActiveConversation::Channel(ChannelId::new()),
            ConversationType::AnonGroup,
        ),
        (
            ActiveConversation::ServerDm {
                peer_id: UserId::new(),
            },
            ConversationType::ServerDm,
        ),
        (
            ActiveConversation::AnonDm(AnonDmId::new()),
            ConversationType::AnonDm,
        ),
        (
            ActiveConversation::AnonGroup(AnonGroupId::new()),
            ConversationType::AnonGroup,
        ),
    ];
    for (scope, kind) in scopes {
        let sent_at = 1000;
        let target = Message::new_text(
            scope.channel_id().unwrap(),
            kind.clone(),
            Some(author),
            "target".into(),
            sent_at,
        );
        client.messages().store_message_local(&target).unwrap();
        let other_scope = chatter_core::domain::reaction::ReactionScope::new(
            scope.channel_id().unwrap(),
            if kind == ConversationType::ServerDm {
                ConversationType::AnonDm
            } else {
                ConversationType::ServerDm
            },
        );
        client
            .messages()
            .store_message_local(&Message::new_text(
                other_scope.id,
                other_scope.kind.clone(),
                Some(author),
                "other kind same author and clock".into(),
                sent_at,
            ))
            .unwrap();
        client
            .messages()
            .record_reaction(&other_scope, &author, sent_at, &second_reactor, "👍", false)
            .unwrap();
        client.chat_state().set_active_conversation(scope);
        let mut row = MessageItemData::default();
        row.sender_id = author.to_string().into();
        row.timestamp_ns = sent_at.to_string().into();
        row.has_reactions = true;
        row.reactions = ModelRc::new(VecModel::from(vec![ReactionEntry {
            emoji: "👍".into(),
            count: 99,
            own: false,
        }]));
        row.reaction_summary = "stale".into();
        window.set_messages(ModelRc::new(VecModel::from(vec![row])));
        client
            .messages()
            .record_reaction(
                &chatter_core::domain::reaction::ReactionScope::new(
                    scope.channel_id().unwrap(),
                    kind.clone(),
                ),
                &author,
                sent_at,
                &own,
                "👍",
                false,
            )
            .unwrap();
        chatter_ui::bin_ui::features::chat::refresh_scoped_reaction_in_ui(
            &window,
            &client,
            scope,
            &other_scope,
            &author,
            sent_at,
        );
        assert_eq!(
            window
                .get_messages()
                .row_data(0)
                .unwrap()
                .reactions
                .row_data(0)
                .unwrap()
                .count,
            99,
            "an event from another kind cannot repair or overwrite this bubble"
        );
        refresh_reaction_in_ui(&window, &client, scope, &author, sent_at);
        assert_eq!(
            window
                .get_messages()
                .row_data(0)
                .unwrap()
                .reactions
                .row_data(0)
                .unwrap()
                .count,
            1
        );
        assert!(
            window
                .get_messages()
                .row_data(0)
                .unwrap()
                .reactions
                .row_data(0)
                .unwrap()
                .own,
            "persisted own add highlights the pill"
        );
        let persisted = client
            .messages()
            .get_message_local(&target.id)
            .unwrap()
            .unwrap();
        let initial = chatter_ui::bin_ui::converters::message_to_display_data(
            &persisted,
            "Author",
            &[],
            None,
        )
        .to_slint_with_user(&own.to_string());
        assert!(
            initial.reactions.row_data(0).unwrap().own,
            "initial rendering uses the persisted own-reactor cache too"
        );
        // A second reactor persists after local completion was queued. That
        // completion fetches again, rather than replaying its older count=1.
        client
            .messages()
            .record_reaction(
                &chatter_core::domain::reaction::ReactionScope::new(
                    scope.channel_id().unwrap(),
                    kind.clone(),
                ),
                &author,
                sent_at,
                &second_reactor,
                "👍",
                false,
            )
            .unwrap();
        for _duplicate_event_or_completion in 0..3 {
            refresh_reaction_in_ui(&window, &client, scope, &author, sent_at);
        }
        assert_eq!(
            window
                .get_messages()
                .row_data(0)
                .unwrap()
                .reactions
                .row_data(0)
                .unwrap()
                .count,
            2
        );
        client
            .messages()
            .record_reaction(
                &chatter_core::domain::reaction::ReactionScope::new(
                    scope.channel_id().unwrap(),
                    kind.clone(),
                ),
                &author,
                sent_at,
                &own,
                "👍",
                true,
            )
            .unwrap();
        refresh_reaction_in_ui(&window, &client, scope, &author, sent_at);
        assert_eq!(
            window
                .get_messages()
                .row_data(0)
                .unwrap()
                .reactions
                .row_data(0)
                .unwrap()
                .count,
            1
        );
        assert!(
            !window
                .get_messages()
                .row_data(0)
                .unwrap()
                .reactions
                .row_data(0)
                .unwrap()
                .own,
            "own remove clears highlight while another reactor keeps the count"
        );
        let persisted = client
            .messages()
            .get_message_local(&target.id)
            .unwrap()
            .unwrap();
        let initial = chatter_ui::bin_ui::converters::message_to_display_data(
            &persisted,
            "Author",
            &[],
            None,
        )
        .to_slint_with_user(&own.to_string());
        assert!(
            !initial.reactions.row_data(0).unwrap().own,
            "initial rendering clears own highlight without dropping another reactor"
        );
        let switched = ActiveConversation::Channel(ChannelId::new());
        client.chat_state().set_active_conversation(switched);
        client
            .messages()
            .record_reaction(
                &chatter_core::domain::reaction::ReactionScope::new(
                    scope.channel_id().unwrap(),
                    kind.clone(),
                ),
                &author,
                sent_at,
                &second_reactor,
                "👍",
                true,
            )
            .unwrap();
        refresh_reaction_in_ui(&window, &client, scope, &author, sent_at);
        assert_eq!(
            window
                .get_messages()
                .row_data(0)
                .unwrap()
                .reactions
                .row_data(0)
                .unwrap()
                .count,
            1,
            "old completion must not touch a switched view"
        );
        client.chat_state().set_active_conversation(scope);
        refresh_reaction_in_ui(&window, &client, scope, &author, sent_at);
        let row = window.get_messages().row_data(0).unwrap();
        assert!(!row.has_reactions);
        assert_eq!(row.reactions.row_count(), 0);
        assert!(row.reaction_summary.is_empty());
        refresh_reaction_in_ui(&window, &client, scope, &author, sent_at);
        assert_eq!(
            window
                .get_messages()
                .row_data(0)
                .unwrap()
                .reactions
                .row_count(),
            0
        );
    }
    // Simulate a real SQL read failure in this disposable fixture database.
    // Refresh must leave the presented row intact, not manufacture a count.
    let scope = client.active_conversation();
    let mut row = window.get_messages().row_data(0).unwrap();
    row.has_reactions = true;
    row.reactions = ModelRc::new(VecModel::from(vec![ReactionEntry {
        emoji: "👍".into(),
        count: 77,
        own: false,
    }]));
    window.set_messages(ModelRc::new(VecModel::from(vec![row])));
    let db = chatter_core::storage::Database::open(
        &dir.path().join("data/chatter.db"),
        "reaction-fixture-passphrase",
    )
    .unwrap();
    db.connection()
        .unwrap()
        .execute_batch("ALTER TABLE messages RENAME TO reaction_fixture_unavailable")
        .unwrap();
    assert!(
        client
            .conversation(scope)
            .reaction_snapshot(&author, 1000)
            .is_err()
    );
    assert!(
        rt.block_on(
            client
                .conversation(scope)
                .toggle_reaction("👍", &author, 1000)
        )
        .is_err()
    );
    refresh_reaction_in_ui(&window, &client, scope, &author, 1000);
    assert_eq!(
        window
            .get_messages()
            .row_data(0)
            .unwrap()
            .reactions
            .row_data(0)
            .unwrap()
            .count,
        77
    );
}

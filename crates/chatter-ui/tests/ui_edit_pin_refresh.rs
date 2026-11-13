//! Persisted edits refresh pins in place; hidden raised actions have no material.
use chatter_core::{
    domain::ids::{ChannelId, MessageId},
    state::ActiveConversation,
};
use chatter_ui::{MainWindow, MessageItemData, PinnedItem};
use i_slint_backend_testing as testing;
use slint::{ComponentHandle, Model, ModelRc, VecModel};

fn pump(w: &MainWindow, width: f32) {
    w.window().set_size(slint::LogicalSize::new(width, 700.0));
    w.show().unwrap();
    for _ in 0..3 {
        slint::Timer::single_shot(std::time::Duration::from_millis(20), || {
            slint::quit_event_loop().unwrap()
        });
        w.run().unwrap();
    }
}

/// The view handoff (app_view_transition.slint) holds a window-sized
/// stray-tap blocker for motion-standard + 40ms after `current-view` changes:
/// 360ms at the default animation speed since 15919403 (120ms before). Until
/// it lifts, pointer events stop there and never instantiate the composer.
fn finish_view_handoff(w: &MainWindow) {
    slint::Timer::single_shot(std::time::Duration::from_millis(500), || {
        slint::quit_event_loop().unwrap();
    });
    w.run().unwrap();
}

#[test]
fn edit_surfaces_and_pins_follow_current_conversation_without_reordering() {
    assert!(chatter_ui::SLINT_DEBUG_INFO_ENABLED);
    testing::init_integration_test_with_system_time();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let _runtime = runtime.enter();
    let fixture = tempfile::tempdir().unwrap();
    // Off the real keyring: the marker must exist before the client opens.
    let db_path = chatter_core::system::Directories::with_base_dir(fixture.path().to_path_buf())
        .unwrap()
        .database_path();
    std::fs::write(
        db_path
            .parent()
            .unwrap()
            .join(chatter_core::system::keyring::NO_KEYRING_MARKER),
        b"",
    )
    .unwrap();
    let client = chatter_core::client::ChatterClient::builder()
        .data_dir(fixture.path().to_string_lossy().to_string())
        .build()
        .unwrap();
    let w = MainWindow::new().unwrap();
    w.set_current_view("main".into());
    finish_view_handoff(&w);
    w.global::<chatter_ui::ChatPane>()
        .set_channel_name("Owned edit fixture".into());
    w.global::<chatter_ui::ChatPane>()
        .set_conversation_kind(chatter_ui::ConversationKind::Channel);
    w.set_is_mobile_platform(true);
    w.global::<chatter_ui::SidebarPane>()
        .set_mobile_show_chat(true);
    w.global::<chatter_ui::ChatPane>()
        .set_composer_text("Edited plaintext".into());
    for width in [360.0, 412.0] {
        for (editing, replying) in [(false, false), (true, false), (false, true), (false, false)] {
            w.global::<chatter_ui::ChatPane>()
                .set_composer_is_editing(editing);
            w.global::<chatter_ui::ChatPane>()
                .set_composer_is_replying(replying);
            w.global::<chatter_ui::ChatPane>()
                .set_composer_reply_preview("Reply accessory material".into());
            pump(&w, width);
            // The per-button `*-action-surface` materials were folded into the
            // composer's one silhouette in 4fd3d8ea; what an edit must still
            // not show is the attachment control, and each control keeps to
            // its own button box.
            let shown = |label: &str| {
                testing::ElementHandle::find_by_accessible_label(&w, label)
                    .filter(|e| e.size().width > 0.0 && e.size().height > 0.0)
                    .collect::<Vec<_>>()
            };
            let attach = shown("Attach file");
            if editing {
                assert!(attach.is_empty(), "Edit must not offer an attachment");
            } else {
                assert_eq!(attach.len(), 1, "width {width}: one Attach file control");
                assert!(attach[0].size().width <= 64.0, "Attach must fit its button");
            }
            let emoji = shown("Emoji & GIF picker");
            assert_eq!(emoji.len(), 1);
            assert!(emoji[0].size().width <= 64.0);
            // A reply's preview fills the gap between the controls (649b152a).
            assert_eq!(
                !shown("Cancel reply").is_empty(),
                replying,
                "the reply preview shows exactly while replying"
            );
        }
    }
    let conversation = ActiveConversation::Channel(ChannelId::new());
    client.select_conversation(conversation);
    let ids = [MessageId::new(), MessageId::new()];
    client.set_pinned_messages(ids.to_vec());
    let selected = client.pinned_current_message_id().unwrap();
    w.global::<chatter_ui::ChatPane>()
        .set_pinned_message_preview("Old plaintext".into());
    w.global::<chatter_ui::ChatPane>()
        .set_pinned_items(ModelRc::new(VecModel::from(
            ids.iter()
                .map(|id| PinnedItem {
                    message_id: id.to_string().into(),
                    preview: "Old plaintext".into(),
                    ..Default::default()
                })
                .collect::<Vec<_>>(),
        )));
    w.global::<chatter_ui::ChatPane>()
        .set_messages(ModelRc::new(VecModel::from(vec![
            MessageItemData {
                message_id: selected.to_string().into(),
                sender_id: "owner".into(),
                timestamp_ns: "42".into(),
                content: "Old plaintext".into(),
                is_pinned: true,
                ..Default::default()
            },
            MessageItemData {
                message_id: "other-author".into(),
                sender_id: "peer".into(),
                timestamp_ns: "42".into(),
                content: "Peer text".into(),
                ..Default::default()
            },
        ])));
    let apply = |author, id: &str, text: &str| {
        chatter_ui::bin_ui::features::chat::apply_message_edit_in_conversation(
            &w,
            &client,
            conversation,
            id,
            author,
            42,
            text,
        )
    };
    apply("owner", "edit-event-id", "Edited plaintext");
    assert_eq!(
        w.global::<chatter_ui::ChatPane>()
            .get_messages()
            .row_data(0)
            .unwrap()
            .content,
        "Edited plaintext"
    );
    assert_eq!(
        w.global::<chatter_ui::ChatPane>()
            .get_messages()
            .row_data(1)
            .unwrap()
            .content,
        "Peer text"
    );
    assert_eq!(
        w.global::<chatter_ui::ChatPane>()
            .get_pinned_message_preview(),
        "Edited plaintext"
    );
    assert_eq!(client.chat_snapshot().pinned.pinned_message_ids, ids);
    for (i, id) in ids.iter().enumerate() {
        let pin = w
            .global::<chatter_ui::ChatPane>()
            .get_pinned_items()
            .row_data(i)
            .unwrap();
        assert_eq!(pin.message_id, id.to_string());
        assert_eq!(
            pin.preview,
            if *id == selected {
                "Edited plaintext"
            } else {
                "Old plaintext"
            }
        );
    }
    // An edit outside loaded history still refreshes the selected pin/list.
    w.global::<chatter_ui::ChatPane>()
        .set_messages(ModelRc::new(VecModel::<MessageItemData>::default()));
    apply("owner", &selected.to_string(), "Unloaded edit");
    assert_eq!(
        w.global::<chatter_ui::ChatPane>()
            .get_pinned_message_preview(),
        "Unloaded edit"
    );
    let other = *ids.iter().find(|id| **id != selected).unwrap();
    apply("owner", &other.to_string(), "Other pin edit");
    assert_eq!(
        w.global::<chatter_ui::ChatPane>()
            .get_pinned_message_preview(),
        "Unloaded edit"
    );
    assert_eq!(
        w.global::<chatter_ui::ChatPane>()
            .get_pinned_items()
            .iter()
            .find(|p| p.message_id == other.to_string())
            .unwrap()
            .preview,
        "Other pin edit"
    );
    assert_eq!(client.pinned_current_message_id(), Some(selected));
    let queued_client = client.clone();
    w.as_weak()
        .upgrade_in_event_loop(move |w| {
            chatter_ui::bin_ui::features::chat::apply_message_edit_in_conversation(
                &w,
                &queued_client,
                conversation,
                &selected.to_string(),
                "owner",
                42,
                "Stale queued edit",
            );
        })
        .unwrap();
    client.select_conversation(ActiveConversation::Channel(ChannelId::new()));
    pump(&w, 360.0);
    assert_eq!(
        w.global::<chatter_ui::ChatPane>()
            .get_pinned_message_preview(),
        "Unloaded edit"
    );
    assert!(
        w.global::<chatter_ui::ChatPane>()
            .get_pinned_items()
            .iter()
            .all(|p| p.preview != "Stale queued edit")
    );
}

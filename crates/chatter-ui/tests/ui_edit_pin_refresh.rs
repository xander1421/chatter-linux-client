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
    w.set_channel_name("Owned edit fixture".into());
    w.set_conversation_kind(chatter_ui::ConversationKind::Channel);
    w.set_is_mobile_platform(true);
    w.set_mobile_show_chat(true);
    w.set_composer_text("Edited plaintext".into());
    for width in [360.0, 412.0] {
        for (editing, replying) in [(false, false), (true, false), (false, true), (false, false)] {
            w.set_composer_is_editing(editing);
            w.set_composer_is_replying(replying);
            w.set_composer_reply_preview("Reply accessory material".into());
            pump(&w, width);
            let right = testing::ElementHandle::find_by_element_id(
                &w,
                "MessageComposer::right-action-surface",
            )
            .filter(|e| e.size().width > 0.0 && e.size().height > 0.0)
            .collect::<Vec<_>>();
            if editing {
                assert!(
                    right.is_empty(),
                    "Edit must not render an empty attachment material"
                );
                assert!(
                    testing::ElementHandle::find_by_accessible_label(&w, "Attach file")
                        .next()
                        .is_none()
                );
            } else {
                assert_eq!(right.len(), 1);
                assert!(
                    right[0].size().width <= 64.0,
                    "Material must fit its actual button"
                );
            }
            let left = testing::ElementHandle::find_by_element_id(
                &w,
                "MessageComposer::left-action-surface",
            )
            .next()
            .unwrap();
            assert!(left.size().width <= 64.0);
            // Check painted descendants, not just the named action rectangles:
            // a vertically centered accessory material passes button geometry.
            let gap_x = left.absolute_position().x + left.size().width + 8.0;
            let gap_y = left.absolute_position().y + left.size().height / 2.0;
            // A reply's preview fills that gap (649b152a); otherwise it is empty.
            let mut materials = 0;
            let mut gap_painted = false;
            for layer in testing::ElementHandle::find_by_element_type_name(&w, "Backdrop") {
                let pos = layer.absolute_position();
                let size = layer.size();
                if size.width <= 0.0 || size.height <= 0.0 {
                    continue;
                }
                if pos.y <= gap_y && pos.y + size.height > gap_y {
                    materials += 1;
                    if pos.x <= gap_x && pos.x + size.width > gap_x {
                        assert!(
                            replying,
                            "Material paints the empty action center: {pos:?} {size:?}"
                        );
                        gap_painted = true;
                    }
                }
            }
            assert!(
                !replying || gap_painted,
                "the reply preview does not fill the action gap"
            );
            assert!(
                materials > 0,
                "Regression must inspect actual material layers"
            );
        }
    }
    let conversation = ActiveConversation::Channel(ChannelId::new());
    client.chat_state().set_active_conversation(conversation);
    let ids = [MessageId::new(), MessageId::new()];
    client.chat_state().set_pinned_messages(ids.to_vec());
    let selected = client.chat_state().pinned_current_message_id().unwrap();
    w.set_pinned_message_preview("Old plaintext".into());
    w.set_pinned_items(ModelRc::new(VecModel::from(
        ids.iter()
            .map(|id| PinnedItem {
                message_id: id.to_string().into(),
                preview: "Old plaintext".into(),
                ..Default::default()
            })
            .collect::<Vec<_>>(),
    )));
    w.set_messages(ModelRc::new(VecModel::from(vec![
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
        w.get_messages().row_data(0).unwrap().content,
        "Edited plaintext"
    );
    assert_eq!(w.get_messages().row_data(1).unwrap().content, "Peer text");
    assert_eq!(w.get_pinned_message_preview(), "Edited plaintext");
    assert_eq!(client.chat_state().get().pinned.pinned_message_ids, ids);
    for (i, id) in ids.iter().enumerate() {
        let pin = w.get_pinned_items().row_data(i).unwrap();
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
    w.set_messages(ModelRc::new(VecModel::<MessageItemData>::default()));
    apply("owner", &selected.to_string(), "Unloaded edit");
    assert_eq!(w.get_pinned_message_preview(), "Unloaded edit");
    let other = *ids.iter().find(|id| **id != selected).unwrap();
    apply("owner", &other.to_string(), "Other pin edit");
    assert_eq!(w.get_pinned_message_preview(), "Unloaded edit");
    assert_eq!(
        w.get_pinned_items()
            .iter()
            .find(|p| p.message_id == other.to_string())
            .unwrap()
            .preview,
        "Other pin edit"
    );
    assert_eq!(
        client.chat_state().pinned_current_message_id(),
        Some(selected)
    );
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
    client
        .chat_state()
        .set_active_conversation(ActiveConversation::Channel(ChannelId::new()));
    pump(&w, 360.0);
    assert_eq!(w.get_pinned_message_preview(), "Unloaded edit");
    assert!(w
        .get_pinned_items()
        .iter()
        .all(|p| p.preview != "Stale queued edit"));
}

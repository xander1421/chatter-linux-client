//! Reactions share the bubble's footer row with the clock: chips at the start,
//! clock at the end, no extra row under the text.
use chatter_ui::{ConversationKind, MainWindow, MessageItemData, ReactionEntry};
use i_slint_backend_testing as testing;
use slint::{ComponentHandle, ModelRc, VecModel};

fn pump(w: &MainWindow, width: f32) {
    w.window().set_size(slint::LogicalSize::new(width, 891.0));
    w.show().unwrap();
    for _ in 0..20 {
        w.window()
            .dispatch_event(slint::platform::WindowEvent::Resized {
                size: slint::LogicalSize::new(width, 891.0),
            });
        slint::Timer::single_shot(std::time::Duration::from_millis(20), || {
            slint::quit_event_loop().unwrap();
        });
        w.run().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

fn rect(w: &MainWindow, label: &str) -> (f32, f32, f32, f32) {
    let e = testing::ElementHandle::find_by_accessible_label(w, label)
        .find(|e| e.size().width > 0.0 && e.size().height > 0.0)
        .unwrap_or_else(|| panic!("missing visible {label}"));
    let (p, s) = (e.absolute_position(), e.size());
    (p.x, p.y, s.width, s.height)
}

#[test]
fn reactions_sit_on_the_clock_row() {
    assert!(chatter_ui::SLINT_DEBUG_INFO_ENABLED);
    testing::init_integration_test_with_system_time();
    let w = MainWindow::new().unwrap();
    w.set_current_view("main".into());
    w.set_is_mobile_platform(true);
    w.global::<chatter_ui::SidebarPane>()
        .set_mobile_show_chat(true);
    w.global::<chatter_ui::ChatPane>()
        .set_conversation_kind(ConversationKind::Channel);
    w.global::<chatter_ui::ChatPane>()
        .set_channel_name("Reaction footer fixture".into());
    let row = |id: &str, own: bool, reacted: bool| MessageItemData {
        message_id: id.into(),
        sender_id: id.into(),
        sender_name: "Fixture".into(),
        content: format!("body-{id}").into(),
        timestamp: format!("clock-{id}").into(),
        is_own: own,
        show_sender: true,
        has_reactions: reacted,
        reaction_summary: format!("sum-{id}").into(),
        reactions: ModelRc::new(VecModel::from(if reacted {
            vec![
                ReactionEntry {
                    emoji: "👍".into(),
                    count: 2,
                    own: false,
                },
                ReactionEntry {
                    emoji: "❤".into(),
                    count: 1,
                    own: true,
                },
            ]
        } else {
            Vec::new()
        })),
        ..Default::default()
    };
    w.global::<chatter_ui::ChatPane>()
        .set_messages(ModelRc::new(VecModel::from(vec![
            row("peer", false, true),
            row("own", true, true),
            row("plain", false, false),
        ])));
    for width in [360.0, 412.0] {
        pump(&w, width);
        for id in ["peer", "own"] {
            let pill = rect(&w, &format!("Reactions: sum-{id}"));
            let clock = rect(&w, &format!("clock-{id}"));
            let body = rect(&w, &format!("body-{id}"));
            // Same row: the clock's vertical span lies inside the pill's.
            assert!(
                clock.1 >= pill.1 - 0.5 && clock.1 + clock.3 <= pill.1 + pill.3 + 0.5,
                "{id} at {width}px: clock y={}..{} outside pill y={}..{}",
                clock.1,
                clock.1 + clock.3,
                pill.1,
                pill.1 + pill.3
            );
            // Chips first, clock after them, no overlap.
            assert!(
                pill.0 + pill.2 <= clock.0 + 0.5,
                "{id}: pill overlaps clock"
            );
            // The footer row is below the text, not beside it.
            assert!(
                pill.1 >= body.1 + body.3 - 0.5,
                "{id}: pill not under the text"
            );
        }
    }
}

#[test]
fn upload_queue_status_never_claims_sent_before_its_turn() {
    assert!(chatter_ui::SLINT_DEBUG_INFO_ENABLED);
    testing::init_integration_test_with_system_time();
    let w = MainWindow::new().unwrap();
    w.set_current_view("main".into());
    w.set_is_mobile_platform(true);
    w.global::<chatter_ui::SidebarPane>().set_mobile_show_chat(true);
    w.global::<chatter_ui::ChatPane>().set_conversation_kind(ConversationKind::Channel);
    w.global::<chatter_ui::ChatPane>().set_channel_name("Queue footer fixture".into());
    let visible = |label: &str| testing::ElementHandle::find_by_accessible_label(&w, label)
        .any(|e| e.size().width > 0.0 && e.size().height > 0.0);
    for (status, label) in [
        ("upload-queued", "Queued behind an earlier send; upload has not started"),
        ("sending", "Sending"),
        ("sent", "Sent"),
    ] {
        w.global::<chatter_ui::ChatPane>().set_messages(ModelRc::new(VecModel::from(vec![MessageItemData {
            message_id: "pending:queue-footer".into(), sender_id: "fixture-owner".into(),
            sender_name: "Fixture".into(), content: "Queued image batch".into(),
            timestamp: "10:42".into(), is_own: true, delivery_status: status.into(),
            ..Default::default()
        }])));
        pump(&w, 360.0);
        assert!(visible(label), "missing {status} status");
        assert_eq!(visible("Sent"), status == "sent");
        assert!(!visible("Delivered"));
        assert!(!visible("Read"));
    }
}

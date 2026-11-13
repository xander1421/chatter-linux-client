//! A queued upload's footer never claims Sent (or Delivered/Read) before its
//! turn. Own test BINARY with one `#[test]`: the integration backend is
//! once-per-process (see `ui_render.rs`), and `ui_reaction_footer.rs` claims it.
use chatter_ui::{ConversationKind, MainWindow, MessageItemData};
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

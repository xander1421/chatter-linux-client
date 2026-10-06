//! Pure phone geometry regression; no client, database or credentials.
use chatter_ui::{ConversationKind, MainWindow, MessageItemData};
use i_slint_backend_testing as testing;
use slint::{ComponentHandle, ModelRc, VecModel};

fn pump(w: &MainWindow) {
    w.window().set_size(slint::LogicalSize::new(412.0, 891.0));
    slint::Timer::single_shot(std::time::Duration::from_millis(80), || {
        slint::quit_event_loop().unwrap();
    });
    w.show().unwrap();
    slint::run_event_loop().unwrap();
}

fn visible(w: &MainWindow, label: &str) -> Vec<testing::ElementHandle> {
    testing::ElementHandle::find_by_accessible_label(w, label)
        .filter(|e| e.size().height > 0.0 && e.size().width > 0.0)
        .collect()
}

#[test]
fn call_clock_is_below_status_and_icon_with_channel_identity_only() {
    assert!(
        chatter_ui::SLINT_DEBUG_INFO_ENABLED,
        "requires ui-test-debug"
    );
    testing::init_integration_test_with_system_time();
    let w = MainWindow::new().unwrap();
    w.set_current_view("main".into());
    w.set_is_mobile_platform(true);
    w.set_mobile_show_chat(true);
    w.set_channel_name("Call fixture".into());
    for kind in [ConversationKind::Channel, ConversationKind::ServerDm] {
        w.set_conversation_kind(kind);
        w.set_is_dm(kind == ConversationKind::ServerDm);
        for (own, continuation, show_sender) in [
            (false, false, true),
            (true, false, true),
            (false, true, true),
            (false, false, false),
        ] {
            for (status, duration) in [("incoming", "1 min 30 s"), ("missed", ""), ("declined", "")]
            {
                let row = MessageItemData {
                    message_id: "call-geometry".into(),
                    sender_id: "caller-fixture".into(),
                    sender_name: "Caller Fixture".into(),
                    sender_initials: "CF".into(),
                    timestamp: "17:42".into(),
                    is_call: true,
                    is_own: own,
                    is_continuation: continuation,
                    show_sender,
                    call_type: status.into(),
                    call_duration: duration.into(),
                    ..Default::default()
                };
                w.set_messages(ModelRc::new(VecModel::from(vec![row])));
                pump(&w);
                let clock = visible(&w, "17:42").into_iter().next().expect("call clock");
                let label = match status {
                    "missed" => "Missed call",
                    "declined" => "Declined call",
                    _ if own => "Outgoing call",
                    _ => "Incoming call",
                };
                let status_el = visible(&w, label).into_iter().next().expect("call status");
                let cp = clock.absolute_position();
                let sp = status_el.absolute_position();
                assert!(
                    cp.y >= sp.y + status_el.size().height - 0.5,
                    "clock must be a footer"
                );
                assert!(
                    cp.x + clock.size().width >= sp.x + status_el.size().width - 0.5,
                    "clock must align right"
                );
                let icon = visible(&w, "Call icon")
                    .into_iter()
                    .next()
                    .expect("call icon");
                assert!(
                    cp.y >= icon.absolute_position().y + icon.size().height - 0.5,
                    "clock must sit below the entire call body"
                );
                if !duration.is_empty() {
                    let d = visible(&w, duration)
                        .into_iter()
                        .next()
                        .expect("distinct duration");
                    assert!(cp.y >= d.absolute_position().y + d.size().height - 0.5);
                }
                let identity =
                    kind == ConversationKind::Channel && !own && !continuation && show_sender;
                assert_eq!(
                    !visible(&w, "Avatar for CF").is_empty(),
                    identity,
                    "avatar: {kind:?}, own={own}, continuation={continuation}, show={show_sender}, status={status}"
                );
                assert_eq!(
                    !visible(&w, "Caller Fixture").is_empty(),
                    identity,
                    "name: {kind:?}, own={own}, continuation={continuation}, show={show_sender}, status={status}"
                );
                assert!(cp.x >= 0.0 && cp.x + clock.size().width <= 412.5);
            }
        }
    }
}

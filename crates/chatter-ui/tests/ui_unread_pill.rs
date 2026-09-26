//! The "N new messages" pill must only float when there is something below the
//! viewport to jump to. Own test binary: the integration testing backend is
//! once-per-process.
//!
//! Regression (device sweep, client 8adfb834): a conversation opened on an
//! unread boundary whose whole tail fits on screen still showed "1 new message ↓"
//! over the message itself. The open clears `is-at-bottom` for the anchor and
//! the list never re-derived it, because nothing it listened to *changed* after
//! the freshly created Flickable laid out.

use chatter_ui::{MainWindow, MessageItemData};
use i_slint_backend_testing as testing;
use slint::{ComponentHandle, ModelRc, VecModel};

fn pump(w: &MainWindow) {
    slint::Timer::single_shot(std::time::Duration::from_millis(400), || {
        slint::quit_event_loop().unwrap();
    });
    w.run().unwrap();
}

/// The testing backend never draws, and the message list lays its rows out
/// only inside the window's instantiation pass (what a frame, an input event
/// or an element query triggers). A pointer move is that pass: it loops
/// instantiation and change handlers until nothing moves, which is the same
/// loop a real frame runs before it is drawn.
fn lay_out(w: &MainWindow) {
    w.window().dispatch_event(slint::platform::WindowEvent::PointerMoved {
        position: slint::LogicalPosition::new(1.0, 1.0),
    });
}

fn msgs(n: usize) -> ModelRc<MessageItemData> {
    ModelRc::new(VecModel::from(
        (0..n)
            .map(|i| MessageItemData {
                message_id: format!("m{i}").into(),
                sender_id: "dev".into(),
                sender_name: "Dev".into(),
                content: format!("message {i}").into(),
                timestamp_ns: format!("{i}").into(),
                show_sender: true,
                ..Default::default()
            })
            .collect::<Vec<_>>(),
    ))
}

/// Open a conversation the way `sidebar::channel` does: loading raised, then
/// anchor + page + loading lowered in one UI closure.
fn open(w: &MainWindow, n: usize, unread_index: i32) {
    w.set_loading_messages(true);
    pump(w);
    w.set_unread_index(unread_index);
    w.set_unread_count(1);
    w.set_is_at_bottom(false);
    w.set_messages(msgs(n));
    w.set_loading_messages(false);
    lay_out(w);
    pump(w);
}

#[test]
fn pill_only_when_unread_is_below_the_viewport() {
    testing::init_integration_test_with_system_time();
    let w = MainWindow::new().expect("MainWindow::new");
    w.set_current_view("main".into());
    // The chat panel exists only for an open conversation (app.slint gates
    // ChatLayout on a non-empty channel name); without one, no list is ever
    // created and nothing here is tested.
    w.set_channel_name("dev".into());
    w.show().unwrap();
    pump(&w);

    // Three short bubbles, the last unread: everything is on screen.
    open(&w, 3, 2);
    assert!(
        w.get_is_at_bottom(),
        "short conversation opened on an unread boundary still reports it is \
         scrolled away from the bottom — the \"1 new message\" pill floats over \
         the message it points at"
    );
    assert_eq!(w.get_unread_count(), 0, "caught up, so the pill has no count");

    // Control: a long conversation opened on an early boundary really does
    // have unread messages below the viewport.
    open(&w, 200, 5);
    assert!(
        !w.get_is_at_bottom(),
        "long conversation anchored near the top claims to be at the bottom"
    );
}

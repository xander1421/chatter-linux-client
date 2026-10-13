//! Swipe-to-reply gesture through the actual message bubble.

use i_slint_backend_testing as testing;
use slint::{ComponentHandle, LogicalPosition};
use slint::platform::{PointerEventButton, WindowEvent};

slint::slint! {
    import { MessageItem } from "../../ui/chat/message_item.slint";
    export component SwipeHarness inherits Window {
        width: 420px;
        height: 300px;
        in-out property <int> replies: 0;
        in-out property <bool> deleted: false;
        in-out property <bool> outgoing: false;
        in-out property <bool> tracking: row.swipe-tracking;
        row := MessageItem {
            width: parent.width;
            height: 120px;
            mobile: true;
            is-dm: true;
            sender-id: "sender";
            timestamp-ns: "123";
            message-id: "message";
            content: "Swipe me";
            is-deleted: root.deleted;
            is-own: root.outgoing;
            reply-clicked(sender, timestamp) => {
                if (sender == "sender" && timestamp == "123") {
                    root.replies += 1;
                }
            }
        }
    }

}

fn find_bubble_press(window: &SwipeHarness, outgoing: bool) -> LogicalPosition {
    let xs: Vec<i32> = if outgoing { (200..420).step_by(10).collect() } else { (10..200).step_by(10).collect() };
    for y in (10..290).step_by(10) {
        for &x in &xs {
            let position = LogicalPosition::new(x as f32, y as f32);
            window.window().dispatch_event(WindowEvent::PointerPressed { position, button: PointerEventButton::Left });
            let hit = window.get_tracking();
            window.window().dispatch_event(WindowEvent::PointerReleased { position, button: PointerEventButton::Left });
            if hit { return position; }
        }
    }
    panic!("no swipe-enabled message bubble found");
}

fn drag(window: &SwipeHarness, start: LogicalPosition, dx: f32, dy: f32) {
    window.window().dispatch_event(WindowEvent::PointerMoved { position: start });
    window.window().dispatch_event(WindowEvent::PointerPressed {
        position: start,
        button: PointerEventButton::Left,
    });
    assert!(window.get_tracking(), "press must begin on a swipe-enabled bubble");
    for step in 1..=10 {
        window.window().dispatch_event(WindowEvent::PointerMoved {
            position: LogicalPosition::new(start.x + dx * step as f32 / 10., start.y + dy * step as f32 / 10.),
        });
    }
    window.window().dispatch_event(WindowEvent::PointerReleased {
        position: LogicalPosition::new(start.x + dx, start.y + dy),
        button: PointerEventButton::Left,
    });
    testing::testing_backend::mock_elapsed_time(250);
}

#[test]
fn swipe_reply_requires_horizontal_threshold() {
    testing::init_no_event_loop();
    let window = SwipeHarness::new().unwrap();
    window.show().unwrap();
    let incoming = find_bubble_press(&window, false);

    drag(&window, incoming, 40., 0.);
    assert_eq!(window.get_replies(), 0, "short drag must not reply");

    drag(&window, incoming, 0., 100.);
    assert_eq!(window.get_replies(), 0, "vertical scroll must not reply");

    drag(&window, incoming, 80., 0.);
    assert_eq!(window.get_replies(), 1, "armed swipe must use the existing reply callback");

    window.set_deleted(true);
    // Deleted rows lose their bubble entirely; the same drag must do nothing.
    let start = incoming;
    window.window().dispatch_event(WindowEvent::PointerPressed { position: start, button: PointerEventButton::Left });
    window.window().dispatch_event(WindowEvent::PointerMoved { position: LogicalPosition::new(start.x + 80., start.y) });
    window.window().dispatch_event(WindowEvent::PointerReleased { position: LogicalPosition::new(start.x + 80., start.y), button: PointerEventButton::Left });
    assert_eq!(window.get_replies(), 1, "deleted message must not arm a reply");

    window.set_deleted(false);
    window.set_outgoing(true);
    let outgoing = find_bubble_press(&window, true);
    drag(&window, outgoing, -80., 0.);
    assert_eq!(window.get_replies(), 2, "outgoing bubble swipes toward the chat center");
}


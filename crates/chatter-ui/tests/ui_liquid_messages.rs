//! Software-renderer frames for a message and its anchored context menu.
use i_slint_backend_testing::{self as testing, TestingBackend, TestingBackendOptions};
use slint::ComponentHandle;

slint::slint! {
    import { MessageItem } from "../../ui/chat/message_item.slint";
    import { MessageContextMenu, MessageMenuActions } from "../../ui/chat/message_context_menu.slint";
    import { Colors } from "../../ui/theme/colors.slint";
    import { Appearance } from "../../ui/theme/appearance.slint";

    export component MessageHarness inherits Window {
        in-out property <bool> desktop: false;
        in-out property <bool> menu-open: false;
        in-out property <bool> arriving: false;
        width: desktop ? 960px : 412px;
        height: desktop ? 640px : 700px;
        background: Colors.background;
        callback theme(string);
        callback flags();
        theme(value) => { Appearance.theme = value; }
        flags() => {
            MessageMenuActions.react = true;
            MessageMenuActions.reply = true;
            MessageMenuActions.copy = true;
            MessageMenuActions.select = true;
        }

        MessageItem {
            x: 16px;
            y: parent.height / 2 - 110px;
            width: parent.width - 32px;
            message-id: "earlier-message";
            sender-id: "morgan";
            sender-name: "Morgan";
            content: "See you there";
            timestamp: "10:41";
            mobile: !root.desktop;
        }

        if root.arriving: MessageItem {
            x: 16px;
            y: parent.height / 2;
            width: parent.width - 32px;
            message-id: "capture-message";
            sender-id: "capture-sender";
            sender-name: "Morgan";
            content: "The message stays under the menu";
            timestamp: "10:42";
            is-own: true;
            delivery-status: "read";
            mobile: !root.desktop;
            animate-arrival: true;
        }

        MessageContextMenu {
            show: root.menu-open;
            position-x: root.desktop ? root.width - 290px : root.width - 80px;
            position-y: root.height / 2 + 20px;
            content-top: 48px;
            message-id: "capture-message";
            sender-id: "capture-sender";
        }
    }
}

fn advance(ms: u64) {
    let mut left = ms;
    while left > 0 {
        let step = left.min(16);
        testing::testing_backend::mock_elapsed_time(step);
        slint::platform::update_timers_and_animations();
        left -= step;
    }
}

fn frame(window: &MessageHarness, name: &str) -> Vec<u8> {
    let pixels = window.window().take_snapshot().unwrap();
    let dir = std::env::var_os("U2_CAPTURE_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("chatter-u2-frames"));
    std::fs::create_dir_all(&dir).unwrap();
    image::save_buffer(
        dir.join(name),
        pixels.as_bytes(),
        pixels.width(),
        pixels.height(),
        image::ColorType::Rgba8,
    )
    .unwrap();
    pixels.as_bytes().to_vec()
}

#[test]
fn context_menu_grows_from_message_in_both_themes_and_widths() {
    slint::platform::set_platform(Box::new(TestingBackend::new(TestingBackendOptions {
        mock_time: true,
        threading: false,
        renderer_name: Some("software".into()),
    })))
    .unwrap();
    for desktop in [false, true] {
        // The testing backend fixes a Window's physical size at show time.
        // Construct a new one for desktop instead of resizing the phone view.
        let window = MessageHarness::new().unwrap();
        window.set_desktop(desktop);
        window.show().unwrap();
        window.invoke_flags();
        let width = if desktop { "desktop" } else { "phone" };
        for theme in ["light", "dark"] {
            window.invoke_theme(theme.into());
            window.set_menu_open(false);
            window.set_arriving(false);
            advance(500);
            let prefix = format!("{theme}-{width}");
            let before = frame(&window, &format!("{prefix}-01-before.png"));
            window.set_arriving(true);
            advance(80);
            let bubble = frame(&window, &format!("{prefix}-02-bubble.png"));
            assert!(before != bubble, "new bubble must visibly travel");
            advance(500);
            window.set_menu_open(true);
            advance(80);
            let opening = frame(&window, &format!("{prefix}-03-opening.png"));
            advance(400);
            let open = frame(&window, &format!("{prefix}-04-open.png"));
            assert!(before != opening, "menu must visibly emerge");
            assert!(opening != open, "menu must settle after the first frame");
        }
    }
}

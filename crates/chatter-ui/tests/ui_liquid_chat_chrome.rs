//! Software-rendered arrival frames for the chat header menu and mention popup.
use i_slint_backend_testing::{self as testing, TestingBackend, TestingBackendOptions};
use slint::ComponentHandle;

slint::slint! {
    import { ChatHeader } from "../../ui/chat/chat_header.slint";
    import { ChatHeaderMenu } from "../../ui/chat/chat_header_menu.slint";
    import { MentionAutocomplete } from "../../ui/components/mention_autocomplete.slint";
    import { ConversationKind } from "../../ui/chat/conversation_kind.slint";
    import { Colors } from "../../ui/theme/colors.slint";
    import { Appearance } from "../../ui/theme/appearance.slint";

    export component ChromeHarness inherits Window {
        in-out property <length> canvas-width: 412px;
        in-out property <bool> menu-open: false;
        in-out property <bool> mention-open: false;
        in-out property <bool> scrolling: false;
        width: root.canvas-width;
        height: 640px;
        background: Colors.background;
        callback theme(string);
        theme(value) => { Appearance.theme = value; }

        ChatHeader {
            x: 12px;
            y: 12px;
            width: parent.width - 24px;
            backdrop-paused: root.scrolling;
            mobile: root.canvas-width < 600px;
            channel-name: "Morgan";
            conversation-kind: ConversationKind.server-dm;
            typing-indicator: "typing";
        }
        ChatHeaderMenu {
            show: root.menu-open;
            width: parent.width;
            height: parent.height;
            card-x: parent.width - self.card-width - 12px;
            card-y: 72px;
            show-video-call: true;
            show-settings: true;
            show-secret-chat: true;
        }
        MentionAutocomplete {
            show: root.mention-open;
            x: 20px;
            y: parent.height - self.height - 24px;
            max-popup-width: min(240px, parent.width - 40px);
        }
    }
}

fn advance(ms: u64) {
    let mut remaining = ms;
    while remaining > 0 {
        let step = remaining.min(16);
        testing::testing_backend::mock_elapsed_time(step);
        slint::platform::update_timers_and_animations();
        remaining -= step;
    }
}

fn frame(window: &ChromeHarness, name: &str) -> Vec<u8> {
    let pixels = window.window().take_snapshot().unwrap();
    if let Some(dir) = std::env::var_os("U3_CAPTURE_DIR") {
        image::save_buffer(
            std::path::Path::new(&dir).join(name),
            pixels.as_bytes(),
            pixels.width(),
            pixels.height(),
            image::ColorType::Rgba8,
        )
        .unwrap();
    }
    pixels.as_bytes().to_vec()
}

#[test]
fn chat_popovers_grow_from_their_anchors_in_both_themes_and_widths() {
    slint::platform::set_platform(Box::new(TestingBackend::new(TestingBackendOptions {
        mock_time: true,
        threading: false,
        renderer_name: Some("software".into()),
    })))
    .unwrap();
    let window = ChromeHarness::new().unwrap();
    window.show().unwrap();

    for theme in ["light", "dark"] {
        window.invoke_theme(theme.into());
        for (layout, width) in [("phone", 412.0), ("desktop", 900.0)] {
            window.set_canvas_width(width);
            window.set_menu_open(false);
            window.set_mention_open(false);
            window.set_scrolling(false);
            advance(500);
            frame(&window, &format!("{theme}-{layout}-01-rest.png"));
            window.set_scrolling(true);
            advance(360);
            frame(&window, &format!("{theme}-{layout}-01b-scrolling.png"));
            window.set_scrolling(false);
            advance(360);

            window.set_menu_open(true);
            advance(48);
            let menu_moving = frame(&window, &format!("{theme}-{layout}-02-menu-growing.png"));
            advance(500);
            let menu_settled = frame(&window, &format!("{theme}-{layout}-03-menu-settled.png"));
            assert_ne!(menu_moving, menu_settled, "menu must visibly settle");

            window.set_menu_open(false);
            advance(500);
            window.set_mention_open(true);
            advance(48);
            let mention_moving =
                frame(&window, &format!("{theme}-{layout}-04-mention-growing.png"));
            advance(500);
            let mention_settled =
                frame(&window, &format!("{theme}-{layout}-05-mention-settled.png"));
            assert_ne!(
                mention_moving, mention_settled,
                "mention popup must visibly settle"
            );
        }
    }
}

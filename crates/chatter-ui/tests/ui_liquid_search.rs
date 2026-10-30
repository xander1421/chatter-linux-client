//! Software renderer frames for the persistent unified search input.
use i_slint_backend_testing::{self as testing, TestingBackend, TestingBackendOptions};
use slint::ComponentHandle;

slint::slint! {
    import { UnifiedSearch, QuickSwitcherItem, SearchResult } from "../../ui/components/unified_search.slint";
    import { Colors } from "../../ui/theme/colors.slint";
    import { Appearance } from "../../ui/theme/appearance.slint";

    export component SearchHarness inherits Window {
        in-out property <length> viewport-width: 412px;
        width: viewport-width;
        height: 760px;
        background: Colors.background;
        in-out property <bool> open: false;
        in-out property <string> query: "";
        in-out property <string> category: "all";
        in-out property <bool> date-open: false;
        in property <[SearchResult]> example-hits: [
            { message-id: "m1", channel-id: "one", channel-name: "Avery", sender-name: "Avery", content-preview: "The project files are ready", timestamp: "10:42", highlight-start: 12, highlight-end: 17 }
        ];
        callback theme(string);
        theme(value) => { Appearance.theme = value; }
        Rectangle {
            width: 100%; height: 100%;
            background: Colors.background;
            Text { text: "Chats"; x: 24px; y: 90px; font-size: 28px; color: Colors.text-primary; }
            Rectangle { x: 12px; y: 12px; width: min(240px, parent.width - 24px); height: 52px;
                border-radius: 20px; background: Colors.inset;
                Text { text: "Search"; x: 40px; height: 100%; vertical-alignment: center; color: Colors.text-secondary; }
            }
        }
        search := UnifiedSearch {
            width: parent.width; height: parent.height;
            show: root.open;
            query <=> root.query;
            category <=> root.category;
            date-sheet-open <=> root.date-open;
            conversations: [
                { id: "one", name: "Avery", conv-type: "dm", presence-status: "online", unread-count: 2 },
                { id: "two", name: "Design team", conv-type: "channel", presence-status: "", unread-count: 0 }
            ];
            messages: root.query == "" ? [] : root.example-hits;
            message-kinds: ["messages"];
            recent-searches: ["design", "photos"];
            close => { root.open = false; }
            cancel => { root.query = ""; root.open = false; }
        }
    }
}

fn frame(window: &SearchHarness, name: &str) {
    let Some(dir) = std::env::var_os("S1_CAPTURE_DIR") else {
        return;
    };
    let pixels = window.window().take_snapshot().unwrap();
    image::save_buffer(
        std::path::Path::new(&dir).join(name),
        pixels.as_bytes(),
        pixels.width(),
        pixels.height(),
        image::ColorType::Rgba8,
    )
    .unwrap();
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

fn native_input(window: &SearchHarness) -> i_slint_core::item_tree::ItemRc {
    use std::ops::ControlFlow;
    let root = i_slint_core::window::WindowInner::from_pub(window.window())
        .window_item_rc()
        .unwrap();
    root.visit_descendants(|item| {
        if item.downcast::<i_slint_core::items::TextInput>().is_some() {
            ControlFlow::Break(item.clone())
        } else {
            ControlFlow::Continue(())
        }
    })
    .expect("persistent search TextInput")
}

#[test]
fn overlay_frames_keep_one_native_input_through_the_transition() {
    slint::platform::set_platform(Box::new(TestingBackend::new(TestingBackendOptions {
        mock_time: true,
        threading: false,
        renderer_name: Some("software".into()),
    })))
    .unwrap();
    let window = SearchHarness::new().unwrap();
    window.show().unwrap();
    let original_input = native_input(&window);
    for theme in ["light", "dark"] {
        window.invoke_theme(theme.into());
        for (width, label) in [(412., "phone"), (1100., "desktop")] {
            window
                .window()
                .set_size(slint::LogicalSize::new(width, 760.));
            window.set_viewport_width(width);
            window.set_open(false);
            window.set_query("".into());
            window.set_category("all".into());
            window.set_date_open(false);
            advance(500);
            frame(&window, &format!("{theme}-{label}-01-home.png"));
            window.set_open(true);
            advance(110);
            frame(&window, &format!("{theme}-{label}-02-levitate.png"));
            advance(450);
            frame(&window, &format!("{theme}-{label}-03-open.png"));
            window.set_query("files".into());
            advance(200);
            frame(&window, &format!("{theme}-{label}-04-results.png"));
            window.set_category("media".into());
            advance(200);
            frame(&window, &format!("{theme}-{label}-05-category.png"));
            window.set_date_open(true);
            advance(250);
            frame(&window, &format!("{theme}-{label}-06-date-sheet.png"));
            window.set_date_open(false);
            window.set_open(false);
            advance(100);
            frame(&window, &format!("{theme}-{label}-07-back.png"));
            assert!(native_input(&window) == original_input);
        }
    }
}

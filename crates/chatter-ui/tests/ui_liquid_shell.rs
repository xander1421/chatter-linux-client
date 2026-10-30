//! Frame captures for the bottom navigation drop and expanding search palette.
use i_slint_backend_testing::{self as testing, TestingBackend, TestingBackendOptions};
use slint::ComponentHandle;

slint::slint! {
    import { MobileBottomNav } from "../../ui/components/mobile_bottom_nav.slint";
    import { UnifiedSearch } from "../../ui/components/unified_search.slint";
    import { Colors } from "../../ui/theme/colors.slint";
    import { Appearance } from "../../ui/theme/appearance.slint";

    export component ShellHarness inherits Window {
        in-out property <length> viewport-width: 412px;
        in-out property <string> tab: "chats";
        in-out property <bool> search-open: false;
        width: viewport-width;
        height: 700px;
        background: Colors.background;
        callback theme(string);
        theme(value) => { Appearance.theme = value; }

        Rectangle {
            x: 16px; y: 90px;
            width: parent.width - 32px; height: 56px;
            border-radius: 20px;
            background: Colors.inset;
            Text { text: "Conversations"; color: Colors.text-primary; x: 20px; }
        }
        MobileBottomNav {
            y: parent.height - self.height;
            width: parent.width;
            active-tab: root.tab;
        }
        UnifiedSearch {
            width: parent.width;
            height: parent.height;
            show: root.search-open;
            conversations: [
                { id: "design", name: "Design team", conv-type: "private_group", presence-status: "online", unread-count: 2 },
                { id: "morgan", name: "Morgan", conv-type: "dm", presence-status: "online", unread-count: 0 },
            ];
            messages: [
                { message-id: "preview", channel-id: "design", channel-name: "Design team",
                    sender-name: "Morgan", content-preview: "The new navigation feels smooth",
                    timestamp: "09:41", highlight-start: 0, highlight-end: 0 },
            ];
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

fn frame(window: &ShellHarness, name: &str) {
    let dir = std::env::var_os("UI_SHELL_CAPTURE_DIR")
        .or_else(|| std::env::var_os("U4_CAPTURE_DIR"))
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("chatter-ui-liquid-shell"));
    std::fs::create_dir_all(&dir).unwrap();
    let pixels = window.window().take_snapshot().unwrap();
    image::save_buffer(
        dir.join(name),
        pixels.as_bytes(),
        pixels.width(),
        pixels.height(),
        image::ColorType::Rgba8,
    )
    .unwrap();
}

fn width(window: &ShellHarness, id: &str) -> f32 {
    testing::ElementHandle::find_by_element_id(window, id)
        .find(|item| item.size().width > 0.0)
        .unwrap_or_else(|| panic!("missing {id}"))
        .size()
        .width
}

fn search_input(window: &ShellHarness) -> i_slint_core::item_tree::ItemRc {
    use std::ops::ControlFlow;
    let root = i_slint_core::window::WindowInner::from_pub(window.window())
        .window_item_rc()
        .unwrap();
    root.visit_descendants(|item| {
        if item.downcast::<i_slint_core::items::TextInput>().is_some() {
            return ControlFlow::Break(item.clone());
        }
        ControlFlow::Continue(())
    })
    .expect("search TextInput")
}

#[test]
fn tab_drop_stretches_and_search_grows_on_phone_and_desktop() {
    slint::platform::set_platform(Box::new(TestingBackend::new(TestingBackendOptions {
        mock_time: true,
        threading: false,
        renderer_name: Some("software".into()),
    })))
    .unwrap();
    let window = ShellHarness::new().unwrap();
    window.show().unwrap();

    for (size, viewport) in [("phone", 412.0), ("desktop", 1100.0)] {
        window.set_viewport_width(viewport);
        window
            .window()
            .set_size(slint::LogicalSize::new(viewport, 700.0));
        for theme in ["light", "dark"] {
            window.invoke_theme(theme.into());
            window.set_tab("chats".into());
            window.set_search_open(false);
            advance(700);
            let original_input = search_input(&window);
            frame(&window, &format!("{size}-{theme}-01-rest.png"));
            let rest_width = width(&window, "MobileBottomNav::selection-drop");

            window.set_tab("profile".into());
            advance(64);
            frame(&window, &format!("{size}-{theme}-02-stretch.png"));
            let travel_width = width(&window, "MobileBottomNav::selection-drop");
            assert!(travel_width > rest_width + 1.0);
            advance(650);
            frame(&window, &format!("{size}-{theme}-03-settled.png"));

            window.set_search_open(true);
            advance(64);
            frame(&window, &format!("{size}-{theme}-04-search-grow.png"));
            let growing_width = width(&window, "UnifiedSearch::palette");
            advance(500);
            frame(&window, &format!("{size}-{theme}-05-search-open.png"));
            assert!(width(&window, "UnifiedSearch::palette") > growing_width);
            window.set_search_open(false);
            advance(400);
            assert!(search_input(&window) == original_input);
        }
    }
}

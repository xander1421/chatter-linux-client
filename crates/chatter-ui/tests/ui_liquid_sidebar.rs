//! Software-rendered captures of sidebar rows and the create card at two widths.
use i_slint_backend_testing::{self as testing, TestingBackend, TestingBackendOptions};
use slint::ComponentHandle;

slint::slint! {
    import { ChatRow } from "../../ui/sidebar/chat_list.slint";
    import { ConversationKind } from "../../ui/chat/conversation_kind.slint";
    import { ChannelListItem } from "../../ui/sidebar/channel_list_item.slint";
    import { PrivateGroupListItem } from "../../ui/sidebar/private_group_list_item.slint";
    import { MeshListItem } from "../../ui/mesh/mesh_list_item.slint";
    import { CreateMenu } from "../../ui/sidebar/create_menu.slint";
    import { Appearance } from "../../ui/theme/appearance.slint";
    import { Colors } from "../../ui/theme/colors.slint";

    export component SidebarHarness inherits Window {
        in-out property <bool> phone: true;
        in-out property <bool> menu-open: false;
        in-out property <int> unread: 0;
        in-out property <bool> selection: false;
        in-out property <bool> stress-preview: false;
        callback theme(string);
        theme(value) => { Appearance.theme = value; }
        callback scale(float);
        scale(value) => { Appearance.text-scale = value; }
        out property <length> first-row-bottom: dm-row.y + dm-row.height;
        out property <length> second-row-top: channel-row.y;
        out property <length> second-row-bottom: channel-row.y + channel-row.height;
        out property <length> third-row-top: group-row.y;
        width: root.phone ? 412px : 920px;
        height: 500px;
        background: Colors.background;

        Rectangle {
            x: root.phone ? 0px : 18px;
            width: root.phone ? 412px : 340px;
            height: parent.height;
            background: Colors.surface;
            VerticalLayout {
                padding-left: 8px;
                padding-right: 8px;
                padding-top: 50px;
                spacing: 4px;
                dm-row := ChatRow {
                    data: {
                        conversation-id: "demo-morgan",
                        conversation-type: ConversationKind.server-dm,
                        display-name: "Morgan",
                        initials: "MO",
                        last-message: root.stress-preview ? "You: rc117-secret-a\n```\n**\nrc117-secret-a" : "A message just arrived",
                        timestamp: "now",
                        unread-count: root.unread,
                        is-selected: root.selection,
                    };
                    mobile-mode: root.phone;
                }
                channel-row := ChannelListItem {
                    name: "Design notes";
                    last-message: root.stress-preview ? "First line\n**bold**\n```\ncode\n```" : "Preview of the newest post";
                    timestamp: "9:42";
                    mobile-mode: root.phone;
                }
                group-row := PrivateGroupListItem {
                    name: "Quiet circle";
                    last-message: root.stress-preview ? "A group message\n> continued on another line" : "See you this evening";
                    timestamp: "Yesterday";
                    mobile-mode: root.phone;
                }
                MeshListItem {
                    title: "Nearby mesh";
                    kind: "room";
                    timestamp: "2m";
                    touch: root.phone;
                }
            }
        }
        CreateMenu {
            show: root.menu-open;
            card-x: root.phone ? parent.width - self.card-width - 16px : 80px;
            card-y: root.phone ? parent.height - self.card-height - 20px : 56px;
        }
    }
}

#[test]
fn multiline_markdown_previews_stay_inside_each_row() {
    slint::platform::set_platform(Box::new(TestingBackend::new(TestingBackendOptions {
        mock_time: true,
        threading: false,
        renderer_name: Some("software".into()),
    }))).unwrap();
    for phone in [true, false] {
        for theme in ["light", "dark"] {
            let window = SidebarHarness::new().unwrap();
            window.set_phone(phone);
            window.invoke_theme(theme.into());
            window.invoke_scale(1.6);
            window.show().unwrap();
            advance(500);
            let prefix = format!("{}-{theme}", if phone { "phone" } else { "desktop" });
            frame(&window, &format!("{prefix}-06-before.png"));
            window.set_stress_preview(true);
            advance(32);
            frame(&window, &format!("{prefix}-07-multiline.png"));
            assert!(window.get_second_row_top() >= window.get_first_row_bottom());
            assert!(window.get_third_row_top() >= window.get_second_row_bottom());
        }
    }
}

fn advance(ms: u64) {
    for _ in 0..(ms / 16 + 1) {
        testing::testing_backend::mock_elapsed_time(16);
        slint::platform::update_timers_and_animations();
    }
}

fn frame(window: &SidebarHarness, name: &str) -> Vec<u8> {
    let pixels = window.window().take_snapshot().unwrap();
    assert_eq!(pixels.height(), 500);
    if let Some(dir) = std::env::var_os("U5c_CAPTURE_DIR")
        .or_else(|| std::env::var_os("SIDEBAR_CAPTURE_DIR"))
    {
        image::save_buffer(
            std::path::Path::new(&dir).join(name), pixels.as_bytes(),
            pixels.width(), pixels.height(), image::ColorType::Rgba8,
        ).unwrap();
    }
    pixels.as_bytes().to_vec()
}

fn swipe_first_row(window: &SidebarHarness, phone: bool) {
    use slint::platform::{PointerEventButton, WindowEvent};
    let start = slint::LogicalPosition::new(if phone { 340.0 } else { 300.0 }, 82.0);
    let end = slint::LogicalPosition::new(start.x - 190.0, start.y);
    window.window().dispatch_event(WindowEvent::PointerMoved { position: start });
    window.window().dispatch_event(WindowEvent::PointerPressed {
        position: start, button: PointerEventButton::Left,
    });
    window.window().dispatch_event(WindowEvent::PointerMoved { position: end });
    window.window().dispatch_event(WindowEvent::PointerReleased {
        position: end, button: PointerEventButton::Left,
    });
}

#[test]
fn sidebar_frames_show_spring_states_in_both_themes_and_widths() {
    slint::platform::set_platform(Box::new(TestingBackend::new(TestingBackendOptions {
        mock_time: true,
        threading: false,
        renderer_name: Some("software".into()),
    }))).unwrap();
    for phone in [true, false] {
        for theme in ["light", "dark"] {
            let window = SidebarHarness::new().unwrap();
            window.set_phone(phone);
            window.invoke_theme(theme.into());
            window.show().unwrap();
            window.set_menu_open(false);
            window.set_unread(0);
            window.set_selection(false);
            advance(500);
            let prefix = format!("{}-{theme}", if phone { "phone" } else { "desktop" });
            let before = frame(&window, &format!("{prefix}-01-before.png"));
            window.set_unread(3);
            window.set_selection(true);
            window.set_menu_open(true);
            advance(64);
            let entering = frame(&window, &format!("{prefix}-02-entering.png"));
            advance(500);
            let settled = frame(&window, &format!("{prefix}-03-settled.png"));
            assert!(before != settled, "sidebar state should be visible in {prefix}");
            assert!(entering != settled, "sidebar arrival should animate in {prefix}");
            window.set_menu_open(false);
            advance(400);
            let unswiped = frame(&window, &format!("{prefix}-04-rest.png"));
            swipe_first_row(&window, phone);
            advance(400);
            let swiped = frame(&window, &format!("{prefix}-05-swipe.png"));
            assert!(unswiped != swiped, "swipe actions should be visible in {prefix}");
        }
    }
}

#[test]
fn dragging_chat_rows_past_scroll_slop_clears_press_highlight() {
    use slint::platform::{PointerEventButton, WindowEvent};
    slint::platform::set_platform(Box::new(TestingBackend::new(TestingBackendOptions {
        mock_time: true,
        threading: false,
        renderer_name: Some("software".into()),
    }))).unwrap();
    let window = SidebarHarness::new().unwrap();
    window.set_phone(true);
    window.invoke_theme("light".into());
    window.show().unwrap();
    advance(500);
    let before = frame(&window, "drag-01-rest.png");
    // DM, channel, private group, and mesh rows: leave the pointer over the
    // following row, as a touch scroll does. No row should retain hover/press.
    for y in [82.0, 160.0, 238.0, 316.0] {
        let start = slint::LogicalPosition::new(210.0, y);
        let end = slint::LogicalPosition::new(210.0, y + 34.0);
        window.window().dispatch_event(WindowEvent::PointerPressed {
            position: start, button: PointerEventButton::Left,
        });
        advance(32);
        window.window().dispatch_event(WindowEvent::PointerMoved { position: end });
        advance(32);
        window.window().dispatch_event(WindowEvent::PointerReleased {
            position: end, button: PointerEventButton::Left,
        });
        advance(500);
    }
    let after = frame(&window, "drag-02-released.png");
    assert_eq!(before, after, "touch drag left a row painted as pressed or hovered");
}

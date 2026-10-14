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
        callback theme(string);
        theme(value) => { Appearance.theme = value; }
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
                ChatRow {
                    data: {
                        conversation-id: "demo-morgan",
                        conversation-type: ConversationKind.server-dm,
                        display-name: "Morgan",
                        initials: "MO",
                        last-message: "A message just arrived",
                        timestamp: "now",
                        unread-count: root.unread,
                        is-selected: root.selection,
                    };
                    mobile-mode: root.phone;
                }
                ChannelListItem {
                    name: "Design notes";
                    last-message: "Preview of the newest post";
                    timestamp: "9:42";
                    mobile-mode: root.phone;
                }
                PrivateGroupListItem {
                    name: "Quiet circle";
                    last-message: "See you this evening";
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

fn advance(ms: u64) {
    for _ in 0..(ms / 16 + 1) {
        testing::testing_backend::mock_elapsed_time(16);
        slint::platform::update_timers_and_animations();
    }
}

fn frame(window: &SidebarHarness, name: &str) -> Vec<u8> {
    let pixels = window.window().take_snapshot().unwrap();
    assert_eq!(pixels.height(), 500);
    if let Some(dir) = std::env::var_os("SIDEBAR_CAPTURE_DIR") {
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

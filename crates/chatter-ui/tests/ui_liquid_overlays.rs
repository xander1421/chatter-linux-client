//! Software-rendered frames for the shared overlay surfaces.
use i_slint_backend_testing::{self as testing, TestingBackend, TestingBackendOptions};
use slint::ComponentHandle;

slint::slint! {
    import { ModalContainer } from "../../ui/components/modal_container.slint";
    import { Toast } from "../../ui/components/toast.slint";
    import { ConnectionBanner } from "../../ui/components/connection_banner.slint";
    import { EmptyState } from "../../ui/components/empty_state.slint";
    import { Colors } from "../../ui/theme/colors.slint";
    import { Appearance } from "../../ui/theme/appearance.slint";

    export component OverlayHarness inherits Window {
        in-out property <length> capture-width: 412px;
        in-out property <bool> modal-open: false;
        in-out property <bool> page-open: false;
        in-out property <bool> toast-open: false;
        in-out property <string> status: "connected";
        width: capture-width;
        height: 720px;
        background: Colors.background;
        callback theme(string);
        theme(value) => { Appearance.theme = value; }

        ConnectionBanner {
            x: 0px;
            y: 0px;
            width: parent.width;
            connection-state: root.status;
        }
        EmptyState {
            x: 24px;
            y: 170px;
            width: parent.width - 48px;
            height: 160px;
            message: "No conversations yet";
            cta-text: "Start a conversation";
        }
        ModalContainer {
            x: 0px;
            y: 0px;
            show: root.modal-open;
            width: parent.width;
            height: parent.height;
            modal-width: 480px;
            max-modal-height: 380px;
            fill-height: true;
            accessible-title: "New conversation";
            Rectangle {
                height: 210px;
                Text {
                    x: 24px;
                    y: 24px;
                    text: "New conversation";
                    color: Colors.text-primary;
                }
            }
        }
        ModalContainer {
            x: 0px;
            y: 0px;
            show: root.page-open;
            full-screen: true;
            width: parent.width;
            height: parent.height;
            accessible-title: "Conversation details";
            VerticalLayout {
                padding-left: 24px;
                padding-right: 24px;
                padding-top: 80px;
                spacing: 12px;
                Text {
                    text: "Conversation details";
                    font-size: 24px;
                    color: Colors.text-primary;
                }
                Text {
                    text: "Members, media and settings";
                    color: Colors.text-secondary;
                }
            }
        }
        Toast {
            x: parent.width - self.width - 16px;
            y: 24px;
            max-toast-width: min(320px, parent.width - 32px);
            show-toast: root.toast-open;
            message: "Message sent";
            variant: "success";
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

fn frame(window: &OverlayHarness, name: &str) {
    let Some(dir) = std::env::var_os("U7_CAPTURE_DIR").or_else(|| {
        std::env::var_os("HOME").map(|home| {
            std::path::PathBuf::from(home)
                .join(".cache/chatter-rc103/reports/ui/U7")
                .into_os_string()
        })
    }) else {
        return;
    };
    std::fs::create_dir_all(std::path::Path::new(&dir)).unwrap();
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

#[test]
fn overlays_arrive_and_capture_both_layouts_and_themes() {
    slint::platform::set_platform(Box::new(TestingBackend::new(TestingBackendOptions {
        mock_time: true,
        threading: false,
        renderer_name: Some("software".into()),
    })))
    .unwrap();
    let window = OverlayHarness::new().unwrap();
    window.show().unwrap();
    for theme in ["light", "dark"] {
        window.invoke_theme(theme.into());
        for (layout, width) in [("phone", 412.0), ("desktop", 1000.0)] {
            window.set_capture_width(width);
            window
                .window()
                .set_size(slint::LogicalSize::new(width, 720.0));
            window.set_modal_open(false);
            window.set_page_open(false);
            window.set_toast_open(false);
            window.set_status("connected".into());
            advance(650);
            frame(&window, &format!("{theme}-{layout}-before.png"));
            let before = window.window().take_snapshot().unwrap().as_bytes().to_vec();
            window.set_status("disconnected".into());
            window.set_modal_open(true);
            window.set_toast_open(true);
            advance(80);
            frame(&window, &format!("{theme}-{layout}-entering.png"));
            advance(560);
            frame(&window, &format!("{theme}-{layout}-after.png"));
            let after = window.window().take_snapshot().unwrap().as_bytes().to_vec();
            assert!(before != after, "overlay did not change the rendered frame");
            window.set_modal_open(false);
            window.set_toast_open(false);
            window.set_page_open(true);
            advance(80);
            frame(&window, &format!("{theme}-{layout}-page-entering.png"));
            advance(480);
            frame(&window, &format!("{theme}-{layout}-page-after.png"));
            window.set_page_open(false);
            advance(100);
            frame(&window, &format!("{theme}-{layout}-page-exiting.png"));
        }
    }
}

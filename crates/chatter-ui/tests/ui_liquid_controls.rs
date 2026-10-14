//! Captures the shared controls at rest, pressed, and settled in both themes.
use i_slint_backend_testing::{self as testing, TestingBackend, TestingBackendOptions};
use slint::ComponentHandle;

slint::slint! {
    import { Button } from "../../ui/components/button.slint";
    import { Input } from "../../ui/components/input.slint";
    import { IconAction } from "../../ui/components/icon_action.slint";
    import { HeaderButton } from "../../ui/components/header_button.slint";
    import { GlassSurface } from "../../ui/components/glass_surface.slint";
    import { Colors } from "../../ui/theme/colors.slint";
    import { Icons } from "../../ui/theme/icons.slint";
    import { Appearance } from "../../ui/theme/appearance.slint";

    export component ControlsHarness inherits Window {
        in property <bool> desktop: false;
        out property <int> clicks: 0;
        callback theme(string);
        theme(value) => { Appearance.theme = value; }
        width: desktop ? 960px : 412px;
        height: 280px;
        background: Colors.background;

        GlassSurface {
            x: 12px;
            y: 12px;
            width: parent.width - 24px;
            height: 256px;
            material-level: 2;
            elevation: 3;
            Button {
                x: 16px;
                y: 20px;
                width: 150px;
                text: "Continue";
                touch: !root.desktop;
                clicked => { root.clicks += 1; }
            }
            Input {
                x: 16px;
                y: 88px;
                width: min(320px, parent.width - 32px);
                placeholder: "Message";
                control-height: 48px;
            }
            HeaderButton {
                x: 16px;
                y: 164px;
                height: self.width;
                label: "More";
                icon: Icons.more-horizontal;
                touch: !root.desktop;
            }
            IconAction {
                x: 88px;
                y: 164px;
                label: "Action";
                icon: Icons.check;
            }
        }
    }
}

fn advance(ms: u64) {
    for _ in 0..(ms / 16) {
        testing::testing_backend::mock_elapsed_time(16);
        slint::platform::update_timers_and_animations();
    }
}

fn frame(window: &ControlsHarness, name: &str) {
    let Some(dir) = std::env::var_os("U0_CAPTURE_DIR") else {
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

#[test]
fn shared_controls_press_and_release_in_both_themes_and_widths() {
    slint::platform::set_platform(Box::new(TestingBackend::new(TestingBackendOptions {
        mock_time: true,
        threading: false,
        renderer_name: Some("software".into()),
    })))
    .unwrap();
    let button = slint::platform::PointerEventButton::Left;

    for desktop in [false, true] {
        let width = if desktop { "desktop" } else { "phone" };
        for theme in ["light", "dark"] {
            let window = ControlsHarness::new().unwrap();
            window.show().unwrap();
            window.set_desktop(desktop);
            window.invoke_theme(theme.into());
            advance(320);
            frame(&window, &format!("{theme}-{width}-01-rest.png"));
            let position = slint::LogicalPosition::new(90.0, 56.0);
            window
                .window()
                .dispatch_event(slint::platform::WindowEvent::PointerPressed { position, button });
            advance(80);
            frame(&window, &format!("{theme}-{width}-02-press.png"));
            window
                .window()
                .dispatch_event(slint::platform::WindowEvent::PointerReleased { position, button });
            advance(320);
            frame(&window, &format!("{theme}-{width}-03-settled.png"));

            let position = slint::LogicalPosition::new(90.0, 124.0);
            window
                .window()
                .dispatch_event(slint::platform::WindowEvent::PointerPressed { position, button });
            window
                .window()
                .dispatch_event(slint::platform::WindowEvent::PointerReleased { position, button });
            advance(320);
            frame(&window, &format!("{theme}-{width}-04-input-focus.png"));

            for (name, position) in [
                ("header", slint::LogicalPosition::new(52.0, 200.0)),
                ("icon", slint::LogicalPosition::new(124.0, 200.0)),
            ] {
                window
                    .window()
                    .dispatch_event(slint::platform::WindowEvent::PointerPressed {
                        position,
                        button,
                    });
                advance(80);
                frame(&window, &format!("{theme}-{width}-05-{name}-press.png"));
                window
                    .window()
                    .dispatch_event(slint::platform::WindowEvent::PointerReleased {
                        position,
                        button,
                    });
                advance(320);
            }
            assert_eq!(window.get_clicks(), 1);
            window.hide().unwrap();
        }
    }
}

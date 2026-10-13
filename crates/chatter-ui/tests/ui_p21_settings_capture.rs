//! Phone Settings visual regression captures for the RC120 layout findings.
use i_slint_backend_testing as testing;
use slint::ComponentHandle;

slint::slint! {
    import { SettingsScreen } from "../../ui/settings/settings_screen.slint";
    export component SettingsCapture inherits Window {
        width: 360px;
        height: 740px;
        in-out property <string> section: "ai";
        SettingsScreen {
            width: parent.width;
            height: parent.height;
            mobile-platform: true;
            active-section <=> root.section;
        }
    }
}

fn pump(w: &SettingsCapture) {
    for _ in 0..3 {
        slint::Timer::single_shot(std::time::Duration::from_millis(30), || {
            slint::quit_event_loop().unwrap();
        });
        w.run().unwrap();
    }
}

fn click(w: &SettingsCapture, x: f32, y: f32) {
    let position = slint::LogicalPosition::new(x, y);
    let button = slint::platform::PointerEventButton::Left;
    w.window()
        .dispatch_event(slint::platform::WindowEvent::PointerPressed { position, button });
    w.window()
        .dispatch_event(slint::platform::WindowEvent::PointerReleased { position, button });
    pump(w);
}

fn capture(w: &SettingsCapture, name: &str) {
    let Ok(dir) = std::env::var("P21_CAPTURE_DIR") else {
        return;
    };
    let pixels = w.window().take_snapshot().unwrap();
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
fn phone_settings_ai_and_scrolled_tabs_render() {
    if std::env::var_os("P21_CAPTURE_DIR").is_none() {
        testing::init_integration_test_with_system_time();
    }
    let w = SettingsCapture::new().unwrap();
    w.show().unwrap();
    pump(&w);
    assert_eq!(w.get_section().as_str(), "ai");
    capture(&w, "p21-ai-after.png");

    w.set_section("account".into());
    pump(&w);
    capture(&w, "p21-tabs-start.png");
    click(&w, 340.0, 28.0);
    capture(&w, "p21-tabs-scrolled.png");
    click(&w, 185.0, 28.0);
    click(&w, 340.0, 28.0);
    capture(&w, "p21-tabs-after.png");
}

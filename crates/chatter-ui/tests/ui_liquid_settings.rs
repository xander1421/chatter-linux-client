//! Real Settings surface captures at phone and desktop sizes.
use i_slint_backend_testing::{self as testing, TestingBackend, TestingBackendOptions};
use slint::ComponentHandle;

slint::slint! {
    import { SettingsScreen } from "../../ui/settings/settings_screen.slint";
    import { Appearance } from "../../ui/theme/appearance.slint";

    export component SettingsHarness inherits Window {
        in-out property <bool> phone: true;
        in-out property <string> section: "account";
        width: phone ? 412px : 1000px;
        height: 760px;
        callback theme(string);
        theme(value) => { Appearance.theme = value; }

        settings := SettingsScreen {
            width: parent.width;
            height: parent.height;
            mobile-platform: root.phone;
            active-section <=> root.section;
            display-name: "Morgan Lee";
            username: "morgan";
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

fn capture(window: &SettingsHarness, name: &str) -> Vec<u8> {
    let pixels = window.window().take_snapshot().unwrap();
    if let Some(dir) = std::env::var_os("U6_CAPTURE_DIR") {
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
fn settings_sections_render_and_travel_in_both_themes_and_widths() {
    slint::platform::set_platform(Box::new(TestingBackend::new(TestingBackendOptions {
        mock_time: true,
        threading: false,
        renderer_name: Some("software".into()),
    })))
    .unwrap();
    let window = SettingsHarness::new().unwrap();
    window.show().unwrap();

    for theme in ["light", "dark"] {
        window.invoke_theme(theme.into());
        for (phone, size) in [(true, "phone"), (false, "desktop")] {
            window.set_phone(phone);
            window.window().set_size(slint::LogicalSize::new(
                if phone { 412.0 } else { 1000.0 },
                760.0,
            ));
            window.set_section("account".into());
            advance(500);
            let account = capture(&window, &format!("{theme}-{size}-01-account.png"));
            window.set_section("privacy".into());
            advance(64);
            let entering = capture(&window, &format!("{theme}-{size}-02-entering.png"));
            advance(400);
            let privacy = capture(&window, &format!("{theme}-{size}-03-privacy.png"));
            assert_ne!(account, privacy, "section navigation did not repaint");
            assert_ne!(entering, privacy, "section movement did not animate");
        }
    }
}

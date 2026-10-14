//! Focused software-renderer frames for Shield chrome and profile presence.
use i_slint_backend_testing::{self as testing, TestingBackend, TestingBackendOptions};
use slint::ComponentHandle;

slint::slint! {
    import { ShieldNetworkBanner } from "../../ui/anon_mode/shield_network_banner.slint";
    import { ShieldOnboardingToggle } from "../../ui/anon_mode/shield_onboarding.slint";
    import { AnonMode, TorStatus } from "../../ui/anon_mode/anon_mode_global.slint";
    import { Avatar } from "../../ui/components/avatar.slint";
    import { Colors } from "../../ui/theme/colors.slint";
    import { Appearance } from "../../ui/theme/appearance.slint";

    export component ShieldProfileHarness inherits Window {
        in-out property <bool> desktop: false;
        width: desktop ? 960px : 412px;
        height: 400px;
        background: Colors.background;
        callback theme(string);
        callback shield(bool, int);
        theme(value) => { Appearance.theme = value; }
        shield(enabled, state) => {
            AnonMode.enabled = enabled;
            AnonMode.shield-available = true;
            TorStatus.state = state;
        }

        banner := ShieldNetworkBanner {
            x: 0;
            y: 0;
            width: parent.width;
        }
        ShieldOnboardingToggle {
            x: 20px;
            y: banner.height + 32px;
            width: parent.width - 40px;
            height: 86px;
            touch: !root.desktop;
        }
        Avatar {
            x: 24px;
            y: 200px;
            size: 88px;
            fallback-text: "M";
            show-presence: true;
            presence-color: Colors.online;
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

fn frame(window: &ShieldProfileHarness, name: &str) {
    let Some(dir) = std::env::var_os("P32_CAPTURE_DIR") else {
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

fn banner_height(window: &ShieldProfileHarness) -> f32 {
    testing::ElementHandle::find_by_element_id(window, "ShieldProfileHarness::banner")
        .next()
        .unwrap()
        .size()
        .height
}

#[test]
fn shield_banner_morphs_at_phone_and_desktop_widths() {
    slint::platform::set_platform(Box::new(TestingBackend::new(TestingBackendOptions {
        mock_time: true,
        threading: false,
        renderer_name: Some("software".into()),
    })))
    .unwrap();
    let window = ShieldProfileHarness::new().unwrap();
    window.show().unwrap();
    for desktop in [false, true] {
        window.set_desktop(desktop);
        window.window().set_size(slint::LogicalSize::new(
            if desktop { 960.0 } else { 412.0 },
            400.0,
        ));
        assert_eq!(
            window.window().take_snapshot().unwrap().width(),
            if desktop { 960 } else { 412 }
        );
        for theme in ["light", "dark"] {
            window.invoke_theme(theme.into());
            window.invoke_shield(false, 0);
            advance(600);
            let width = if desktop { "desktop" } else { "phone" };
            frame(&window, &format!("{theme}-{width}-01-off.png"));
            assert!(banner_height(&window) < 1.0);
            window.invoke_shield(true, 4);
            advance(80);
            frame(&window, &format!("{theme}-{width}-02-travel.png"));
            let travel_height = banner_height(&window);
            assert!(travel_height > 0.0);
            advance(500);
            frame(&window, &format!("{theme}-{width}-03-warning.png"));
            assert!(banner_height(&window) >= travel_height);
            // The warning stays at a readable line length on both widths.
            assert!(banner_height(&window) > 40.0);
            window.invoke_shield(true, 2);
            advance(500);
            frame(&window, &format!("{theme}-{width}-04-connected.png"));
            assert!(banner_height(&window) < 1.0);
        }
    }
}

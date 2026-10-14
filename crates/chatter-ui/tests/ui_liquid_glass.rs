//! Captures busy content behind the real mobile dock and GlassSurface chrome.
use i_slint_backend_testing::{self as testing, TestingBackend, TestingBackendOptions};
use slint::ComponentHandle;

slint::slint! {
    import { MobileBottomNav } from "../../ui/components/mobile_bottom_nav.slint";
    import { GlassSurface } from "../../ui/components/glass_surface.slint";
    import { Materials } from "../../ui/theme/materials.slint";
    import { Colors } from "../../ui/theme/colors.slint";
    import { Appearance } from "../../ui/theme/appearance.slint";
    import { Spacing } from "../../ui/theme/spacing.slint";

    export component GlassHarness inherits Window {
        width: 412px;
        height: 780px;
        background: Colors.background;
        in property <bool> desktop: false;
        callback theme(string);
        callback glass(bool);
        theme(value) => { Appearance.theme = value; }
        glass(value) => { Appearance.glass-effects = value; }

        for row[i] in ["Ada Lovelace", "Group: Design", "Luca", "Weekend plans",
                       "Channels", "Morgan", "Photos", "River", "Release notes",
                       "Elena", "Monday", "Tess", "Community", "Nadia"]: Rectangle {
            x: root.desktop ? 28px : 12px;
            y: 18px + i * 54px;
            width: root.desktop ? 440px : parent.width - 24px;
            height: 46px;
            border-radius: 18px;
            background: Math.mod(i, 3) == 0 ? #16998b
                : (Math.mod(i, 3) == 1 ? #805cbb : #ba7b45);
            Text {
                x: 18px;
                text: row;
                color: white;
                vertical-alignment: center;
            }
        }

        if !root.desktop: MobileBottomNav {
            width: parent.width;
            y: parent.height - self.height;
            active-tab: "chats";
        }

        // The app FAB is inline in app.slint. Reuse its GlassSurface material
        // and geometry here, while the dock above is the production component.
        fab := GlassSurface {
            x: root.desktop ? 520px : parent.width - self.width - Spacing.xl;
            y: root.desktop ? 350px : parent.height - 150px;
            width: Spacing.fab-size;
            height: Spacing.fab-size;
            border-radius: self.width / 2;
            clip: true;
            material-level: 2;
            material-tint: Appearance.glass-effects ? Materials.fab-base : Colors.action-fill;
            background: transparent;
            Rectangle {
                x: 2px;
                y: 2px;
                width: parent.width - 4px;
                height: parent.height - 4px;
                border-radius: self.height / 2;
                background: Appearance.glass-effects ? Materials.fab-accent : transparent;
            }
            Text {
                text: "+";
                color: white;
                font-size: 32px;
                font-weight: 500;
                horizontal-alignment: center;
                vertical-alignment: center;
            }
        }

        if root.desktop: GlassSurface {
            x: 520px;
            y: 80px;
            width: 390px;
            height: 56px;
            border-radius: self.height / 2;
            material-level: 2;
            clip: true;
            background: transparent;
            Text { text: "Search"; x: 22px; color: Colors.text-muted; vertical-alignment: center; }
        }
    }
}

fn capture(window: &GlassHarness, name: &str) {
    let Some(dir) = std::env::var_os("G1_CAPTURE_DIR") else { return; };
    let pixels = window.window().take_snapshot().unwrap();
    image::save_buffer(
        std::path::Path::new(&dir).join(name),
        pixels.as_bytes(),
        pixels.width(),
        pixels.height(),
        image::ColorType::Rgba8,
    ).unwrap();
}

#[test]
fn captures_chrome_over_busy_list_in_both_themes_and_widths() {
    slint::platform::set_platform(Box::new(TestingBackend::new(TestingBackendOptions {
        mock_time: true,
        threading: false,
        renderer_name: Some("software".into()),
    }))).unwrap();
    let window = GlassHarness::new().unwrap();
    window.show().unwrap();
    for (width, height, desktop) in [(412, 780, false), (960, 600, true)] {
        window.window().set_size(slint::PhysicalSize::new(width, height));
        window.set_desktop(desktop);
        for theme in ["light", "dark"] {
            window.invoke_theme(theme.into());
            window.invoke_glass(false);
            testing::testing_backend::mock_elapsed_time(500);
            slint::platform::update_timers_and_animations();
            capture(&window, &format!("{theme}-{width}-before.png"));
            window.invoke_glass(true);
            testing::testing_backend::mock_elapsed_time(500);
            slint::platform::update_timers_and_animations();
            capture(&window, &format!("{theme}-{width}-after.png"));
        }
    }
}

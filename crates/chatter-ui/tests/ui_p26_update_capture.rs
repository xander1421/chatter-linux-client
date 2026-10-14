//! Headless phone and desktop captures of the update banner and About error.
use i_slint_backend_testing::{TestingBackend, TestingBackendOptions};
use slint::{ComponentHandle, LogicalSize};

slint::slint! {
    import { Sidebar } from "../../ui/sidebar/sidebar.slint";
    import { SettingsScreen } from "../../ui/settings/settings_screen.slint";
    import { MobileBottomNav } from "../../ui/components/mobile_bottom_nav.slint";
    import { UpdateGlobal } from "../../ui/update/update_global.slint";
    import { Appearance } from "../../ui/theme/appearance.slint";
    import { Colors } from "../../ui/theme/colors.slint";

    export component UpdateCapture inherits Window {
        background: Colors.background;
        in-out property <bool> phone: true;
        in-out property <bool> about: false;
        callback scenario(string, bool);
        scenario(theme, show-about) => {
            Appearance.theme = theme;
            root.about = show-about;
            UpdateGlobal.state = show-about ? "error" : "available";
            UpdateGlobal.version = "0.1.0-rc123";
            UpdateGlobal.error = "Update failed: invalid artifact signature";
            UpdateGlobal.installed-version = "0.1.0-rc122";
        }
        if !root.about: Sidebar {
            width: root.phone ? parent.width : 300px;
            height: parent.height;
            mobile-mode: root.phone;
            bottom-inset: root.phone ? 64px : 0px;
        }
        if root.about: SettingsScreen {
            width: parent.width;
            height: parent.height;
            mobile-platform: root.phone;
            active-section: "about";
            bottom-inset: root.phone ? 64px : 0px;
        }
        if root.phone && !root.about: MobileBottomNav {
            width: parent.width;
            y: parent.height - self.height;
        }
    }
}

#[test]
fn capture_update_placement() {
    slint::platform::set_platform(Box::new(TestingBackend::new(TestingBackendOptions {
        mock_time: true,
        threading: false,
        renderer_name: Some("software".into()),
    }))).unwrap();
    let window = UpdateCapture::new().unwrap();
    let directory = std::env::var("P26_CAPTURE_DIR").ok();
    for phone in [true, false] {
        window.set_phone(phone);
        window.window().set_size(if phone { LogicalSize::new(360., 740.) } else { LogicalSize::new(1100., 800.) });
        for theme in ["light", "dark"] {
            for about in [false, true] {
                window.invoke_scenario(theme.into(), about);
                let pixels = window.window().take_snapshot().unwrap();
                let name = format!("p26-after-{}-{theme}-{}.png",
                    if phone { "phone" } else { "desktop" },
                    if about { "about-error" } else { "banner" });
                assert_eq!(pixels.width(), if phone { 360 } else { 1100 });
                assert_eq!(pixels.height(), if phone { 740 } else { 800 });
                if let Some(directory) = &directory {
                    image::save_buffer(
                        std::path::Path::new(directory).join(name),
                        pixels.as_bytes(), pixels.width(), pixels.height(), image::ColorType::Rgba8,
                    ).unwrap();
                }
            }
        }
    }
}

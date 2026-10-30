//! Headless frame captures for onboarding surfaces at phone and desktop widths.
use i_slint_backend_testing::{self as testing, TestingBackend, TestingBackendOptions};
use slint::ComponentHandle;

slint::slint! {
    import { WelcomePage } from "../../ui/pages/welcome.slint";
    import { LoadingPage } from "../../ui/pages/loading.slint";
    import { LinkDevicePage } from "../../ui/pages/link_device.slint";
    import { LoginPage } from "../../ui/pages/login.slint";
    import { RegisterPage } from "../../ui/pages/register.slint";
    import { AccountRecoveryPage } from "../../ui/pages/account_recovery.slint";
    import { Colors } from "../../ui/theme/colors.slint";
    import { Appearance } from "../../ui/theme/appearance.slint";

    export component OnboardingHarness inherits Window {
        in property <bool> desktop: false;
        width: root.desktop ? 1000px : 412px;
        height: 860px;
        background: Colors.background;
        in property <string> scene: "welcome";
        in property <bool> link-ready: false;
        in property <image> fake-qr;
        callback theme(string);
        theme(value) => { Appearance.theme = value; }

        if root.scene == "welcome": WelcomePage {
            width: parent.width;
            height: parent.height;
            display-name: "Taylor";
        }
        if root.scene == "loading": LoadingPage {
            width: parent.width;
            height: parent.height;
            message: "Securing your account";
            secondary-message: "Preparing encrypted messages";
        }
        if root.scene == "link": LinkDevicePage {
            width: parent.width;
            height: parent.height;
            linking: root.link-ready;
            qr-image: root.fake-qr;
            link-url: root.link-ready ? "chatter://link?uuid=fake-preview" : "";
            status-text: root.link-ready ? "Waiting for approval on your other device" : "Ready when you are";
        }
        if root.scene == "login": LoginPage {
            width: parent.width;
            height: parent.height;
            configured-server-url: "https://example.test";
        }
        if root.scene == "register": RegisterPage {
            width: parent.width;
            height: parent.height;
            server-url: "https://example.test";
        }
        if root.scene == "recovery": AccountRecoveryPage {
            width: parent.width;
            height: parent.height;
        }
    }
}

fn advance(ms: u64) {
    let mut left = ms;
    while left > 0 {
        let step = left.min(16);
        testing::testing_backend::mock_elapsed_time(step);
        slint::platform::update_timers_and_animations();
        left -= step;
    }
}

fn frame(window: &OnboardingHarness, name: &str) {
    let pixels = window.window().take_snapshot().unwrap();
    let bytes = pixels.as_bytes();
    assert!(
        bytes.chunks_exact(4).any(|pixel| pixel != &bytes[..4]),
        "{name} rendered as a flat surface"
    );
    let Some(dir) = std::env::var_os("U10_CAPTURE_DIR") else {
        return;
    };
    image::save_buffer(
        std::path::Path::new(&dir).join(name),
        pixels.as_bytes(),
        pixels.width(),
        pixels.height(),
        image::ColorType::Rgba8,
    )
    .unwrap();
}

fn fake_qr() -> slint::Image {
    let path = std::env::temp_dir().join("chatter-u10-fake-qr.png");
    let pixels = image::RgbaImage::from_fn(256, 256, |x, y| {
        let cell_x = x / 8;
        let cell_y = y / 8;
        let finder = |cx: u32, cy: u32| {
            let dx = cell_x.abs_diff(cx);
            let dy = cell_y.abs_diff(cy);
            dx <= 3 && dy <= 3 && (dx == 3 || dy == 3 || (dx <= 1 && dy <= 1))
        };
        let dark = finder(4, 4)
            || finder(27, 4)
            || finder(4, 27)
            || ((cell_x * 17 + cell_y * 31 + cell_x * cell_y) % 7 < 3);
        if dark {
            image::Rgba([24, 29, 38, 255])
        } else {
            image::Rgba([255, 255, 255, 255])
        }
    });
    pixels.save(&path).unwrap();
    slint::Image::load_from_path(&path).unwrap()
}

#[test]
fn onboarding_surfaces_render_across_theme_and_width() {
    slint::platform::set_platform(Box::new(TestingBackend::new(TestingBackendOptions {
        mock_time: true,
        threading: false,
        renderer_name: Some("software".into()),
    })))
    .unwrap();
    let window = OnboardingHarness::new().unwrap();
    window.show().unwrap();
    window.set_fake_qr(fake_qr());
    for theme in ["light", "dark"] {
        window.invoke_theme(theme.into());
        for (width, size) in [(412, "phone"), (1000, "desktop")] {
            window.set_desktop(width == 1000);
            window
                .window()
                .set_size(slint::LogicalSize::new(width as f32, 860.0));
            window.set_scene("welcome".into());
            advance(48);
            frame(&window, &format!("{theme}-{size}-welcome-early.png"));
            advance(600);
            frame(&window, &format!("{theme}-{size}-welcome-settled.png"));
            window.set_scene("loading".into());
            advance(160);
            frame(&window, &format!("{theme}-{size}-loading.png"));
            for scene in ["login", "register", "recovery"] {
                window.set_scene(scene.into());
                advance(360);
                frame(&window, &format!("{theme}-{size}-{scene}.png"));
            }
            window.set_scene("link".into());
            window.set_link_ready(false);
            frame(&window, &format!("{theme}-{size}-link-empty.png"));
            window.set_link_ready(true);
            advance(360);
            frame(&window, &format!("{theme}-{size}-link-ready.png"));
        }
    }
}

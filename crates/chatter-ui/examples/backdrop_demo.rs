//! Run with `cargo run -p chatter-ui --example backdrop_demo --features dev-trust-root -j 2`.
//! Lightweight host test: `cargo test --manifest-path vendor/i-slint-core/tests/backdrop-demo/Cargo.toml --target-dir target -j 2`.
//! Set SLINT_BACKDROP_EFFECTS=0 before startup to compare the effect cost.
use slint::ComponentHandle;
slint::slint! {
    export component BackdropDemo inherits Window {
        width: 480px;
        height: 360px;
        background: #192742;
        in-out property <float> phase;
        in-out property <bool> effects: true;
        for stripe in 12: Rectangle {
            x: mod(stripe * 70px + root.phase * 2px, 840px) - 150px;
            y: -80px;
            width: 40px;
            height: 520px;
            transform-rotation: 25deg;
            background: stripe / 2 == floor(stripe / 2) ? #ef5580 : #49aedd;
        }
        Backdrop {
            x: 60px; y: 65px; width: 360px; height: 230px;
            border-radius: 32px;
            blur: root.effects ? 14px : 0px;
            saturation: root.effects ? 1.7 : 1;
            brightness: root.effects ? 0.06 : 0;
            refraction: root.effects ? 0.65 : 0;
            Rectangle { background: #ffffff20; border-radius: 32px; }
            Text {
                text: root.effects ? "Backdrop effects ON\nClick to compare" : "Backdrop effects OFF\nClick to compare";
                color: white; font-size: 22px;
                horizontal-alignment: center; vertical-alignment: center;
            }
            TouchArea { clicked => { root.effects = !root.effects; } }
        }
    }
}
fn main() -> Result<(), slint::PlatformError> {
    let window = BackdropDemo::new()?;
    let weak = window.as_weak();
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_millis(16),
        move || {
            if let Some(window) = weak.upgrade() {
                window.set_phase((window.get_phase() + 1.0) % 420.0);
            }
        },
    );
    window.run()
}

// `cargo test -p chatter-ui --example backdrop_demo --no-default-features \
//    --features windows-cross,dev-trust-root -j 2`
// windows-cross selects the existing software-renderer feature (also usable on Linux).
#[cfg(all(test, feature = "windows-cross"))]
mod tests {
    use super::*;
    use slint::platform::{
        Platform, WindowAdapter,
        software_renderer::{MinimalSoftwareWindow, RepaintBufferType},
    };
    use std::rc::Rc;

    slint::slint! {
        export component FallbackWindow inherits Window {
            width: 64px; height: 64px; background: blue;
            Backdrop {
                x: 8px; y: 8px; width: 48px; height: 48px;
                blur: 12px; saturation: 1.8; brightness: 0.3;
                refraction: 0.6; border-radius: 8px;
                Rectangle { x: 12px; y: 12px; width: 24px; height: 24px; background: red; }
            }
        }
    }
    struct Headless(Rc<MinimalSoftwareWindow>);
    impl Platform for Headless {
        fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
            Ok(self.0.clone())
        }
    }
    #[test]
    fn software_fallback_paints_children_without_effects() {
        let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        slint::platform::set_platform(Box::new(Headless(adapter.clone()))).unwrap();
        let window = FallbackWindow::new().unwrap();
        window.show().unwrap();
        adapter.set_size(slint::PhysicalSize::new(64, 64));
        let mut pixels = vec![slint::Rgb8Pixel::default(); 64 * 64];
        adapter.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, 64);
        });
        assert_eq!(pixels[32 * 64 + 32], slint::Rgb8Pixel::new(255, 0, 0));
        assert_eq!(pixels[10 * 64 + 10], slint::Rgb8Pixel::new(0, 0, 255));
    }
}

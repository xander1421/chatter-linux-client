//! Headless runtime and geometry check of the production update notice.
//! CHATTER_NULL_AUDIO=1 cargo run -p chatter-ui --no-default-features \
//!   --features desktop,dev-trust-root,quic,ui-test-debug,chatter-core/insecure-test-anchors \
//!   --example update_offer_preview
use chatter_ui::{Appearance, AuthFlow, ChatPane, MainWindow, SidebarPane, UpdateGlobal};
use i_slint_backend_testing::{ElementHandle, TestingBackend, TestingBackendOptions};
use slint::{ComponentHandle, LogicalSize};

fn main() {
    slint::platform::set_platform(Box::new(TestingBackend::new(TestingBackendOptions {
        mock_time: true,
        threading: false,
        renderer_name: Some("software".into()),
    })))
    .unwrap();
    let window = MainWindow::new().unwrap();
    window.set_current_view("main".into());
    window.show().unwrap();
    let actions = std::rc::Rc::new(std::cell::Cell::new(0usize));
    let calls = actions.clone();
    window
        .global::<UpdateGlobal>()
        .on_update(move || calls.set(calls.get() + 1));
    let out = std::path::PathBuf::from(
        std::env::var("UPDATE_PREVIEW_DIR")
            .unwrap_or_else(|_| "/tmp/chatter-update-preview".into()),
    );
    std::fs::create_dir_all(&out).unwrap();
    window.global::<UpdateGlobal>().set_version("0.1.58".into());
    window
        .global::<ChatPane>()
        .set_channel_name("Preview conversation".into());
    for (name, size, chat, signed_in) in [
        ("phone-list", LogicalSize::new(360., 740.), false, true),
        ("phone-chat", LogicalSize::new(360., 740.), true, true),
        ("phone-large-text", LogicalSize::new(360., 740.), true, true),
        ("desktop", LogicalSize::new(1100., 800.), false, true),
        (
            "phone-signed-out",
            LogicalSize::new(360., 740.),
            false,
            false,
        ),
    ] {
        window
            .global::<Appearance>()
            .set_text_scale(if name == "phone-large-text" { 1.3 } else { 1.0 });
        window.global::<AuthFlow>().set_is_signed_in(signed_in);
        window.set_is_mobile_platform(name.starts_with("phone"));
        window.window().set_size(size);
        window.global::<SidebarPane>().set_mobile_show_chat(chat);
        for state in ["available", "downloading", "ready", "idle"] {
            window.global::<UpdateGlobal>().set_state(state.into());
            let _ = window.window().take_snapshot().unwrap();
            for _ in 0..40 {
                i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(16));
                slint::platform::update_timers_and_animations();
            }
            // A fresh output size forces a complete frame rather than only
            // the software renderer's damage from the preceding scenario.
            window
                .window()
                .set_size(LogicalSize::new(size.width + 1., size.height));
            let _ = window.window().take_snapshot().unwrap();
            window.window().set_size(size);
            let pixels = window.window().take_snapshot().unwrap();
            let notices: Vec<_> = ElementHandle::find_by_accessible_label(&window, "App update")
                .filter(|item| item.size().width > 0. && item.size().height > 0.)
                .collect();
            if state == "idle" {
                assert!(notices.is_empty());
                continue;
            }
            assert_eq!(notices.len(), 1, "{name}: duplicate or missing notice");
            let notice = &notices[0];
            let position = notice.absolute_position();
            let bounds = notice.size();
            assert!(
                position.y >= 0. && position.y + bounds.height < 120.,
                "{name}: notice is not at the top: {position:?} {bounds:?}"
            );
            assert!(
                position.x >= 0. && position.x + bounds.width <= size.width + 1.,
                "{name}: notice extends outside the viewport"
            );
            image::save_buffer(
                out.join(format!("{name}-{state}.png")),
                pixels.as_bytes(),
                pixels.width(),
                pixels.height(),
                image::ColorType::Rgba8,
            )
            .unwrap();
            let label = if state == "ready" {
                "Restart"
            } else {
                "Update"
            };
            let button = ElementHandle::find_by_accessible_label(&window, label)
                .find(|item| item.size().width > 0. && item.size().height > 0.)
                .unwrap();
            let bp = button.absolute_position();
            let bs = button.size();
            let click = slint::LogicalPosition::new(bp.x + bs.width / 2., bp.y + bs.height / 2.);
            println!("{name}/{state}: notice {position:?} {bounds:?}, button {bp:?} {bs:?}");
            let before = actions.get();
            window
                .window()
                .dispatch_event(slint::platform::WindowEvent::PointerMoved { position: click });
            window
                .window()
                .dispatch_event(slint::platform::WindowEvent::PointerPressed {
                    position: click,
                    button: slint::platform::PointerEventButton::Left,
                });
            window
                .window()
                .dispatch_event(slint::platform::WindowEvent::PointerReleased {
                    position: click,
                    button: slint::platform::PointerEventButton::Left,
                });
            assert_eq!(
                actions.get() - before,
                if state == "downloading" { 0 } else { 1 },
                "{name}: update action is covered or incorrectly enabled"
            );
        }
    }
    println!(
        "Update notice geometry passed for phone list, phone chat and desktop; previews: {}",
        out.display()
    );
}

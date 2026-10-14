//! Focused geometry and frame captures for the composer droplet transition.
use i_slint_backend_testing::{self as testing, TestingBackend, TestingBackendOptions};
use slint::ComponentHandle;

slint::slint! {
    import { MessageComposer } from "../../ui/chat/message_composer.slint";
    import { Colors } from "../../ui/theme/colors.slint";
    import { Appearance } from "../../ui/theme/appearance.slint";

    export component LiquidHarness inherits Window {
        width: 420px;
        height: 270px;
        background: Colors.background;
        in-out property <string> draft: "";
        in-out property <bool> replying: false;
        callback theme(string);
        callback motion(bool);
        theme(value) => { Appearance.theme = value; }
        motion(value) => { Appearance.reduce-motion = value; }

        composer := MessageComposer {
            x: 16px;
            y: parent.height - self.height - 16px;
            width: parent.width - 32px;
            mobile: true;
            text <=> root.draft;
            is-replying: root.replying;
            reply-to-sender: "Morgan";
            reply-to-preview: "A message between the droplets";
        }
    }
}

fn frame(window: &LiquidHarness, name: &str) {
    let Some(dir) = std::env::var_os("P31_CAPTURE_DIR") else {
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

fn advance(ms: u64) {
    testing::testing_backend::mock_elapsed_time(ms);
    slint::platform::update_timers_and_animations();
}

fn rect(window: &LiquidHarness, id: &str) -> (f32, f32, f32, f32) {
    let element = testing::ElementHandle::find_by_element_id(window, id)
        .find(|item| item.size().width > 0.0 && item.size().height > 0.0)
        .unwrap_or_else(|| panic!("missing composer element {id}"));
    let p = element.absolute_position();
    let s = element.size();
    (p.x, p.y, s.width, s.height)
}

fn native_input(window: &LiquidHarness) -> i_slint_core::item_tree::ItemRc {
    use std::ops::ControlFlow;
    let root = i_slint_core::window::WindowInner::from_pub(window.window())
        .window_item_rc()
        .unwrap();
    root.visit_descendants(|item| {
        if item.downcast::<i_slint_core::items::TextInput>().is_some() {
            return ControlFlow::Break(item.clone());
        }
        ControlFlow::Continue(())
    })
    .expect("native composer TextInput")
}

#[test]
fn droplets_rise_retarget_and_fall_around_the_same_input() {
    slint::platform::set_platform(Box::new(TestingBackend::new(TestingBackendOptions {
        mock_time: true,
        threading: false,
        renderer_name: Some("software".into()),
    })))
    .unwrap();
    let window = LiquidHarness::new().unwrap();
    window.show().unwrap();
    window.invoke_theme("light".into());
    frame(&window, "01-rest.png");
    let original_input = native_input(&window);
    let (ix, iy, iw, ih) = rect(&window, "MessageComposer::text-input");
    let position = slint::LogicalPosition::new(ix + iw / 2.0, iy + ih / 2.0);
    let button = slint::platform::PointerEventButton::Left;
    window
        .window()
        .dispatch_event(slint::platform::WindowEvent::PointerPressed { position, button });
    window
        .window()
        .dispatch_event(slint::platform::WindowEvent::PointerReleased { position, button });
    assert!(original_input
        .downcast::<i_slint_core::items::TextInput>()
        .unwrap()
        .as_pin_ref()
        .has_focus());

    window.set_draft("A".into());
    advance(90);
    frame(&window, "02-rise.png");
    let rise_gap = rect(&window, "MessageComposer::field-surface").1
        - rect(&window, "MessageComposer::left-action-surface").1;
    window.set_draft("".into());
    advance(70);
    frame(&window, "02b-reverse.png");
    let reverse_gap = rect(&window, "MessageComposer::field-surface").1
        - rect(&window, "MessageComposer::left-action-surface").1;
    assert!(native_input(&window) == original_input);
    assert!(original_input
        .downcast::<i_slint_core::items::TextInput>()
        .unwrap()
        .as_pin_ref()
        .has_focus());
    window.set_draft("A".into());
    advance(100);
    frame(&window, "03-mid-neck.png");
    let mid_gap = rect(&window, "MessageComposer::field-surface").1
        - rect(&window, "MessageComposer::left-action-surface").1;
    advance(400);
    frame(&window, "04-raised.png");
    let raised_gap = rect(&window, "MessageComposer::field-surface").1
        - rect(&window, "MessageComposer::left-action-surface").1;
    assert!(rise_gap > reverse_gap && mid_gap > reverse_gap);
    assert!(rise_gap > 0.0 && mid_gap < raised_gap + 3.0);
    let (_, neck_y, neck_w, neck_h) = rect(&window, "MessageComposer::left-neck");
    let (_, field_y, _, _) = rect(&window, "MessageComposer::field-surface");
    assert!(neck_w >= 12.0 && neck_h >= 4.0 && neck_y + neck_h >= field_y);

    window.set_replying(true);
    advance(180);
    frame(&window, "05-reply-chip.png");
    let (chip_x, _, chip_w, _) = rect(&window, "MessageComposer::reply-chip");
    let (right_x, _, _, _) = rect(&window, "MessageComposer::right-action-surface");
    assert!(chip_w > 100.0 && chip_x + chip_w < right_x);
    window.set_replying(false);
    window.set_draft("".into());
    advance(140);
    frame(&window, "06-fall.png");
    let fall_gap = rect(&window, "MessageComposer::field-surface").1
        - rect(&window, "MessageComposer::left-action-surface").1;
    assert!(fall_gap < raised_gap);
    advance(450);
    frame(&window, "07-restored.png");

    window.set_draft("A".into());
    advance(550);
    window.invoke_motion(true);
    advance(100);
    frame(&window, "08-near-touching.png");
    window.invoke_motion(false);
    advance(550);
    frame(&window, "09-connected.png");
    window.invoke_theme("dark".into());
    frame(&window, "10-dark-connected.png");
    window.set_replying(true);
    advance(450);
    frame(&window, "11-dark-chip.png");

    // Reversal uses the same native TextInput item; Slint would recreate it
    // only if this count changed while the conditional shells appeared.
    let inputs: Vec<_> =
        testing::ElementHandle::find_by_element_id(&window, "MessageComposer::text-input")
            .filter(|e| e.size().height > 0.0)
            .collect();
    assert_eq!(inputs.len(), 1);
    assert!(native_input(&window) == original_input);
    assert!(original_input
        .downcast::<i_slint_core::items::TextInput>()
        .unwrap()
        .as_pin_ref()
        .has_focus());
}

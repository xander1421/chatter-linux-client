//! Focused geometry and frame captures for the composer droplet transition.
use i_slint_backend_testing::{self as testing, TestingBackend, TestingBackendOptions};
use slint::ComponentHandle;

slint::slint! {
    import { MessageComposer } from "../../ui/chat/message_composer.slint";
    import { Colors } from "../../ui/theme/colors.slint";
    import { Appearance } from "../../ui/theme/appearance.slint";
    import { PendingAttachmentData } from "../../ui/chat/composer_types.slint";

    export component LiquidHarness inherits Window {
        width: 420px;
        height: 270px;
        background: Colors.background;
        in-out property <string> draft: "";
        in-out property <bool> replying: false;
        in property <[PendingAttachmentData]> attachments;
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
            pending-attachments: root.attachments;
            reply-to-sender: "Morgan";
            reply-to-preview: "A message between the droplets";
        }
    }
}

#[test]
fn attachment_rows_fill_available_width_and_center_previews() {
    slint::platform::set_platform(Box::new(TestingBackend::new(TestingBackendOptions {
        mock_time: true,
        threading: false,
        renderer_name: Some("software".into()),
    }))).unwrap();
    let window = LiquidHarness::new().unwrap();
    window.show().unwrap();
    let input = native_input(&window);
    let mut pixels = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(32, 32);
    pixels.make_mut_slice().fill(slint::Rgba8Pixel::new(241, 17, 139, 255));
    let thumbnail = slint::Image::from_rgba8(pixels);
    for width in [320.0, 768.0] {
        window.window().set_size(slint::LogicalSize::new(width, 270.0));
        for theme in ["light", "dark"] {
            window.invoke_theme(theme.into());
            for count in [1, 8] {
                window.set_attachments(slint::ModelRc::new(slint::VecModel::from(
                    (0..count).map(|i| PendingAttachmentData {
                        id: format!("fixture-{i}").into(), filename: format!("photo-{i}.jpg").into(),
                        size_display: "4 MB".into(), category: "image".into(),
                        is_ready: true, has_thumbnail: true, thumbnail: thumbnail.clone(),
                    }).collect::<Vec<_>>()
                )));
                advance(600);
                let frame = window.window().take_snapshot().unwrap();
                let (x, _, w, _) = rect(&window, "ComposerAttachmentStrip::attachment-scroll");
                let action = |label| testing::ElementHandle::find_by_accessible_label(&window, label)
                    .find(|e| e.size().width > 0.0).expect("visible composer action");
                let left = action("Emoji & GIF picker");
                let right = action("Attach file");
                assert!(x <= left.absolute_position().x + 8.0, "attachment row left inset at {width}px/{count}");
                assert!(x + w >= right.absolute_position().x + right.size().width - 8.0,
                    "attachment row leaves reserved action space at {width}px/{count}");
                let remove = action("Remove attachment");
                let ys: Vec<_> = frame.as_bytes().chunks_exact(4).enumerate()
                    .filter(|(_, p)| p[..3] == [241, 17, 139])
                    .map(|(i, _)| (i / frame.width() as usize) as f32).collect();
                assert!(!ys.is_empty(), "fixture preview rendered");
                let center = (ys.iter().copied().fold(f32::INFINITY, f32::min)
                    + ys.iter().copied().fold(f32::NEG_INFINITY, f32::max) + 1.0) / 2.0;
                let card_center = remove.absolute_position().y + remove.size().height / 2.0;
                assert!((center - card_center).abs() <= 1.0,
                    "preview is not centered: {center} vs {card_center} at {width}px/{count}");
                assert!(native_input(&window) == input, "attachment changes retain the native input");
            }
        }
    }
}

fn frame(window: &LiquidHarness, name: &str) {
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

fn advance(ms: u64) {
    let mut remaining = ms;
    while remaining > 0 {
        let step = remaining.min(16);
        testing::testing_backend::mock_elapsed_time(step);
        slint::platform::update_timers_and_animations();
        remaining -= step;
    }
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
    assert!(
        original_input
            .downcast::<i_slint_core::items::TextInput>()
            .unwrap()
            .as_pin_ref()
            .has_focus()
    );

    for theme in ["light", "dark"] {
        window.invoke_theme(theme.into());
        window.set_replying(false);
        window.set_draft("".into());
        advance(650);
        frame(&window, &format!("{theme}-01-rest.png"));
        window.set_draft("A".into());
        advance(64);
        frame(&window, &format!("{theme}-02-rise-stretch.png"));
        advance(160);
        frame(&window, &format!("{theme}-03-arrival-squash.png"));
        advance(96);
        frame(&window, &format!("{theme}-04-wobble.png"));
        advance(280);
        frame(&window, &format!("{theme}-05-raised-connected.png"));
        let (_, neck_y, neck_w, neck_h) = rect(&window, "MessageComposer::left-neck");
        let (_, field_y, _, _) = rect(&window, "MessageComposer::field-surface");
        assert!(neck_w >= 45.0 && neck_h >= 5.0 && neck_y + neck_h >= field_y);
        if theme == "dark" {
            // Check the rendered meniscus, not just its Path item bounds.
            // A square viewbox with aspect-preserving fit made this six pixels wide.
            let pixels = window.window().take_snapshot().unwrap();
            let bytes = pixels.as_bytes();
            let row = (field_y as usize - 4) * pixels.width() as usize * 4;
            let base = &bytes[row..row + 3];
            let (left_x, _, left_w, _) = rect(&window, "MessageComposer::left-action-surface");
            let filled = ((left_x + 8.0) as usize..(left_x + left_w - 8.0) as usize)
                .filter(|&x| {
                    let at = row + x * 4;
                    (0..3)
                        .map(|c| bytes[at + c].abs_diff(base[c]) as usize)
                        .sum::<usize>()
                        > 18
                })
                .count();
            assert!(filled >= 20, "rendered meniscus narrowed to {filled}px");
        }
        window.set_replying(true);
        advance(600);
        frame(&window, &format!("{theme}-06-reply-chip.png"));
        let (chip_x, chip_y, chip_w, chip_h) = rect(&window, "MessageComposer::reply-chip");
        let (right_x, right_y, _, _) = rect(&window, "MessageComposer::right-action-surface");
        assert!(chip_w > 100.0 && chip_x + chip_w < right_x);
        let (_, left_y, _, left_h) = rect(&window, "MessageComposer::left-action-surface");
        assert!(chip_y >= 0.0 && chip_y + chip_h <= 270.0);
        assert!(left_y >= 0.0 && left_y + left_h <= 270.0 && right_y >= 0.0);
        window.set_replying(false);
        window.set_draft("".into());
        advance(150);
        frame(&window, &format!("{theme}-07-fall-merge.png"));
        advance(500);
        assert!(
            original_input
                .downcast::<i_slint_core::items::TextInput>()
                .unwrap()
                .as_pin_ref()
                .has_focus()
        );
    }

    window.set_draft("A".into());
    advance(64);
    let rise_gap = rect(&window, "MessageComposer::field-surface").1
        - rect(&window, "MessageComposer::left-action-surface").1;
    window.set_draft("".into());
    advance(128);
    let reverse_gap = rect(&window, "MessageComposer::field-surface").1
        - rect(&window, "MessageComposer::left-action-surface").1;
    assert!(rise_gap > reverse_gap);
    assert!(native_input(&window) == original_input);

    // Reversal uses the same native TextInput item; Slint would recreate it
    // only if this count changed while the conditional shells appeared.
    let inputs: Vec<_> =
        testing::ElementHandle::find_by_element_id(&window, "MessageComposer::text-input")
            .filter(|e| e.size().height > 0.0)
            .collect();
    assert_eq!(inputs.len(), 1);
    assert!(native_input(&window) == original_input);
    assert!(
        original_input
            .downcast::<i_slint_core::items::TextInput>()
            .unwrap()
            .as_pin_ref()
            .has_focus()
    );
}

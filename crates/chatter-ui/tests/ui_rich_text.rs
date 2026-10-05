//! Mixed inline markdown flows in ONE wrapped paragraph, and the text still
//! takes the bubble's gestures.
//!
//! Before: `**bold** *italic* ~~strike~~` was one Slint element per span in a
//! `VerticalLayout`, so each style sat on a line of its own. Now each inline
//! run is one `StyledText` built from our own spans
//! (`converters/markdown.rs` `parse_blocks`, `StyledText::from_paragraphs`).
//! Upstream `StyledText` accepted every pointer press, so a hold or a
//! right-click on the text never reached `bubble-touch` underneath it; the
//! vendored item now ignores presses that are not on a link
//! (vendor/i-slint-core/items/text.rs).
//!
//! Own test binary, one test: the integration testing backend is
//! once-per-process.

use std::cell::RefCell;
use std::rc::Rc;

use chatter_ui::{MainWindow, MessageItemData, RichTextBlock};
use i_slint_backend_testing as testing;
use i_slint_core::styled_text::{FormattedSpan, Style, StyledTextParagraph};
use slint::platform::{PointerEventButton, WindowEvent};
use slint::{ComponentHandle, LogicalPosition, ModelRc, VecModel};

/// Frames with real time between them: the hold timer and the list's
/// layout both run off the system clock under
/// `init_integration_test_with_system_time` (see `ui_render.rs` `pump`).
fn pump(w: &MainWindow, ms: u64) {
    for _ in 0..(ms / 20) {
        let size = w.window().size().to_logical(w.window().scale_factor());
        w.window().dispatch_event(WindowEvent::Resized { size });
        slint::Timer::single_shot(std::time::Duration::from_millis(1), || {
            slint::quit_event_loop().unwrap();
        });
        w.run().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// One inline paragraph: `(text, style)` runs concatenated, styles as ranges.
fn styled(runs: &[(&str, Option<Style>)]) -> RichTextBlock {
    let mut p = StyledTextParagraph { text: String::new(), formatting: vec![], links: vec![] };
    for (text, style) in runs {
        let start = p.text.len();
        p.text.push_str(text);
        if let Some(style) = style {
            p.formatting.push(FormattedSpan { range: start..p.text.len(), style: style.clone() });
        }
    }
    RichTextBlock { plain: p.text.clone().into(), text: slint::StyledText::from_paragraphs([p]), ..Default::default() }
}

fn row(i: usize, content: String, block: RichTextBlock) -> MessageItemData {
    MessageItemData {
        message_id: format!("m{i}").into(),
        sender_id: "dev".into(),
        sender_name: "Dev".into(),
        content: content.into(),
        rich_content: ModelRc::new(VecModel::from(vec![block])),
        timestamp_ns: format!("{i}").into(),
        show_sender: true,
        ..Default::default()
    }
}

#[derive(Clone, Copy, Debug)]
struct Rect {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

/// Every laid-out `StyledText`, top to bottom.
fn styled_texts(w: &MainWindow) -> Vec<Rect> {
    let mut v: Vec<Rect> = testing::ElementHandle::find_by_element_type_name(w, "StyledText")
        .map(|e| {
            let (p, s) = (e.absolute_position(), e.size());
            Rect { x: p.x, y: p.y, w: s.width, h: s.height }
        })
        .filter(|r| r.h > 0.0)
        .collect();
    v.sort_by(|a, b| a.y.total_cmp(&b.y));
    v
}

fn press(w: &MainWindow, r: Rect, button: PointerEventButton) {
    let position = LogicalPosition::new(r.x + r.w / 2.0, r.y + r.h / 2.0);
    w.window().dispatch_event(WindowEvent::PointerMoved { position });
    w.window().dispatch_event(WindowEvent::PointerPressed { position, button });
}

fn release(w: &MainWindow, r: Rect, button: PointerEventButton) {
    let position = LogicalPosition::new(r.x + r.w / 2.0, r.y + r.h / 2.0);
    w.window().dispatch_event(WindowEvent::PointerReleased { position, button });
}

#[test]
fn mixed_styles_flow_in_one_wrapped_paragraph_and_keep_the_bubble_gestures() {
    assert!(
        chatter_ui::SLINT_DEBUG_INFO_ENABLED,
        "element queries are inert without the ui-test-debug feature: ./scripts/test-ui.sh"
    );
    testing::init_integration_test_with_system_time();
    let w = MainWindow::new().expect("MainWindow::new");
    w.window().set_size(slint::LogicalSize::new(1100.0, 800.0));
    w.set_current_view("main".into());
    w.set_channel_name("dev".into());
    w.show().unwrap();
    pump(&w, 200);

    let words = "flowing ".repeat(60);
    w.set_messages(ModelRc::new(VecModel::from(vec![
        row(
            0,
            "bold italic strike".into(),
            styled(&[
                ("bold", Some(Style::Strong)),
                (" ", None),
                ("italic", Some(Style::Emphasis)),
                (" ", None),
                ("strike", Some(Style::Strikethrough)),
            ]),
        ),
        row(1, "x".into(), styled(&[("x", Some(Style::Strong))])),
        row(
            2,
            format!("bold {words}"),
            styled(&[("bold", Some(Style::Strong)), (" ", None), (&words, Some(Style::Emphasis))]),
        ),
    ])));
    w.set_is_at_bottom(true);
    pump(&w, 500);

    let texts = styled_texts(&w);
    assert_eq!(texts.len(), 3, "one StyledText per message body, got {texts:?}");
    let (mixed, one, long) = (texts[0], texts[1], texts[2]);
    println!("mixed {mixed:?}\none {one:?}\nlong {long:?}");

    // One line: as tall as a one-word line, and laid out side by side.
    assert!(one.h > 0.0 && one.w > 0.0);
    assert!(
        (mixed.h - one.h).abs() < 1.0,
        "\"bold italic strike\" is {}px tall, a single line is {}px: the styles did not share a line",
        mixed.h,
        one.h
    );
    assert!(mixed.w > 3.0 * one.w, "the three styled words are not side by side ({}px)", mixed.w);
    // Long mixed text wraps inside ONE element, within the bubble.
    assert!(long.h >= 3.0 * one.h, "the long mixed paragraph did not wrap ({}px tall)", long.h);
    assert!(
        long.w <= 0.7 * 1100.0,
        "the long mixed paragraph overflows the bubble ({}px wide)",
        long.w
    );

    // StyledText has no implicit accessible label (Text does); the row sets one.
    assert!(
        testing::ElementHandle::find_by_accessible_label(&w, "bold italic strike").next().is_some(),
        "the styled body is invisible to a screen reader"
    );

    let menu: Rc<RefCell<Vec<String>>> = Rc::default();
    let m = menu.clone();
    w.on_more_clicked(move |_, ts, _, _| m.borrow_mut().push(ts.to_string()));

    // Desktop: right-click on the styled text opens the menu.
    press(&w, mixed, PointerEventButton::Right);
    release(&w, mixed, PointerEventButton::Right);
    pump(&w, 60);
    assert_eq!(*menu.borrow(), ["0"], "right-click on the styled text did not reach bubble-touch");

    // Touch: press-and-hold on the styled text opens the menu.
    menu.borrow_mut().clear();
    w.set_is_mobile_platform(true);
    pump(&w, 200);
    let mixed = *styled_texts(&w).first().expect("message text still laid out on mobile");
    press(&w, mixed, PointerEventButton::Left);
    pump(&w, 800);
    release(&w, mixed, PointerEventButton::Left);
    pump(&w, 60);
    assert_eq!(*menu.borrow(), ["0"], "a hold on the styled text did not open the menu");
}

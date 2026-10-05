//! Long unbroken tokens wrap inside a bubble-width `Text { wrap: word-wrap }`.
//!
//! Message bodies used to get a zero-width space spliced in every 50 non-space
//! characters so Slint could break them. That is not needed: Slint 1.17 maps
//! `TextWrap::WordWrap` to parley `OverflowWrap::Anywhere`
//! (vendor/i-slint-core/textlayout/sharedparley.rs:292-295), which breaks an
//! overlong word mid-token. The Skia, software and testing renderers all
//! measure and break lines through that same `sharedparley` layout, so this
//! headless check exercises the code the device renderers run.
//!
//! The probe mirrors the bubble in `ui/chat/message_item.slint`: a word-wrap
//! `Text` with a `max-width`, sized by its parent `VerticalLayout`, and the
//! `StyledText` a body with markdown is drawn in (always `WordWrap`,
//! vendor/i-slint-core/items/text.rs `StyledTextItem::wrap`).
//!
//! Own test binary: the testing platform is set once per process.

use slint::ComponentHandle;

slint::slint! {
    export component WrapProbe inherits Window {
        in property <string> body;
        in property <styled-text> styled-body;
        out property <length> text-w: wrapped.width;
        out property <length> text-h: wrapped.height;
        out property <length> styled-w: styled.width;
        out property <length> styled-h: styled.height;
        out property <length> line-h: one-line.preferred-height;
        out property <length> unwrapped-w: unwrapped.preferred-width;
        width: 400px;
        height: 2000px;

        VerticalLayout {
            alignment: start;
            HorizontalLayout {
                alignment: start;
                VerticalLayout {
                    max-width: 200px;
                    wrapped := Text {
                        text: root.body;
                        font-size: 14px;
                        wrap: word-wrap;
                        max-width: 200px;
                    }
                }
            }
            HorizontalLayout {
                alignment: start;
                VerticalLayout {
                    max-width: 200px;
                    styled := StyledText {
                        text: root.styled-body;
                        default-font-size: 14px;
                        max-width: 200px;
                    }
                }
            }
        }
        one-line := Text { text: "x"; font-size: 14px; visible: false; }
        unwrapped := Text { text: root.body; font-size: 14px; wrap: no-wrap; visible: false; }
    }
}

fn assert_wraps(probe: &WrapProbe, body: &str, what: &str) {
    use i_slint_core::styled_text::{FormattedSpan, Style, StyledTextParagraph};
    probe.set_body(body.into());
    // The same body as a bold run then the token in italics, the way
    // converters/markdown.rs builds a body with markdown.
    let text = format!("bold {body}");
    let formatting = vec![
        FormattedSpan { range: 0..4, style: Style::Strong },
        FormattedSpan { range: 5..text.len(), style: Style::Emphasis },
    ];
    probe.set_styled_body(slint::StyledText::from_paragraphs([StyledTextParagraph {
        text,
        formatting,
        links: vec![],
    }]));
    let (sw, sh) = (probe.get_styled_w(), probe.get_styled_h());
    let (w, h) = (probe.get_text_w(), probe.get_text_h());
    let (line, unwrapped) = (probe.get_line_h(), probe.get_unwrapped_w());
    assert!(line > 0.0, "{what}: no font metrics (line height {line})");
    assert!(
        unwrapped > 400.0,
        "{what}: control failed, the token is only {unwrapped}px unwrapped, \
         so it would fit without breaking and the test proves nothing"
    );
    assert!(w <= 200.0, "{what}: text is {w}px wide, overflowing the 200px bubble");
    assert!(
        h >= 2.0 * line,
        "{what}: text is {h}px tall (line {line}px): the token did not wrap"
    );
    assert!(sw <= 200.0, "{what}: styled text is {sw}px wide, overflowing the 200px bubble");
    assert!(
        sh >= 2.0 * line,
        "{what}: styled text is {sh}px tall (line {line}px): the token did not wrap"
    );
}

#[test]
fn long_unbroken_tokens_wrap_without_zero_width_spaces() {
    i_slint_backend_testing::init_no_event_loop();
    let probe = WrapProbe::new().unwrap();
    probe.show().unwrap();

    assert_wraps(&probe, &"a".repeat(200), "200-char token");
    assert_wraps(
        &probe,
        "https://example.com/a/very/long/path/that/keeps/going/without/any/\
         spaces/at/all/so/only/overflow-wrap/can/break/it?query=0123456789abcdef",
        "long URL",
    );
    assert_wraps(
        &probe,
        &format!("see {} 🙂🙂🙂", "x".repeat(150)),
        "token with emoji",
    );
}

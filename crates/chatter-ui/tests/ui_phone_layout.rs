//! Phone-width geometry proofs for the RC39 device-pass findings (§138).
//!
//! Own test binary, one `#[test]`: the integration testing backend is
//! `set_platform`-once-per-process (see `ui_layout.rs`), and Slint's event
//! loop belongs to one thread.
//!
//! - Settings: the compact section tab bar must sit at the TOP edge, where the
//!   content's padding-top reserves its room. It had no `y`, so Slint centred
//!   it and the S25 showed it mid-screen over the settings content.
//! - Link-device page (new device): the two-line "Verification code" status
//!   must stack BELOW the "Copy link" button, not overlap it.
//! - Channel call history (server records): rows must stack top-aligned and
//!   the list must scroll when long. Each repeated word-wrap Text was measured
//!   at ~0 width (a glyph per line), so every row was ~1150px tall with the
//!   slack in its empty button row: records far apart with a gap between.

use chatter_ui::{MainWindow, ServerRecordRow, ServerRecordsGlobal};
use i_slint_backend_testing as testing;
use slint::ComponentHandle;

/// Galaxy S25 portrait, 1440x3120 at 3.5x: ~412x891 logical.
const PHONE_W: f32 = 412.0;
const PHONE_H: f32 = 891.0;

fn pump(w: &MainWindow) {
    w.window()
        .set_size(slint::LogicalSize::new(PHONE_W, PHONE_H));
    slint::Timer::single_shot(std::time::Duration::from_millis(80), || {
        slint::quit_event_loop().unwrap();
    });
    w.show().unwrap();
    slint::run_event_loop().unwrap();
}

fn rect(e: &testing::ElementHandle) -> (f32, f32, f32, f32) {
    let p = e.absolute_position();
    let s = e.size();
    (p.x, p.y, s.width, s.height)
}

fn one(w: &MainWindow, label: &str) -> testing::ElementHandle {
    let found: Vec<_> = testing::ElementHandle::find_by_accessible_label(w, label)
        .filter(|e| e.size().height > 0.0)
        .collect();
    assert!(!found.is_empty(), "no visible element labelled {label:?}");
    found.into_iter().next().unwrap()
}

#[test]
fn settings_tab_bar_and_link_device_status_land_where_designed_on_a_phone() {
    assert!(
        chatter_ui::SLINT_DEBUG_INFO_ENABLED,
        "element queries are inert without the ui-test-debug feature: \
         cargo test -p chatter-ui --features ui-test-debug --test ui_phone_layout"
    );
    testing::init_integration_test_with_system_time();
    let w = MainWindow::new().expect("MainWindow::new");
    w.set_is_mobile_platform(true);

    // ── Link-device page, status two lines long, long paste-link shown ──
    w.set_current_view("link-device".into());
    w.set_link_device_active(true);
    w.set_link_device_url(
        "chatter://link?uuid=0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0&pub_key=BdW1u0nIzfQ8x0yJ7m3Kq2Vt9pL4sR6aE1cF5gH8jK0mN2oP4qR6sT8uV0wX2yZ4&server=https%3A%2F%2Fchat.example.org%3A50051".into(),
    );
    let status = "Verification code: 482913\nCheck your other device shows the same code, then approve there.";
    w.set_link_device_status(status.into());
    pump(&w);

    let copy = one(&w, "Copy link");
    let status_el = one(&w, status);
    let (_, cy, _, ch) = rect(&copy);
    let (_, sy, _, sh) = rect(&status_el);
    println!("copy-link y={cy} h={ch}; status y={sy} h={sh}");
    assert!(
        sy >= cy + ch - 0.5,
        "verification status (y={sy}) overlaps the Copy link button (bottom={})",
        cy + ch
    );
    let cancel = one(&w, "Cancel");
    assert!(
        rect(&cancel).1 >= sy + sh - 0.5,
        "verification status (bottom={}) overlaps the Cancel/Restart row (y={})",
        sy + sh,
        rect(&cancel).1
    );

    // ── Settings, compact (phone) ──
    w.set_current_view("main".into());
    w.set_settings_open(true);
    pump(&w);

    let screen = testing::ElementHandle::find_by_element_type_name(&w, "SettingsScreen")
        .next()
        .expect("SettingsScreen not instantiated with settings-open");
    let (_, screen_y, _, screen_h) = rect(&screen);
    let tab = one(&w, "Linked devices");
    let (_, ty, _, th) = rect(&tab);
    println!("settings y={screen_y} h={screen_h}; 'Linked devices' tab y={ty} h={th}");
    assert!(
        screen_h > 400.0,
        "SettingsScreen has no real height ({screen_h})"
    );
    assert!(
        ty < screen_y + 80.0,
        "settings tab bar is not at the top edge: tab y={ty}, screen y={screen_y} h={screen_h}"
    );

    // ── Channel call history, two records, then many ──
    w.set_settings_open(false);
    let g = w.global::<ServerRecordsGlobal>();
    g.set_kind("calls".into());
    g.set_channel("c1".into());
    let rows = |n: usize| {
        let v: Vec<ServerRecordRow> = (0..n)
            .map(|i| ServerRecordRow {
                id: format!("r{i}").into(),
                title: format!("Voice call {i}").into(),
                body: "Started by alice, 3 participants, lasted 12 minutes and 40 seconds".into(),
                primary: "".into(),
                secondary: "".into(),
            })
            .collect();
        slint::ModelRc::new(slint::VecModel::from(v))
    };
    g.set_rows(rows(2));
    w.set_modal_stack(slint::ModelRc::new(slint::VecModel::from(vec![
        slint::SharedString::from("server-records"),
    ])));
    pump(&w);

    // Label queries did not find rows scrolled/laid out past the visible list
    // (seen before the fix), so the proofs read the list's own geometry: the
    // viewport content layout and row 0, via the ScrollView's descendants.
    let records = |w: &MainWindow| {
        let scroll = testing::ElementHandle::find_by_element_type_name(w, "ScrollView")
            .find(|e| e.size().height > 0.0)
            .expect("no ScrollView in the call history");
        let mut kids = Vec::new();
        scroll.visit_descendants(|d| {
            kids.push((d.type_name().unwrap_or_default().to_string(), rect(&d)));
            std::ops::ControlFlow::<()>::Continue(())
        });
        let flick = kids
            .iter()
            .find(|k| k.0 == "Flickable")
            .expect("Flickable")
            .1;
        let content = kids
            .iter()
            .find(|k| k.0 == "VerticalLayout")
            .expect("rows layout")
            .1;
        let row0 = kids
            .iter()
            .filter(|k| k.0 == "VerticalLayout")
            .nth(1)
            .expect("row 0")
            .1;
        (flick, content, row0)
    };
    let (flick, content, row0) = records(&w);
    println!("2 records: list {flick:?}, content {content:?}, row 0 {row0:?}");
    assert!(
        row0.3 < 120.0,
        "call record 0 is stretched to {}px: the rows spread over the list instead of stacking",
        row0.3
    );
    assert!(
        content.3 <= flick.3 + 0.5,
        "two records' content ({}px) is taller than the list ({}px)",
        content.3,
        flick.3
    );

    g.set_rows(rows(40));
    pump(&w);
    let (flick, content, _) = records(&w);
    println!("40 records: list {flick:?}, content {content:?}");
    assert!(
        flick.1 + flick.3 <= PHONE_H + 0.5,
        "records list runs off the screen"
    );
    assert!(
        content.3 > flick.3,
        "40 records do not overflow the list, so it cannot scroll"
    );
    let before = content.1;
    w.window()
        .dispatch_event(slint::platform::WindowEvent::PointerScrolled {
            position: slint::LogicalPosition::new(200.0, flick.1 + 200.0),
            delta_x: 0.0,
            delta_y: -300.0,
        });
    pump(&w);
    let (_, scrolled, _) = records(&w);
    let after = scrolled.1;
    println!("records content y before scroll={before}, after={after}");
    assert!(
        after < before - 50.0,
        "the records list did not scroll (y {before} -> {after})"
    );

    // ── Composer: the field is concentric with the glass shell ──
    // Owner, 2026-10-05: the field's rounding did not match the shell's. The
    // field's radius is the shell's minus the inset between them
    // (Spacing.radius-chrome-inner); that only reads as concentric when the
    // outer controls and field share the top/bottom inset. The leading
    // emoji toggle owns the left edge; the field follows it by one inset.
    let spacing = include_str!("../../../ui/theme/spacing.slint");
    assert!(
        spacing
            .contains("out property <length> radius-chrome-inner: radius-chrome - chrome-inset;")
    );
    assert!(spacing.contains("out property <length> chrome-inset: xs;"));
    assert!(spacing.contains("out property <length> xs: 4px;"));
    let inset = 4.0_f32;
    let composer_src = include_str!("../../../ui/chat/message_composer.slint");
    assert!(
        composer_src.contains("border-radius: Spacing.radius-chrome;"),
        "shell radius"
    );
    assert_eq!(
        composer_src
            .matches("border-radius: Spacing.radius-chrome-inner;")
            .count(),
        4,
        "field, emoji, attach and send are concentric with the shell"
    );
    w.set_modal_stack(slint::ModelRc::new(slint::VecModel::from(Vec::<
        slint::SharedString,
    >::new())));
    w.set_conversation_kind(chatter_ui::ConversationKind::ServerDm);
    w.set_channel_name("Bob".into());
    w.set_mobile_show_chat(true);
    pump(&w);
    let shell = testing::ElementHandle::find_by_element_type_name(&w, "MessageComposer")
        .find(|e| e.size().height > 0.0)
        .expect("no MessageComposer on the open chat");
    let (sx, sy, sw, sh) = rect(&shell);
    let (fx, fy, fw, fh) = rect(&one(&w, "Message input"));
    let (bx, _, bw, _) = rect(&one(&w, "Record voice message"));
    // The emoji & GIF toggle leads the row; the field sits one inset after it.
    let (ex, _, ew, _) = rect(&one(&w, "Emoji & GIF picker"));
    println!(
        "composer {sx},{sy} {sw}x{sh}; leading toggle x={ex} w={ew}; field {fx},{fy} {fw}x{fh}; trailing button x={bx} w={bw}"
    );
    for (side, got) in [
        ("left (leading toggle)", ex - sx),
        ("toggle-to-field gap", fx - (ex + ew)),
        ("top", fy - sy),
        ("bottom", sy + sh - (fy + fh)),
        ("right (trailing button)", sx + sw - (bx + bw)),
    ] {
        assert!(
            (got - inset).abs() < 0.5,
            "composer {side} inset is {got}px, not chrome-inset ({inset}px)"
        );
    }

    // Empty committed body + >six-line preedit with selection at start.
    chatter_ui::bin_ui::features::composer::wire_text_format(&w);
    w.set_composer_text("".into());
    pump_size(&w, PHONE_W, 460.0);
    let field = one(&w, "Message input");
    click_at(
        &w,
        field.absolute_position().x + 20.0,
        field.absolute_position().y + 20.0,
    );
    let preedit_only = "uncommitted native caret words ".repeat(30);
    composition_selection(&w, &preedit_only, Some(0..0));
    pump_size(&w, PHONE_W, 460.0);
    assert_native_caret_visible(&w);
    assert!(w.get_composer_text().is_empty());
    composition_selection(&w, "", None);

    // A long draft must leave history visible with a phone-sized IME viewport.
    // Drive the actual raw input: no source-string or duplicate layout probe.
    let draft = format!(
        "{}\nEND",
        "wrapped words preserve the entire draft ".repeat(28)
    );
    w.set_composer_text(draft.clone().into());
    pump_size(&w, PHONE_W, 460.0);
    let field = one(&w, "Message input");
    let shell = testing::ElementHandle::find_by_element_type_name(&w, "MessageComposer")
        .find(|e| e.size().height > 0.0)
        .unwrap();
    assert!(
        shell.size().height <= 184.5,
        "composer occupies too much of the IME-reduced pane: {:?}",
        shell.size()
    );
    assert!(field.size().height >= 48.0, "input touch floor disappeared");
    assert_composer_caps(&w, false, 0.0);
    let raw = testing::ElementHandle::find_by_element_id(&w, "MessageComposer::text-input")
        .find(|e| e.accessible_value().as_ref().map(|s| s.as_str()) == Some(draft.as_str()))
        .expect("actual composer TextInput");
    click_at(
        &w,
        field.absolute_position().x + 20.0,
        field.absolute_position().y + 20.0,
    );
    for _ in 0..100 {
        key(&w, slint::platform::Key::DownArrow.into());
    }
    key(&w, slint::platform::Key::End.into());
    pump_size(&w, PHONE_W, 460.0);
    assert_tail_visible(&raw, &one(&w, "Message input"));
    assert_native_caret_visible(&w);
    key(&w, "!".into());
    assert_eq!(w.get_composer_text().as_str(), format!("{draft}!"));
    key(&w, "\n".into());
    key(&w, "tail".into());
    pump_size(&w, PHONE_W, 460.0);
    assert_tail_visible(&raw, &one(&w, "Message input"));
    assert_native_caret_visible(&w);
    let end_y = raw.absolute_position().y;
    for _ in 0..100 {
        key(&w, slint::platform::Key::UpArrow.into());
    }
    key(&w, slint::platform::Key::Home.into());
    pump_size(&w, PHONE_W, 460.0);
    assert!(
        raw.absolute_position().y > end_y + 40.0,
        "caret movement did not scroll back"
    );
    key(&w, "START ".into());
    assert!(w.get_composer_text().as_str().starts_with("START "));
    for _ in 0..100 {
        key(&w, slint::platform::Key::DownArrow.into());
    }
    key(&w, slint::platform::Key::End.into());
    // Mobile long press gives the child input ownership before selection drag.
    let clip = testing::ElementHandle::find_by_element_id(&w, "MessageComposer::input-scroll")
        .find(|e| e.size().height > 0.0)
        .unwrap();
    let item = composer_native_item(&w);
    let input = item.downcast::<i_slint_core::items::TextInput>().unwrap();
    let input = input.as_pin_ref();
    let window = item.window_adapter().unwrap();
    let unchanged = w.get_composer_text();
    assert!(unchanged.ends_with("tail"));
    // Drag between shaped positions within the known final word, rather than
    // arbitrary points that may both be after a short or empty wrapped line.
    let start_rect =
        window
            .renderer()
            .text_input_cursor_rect_for_byte_offset(input, &item, unchanged.len() - 3);
    let end_rect =
        window
            .renderer()
            .text_input_cursor_rect_for_byte_offset(input, &item, unchanged.len() - 1);
    let origin = item.map_to_window(Default::default());
    let start = slint::LogicalPosition::new(
        origin.x + start_rect.origin.x + 0.2,
        origin.y + start_rect.origin.y + start_rect.size.height / 2.0,
    );
    let finish = slint::LogicalPosition::new(
        origin.x + end_rect.origin.x + 0.2,
        origin.y + end_rect.origin.y + end_rect.size.height / 2.0,
    );
    assert!(
        start.y >= clip.absolute_position().y
            && finish.y < clip.absolute_position().y + clip.size().height
    );
    let button = slint::platform::PointerEventButton::Left;
    w.window()
        .dispatch_event(slint::platform::WindowEvent::PointerPressed {
            position: start,
            button,
        });
    slint::Timer::single_shot(std::time::Duration::from_millis(600), || {
        slint::quit_event_loop().unwrap();
    });
    w.show().unwrap();
    slint::run_event_loop().unwrap();
    w.window()
        .dispatch_event(slint::platform::WindowEvent::PointerMoved { position: finish });
    w.window()
        .dispatch_event(slint::platform::WindowEvent::PointerReleased {
            position: finish,
            button,
        });
    pump_size(&w, PHONE_W, 460.0);
    let item = composer_native_item(&w);
    let input = item.downcast::<i_slint_core::items::TextInput>().unwrap();
    let input = input.as_pin_ref();
    assert_ne!(
        input.anchor_position_byte_offset(),
        input.cursor_position_byte_offset(),
        "long press drag did not select text"
    );
    assert_eq!(
        w.get_composer_text(),
        unchanged,
        "selection drag changed the draft"
    );
    input.set_selection_offsets(
        &item.window_adapter().unwrap(),
        &item,
        unchanged.len() as i32,
        unchanged.len() as i32,
    );
    pump_size(&w, PHONE_W, 460.0);
    assert_native_caret_visible(&w);
    // A reverse committed selection survives geometry refresh unchanged.
    let cursor = unchanged.len() as i32 - 2;
    input.set_selection_offsets(
        &item.window_adapter().unwrap(),
        &item,
        unchanged.len() as i32,
        cursor,
    );
    pump_size(&w, 360.0, 380.0);
    assert_eq!(input.anchor_position_byte_offset(), unchanged.len() as i32);
    assert_eq!(input.cursor_position_byte_offset(), cursor);
    assert_native_caret_visible(&w);
    input.set_selection_offsets(
        &item.window_adapter().unwrap(),
        &item,
        unchanged.len() as i32,
        unchanged.len() as i32,
    );
    // Rewrap and resize while preserving the same end caret.
    pump_size(&w, 360.0, 380.0);
    assert_tail_visible(&raw, &one(&w, "Message input"));
    assert_native_caret_visible(&w);
    let expected = w.get_composer_text();
    w.set_composer_emoji_picker_visible(true);
    pump_size(&w, PHONE_W, PHONE_H);
    assert_native_caret_visible(&w);
    assert_composer_caps(&w, true, 0.0);
    w.set_composer_emoji_picker_visible(false);
    pump_size(&w, PHONE_W, 460.0);
    assert_tail_visible(&raw, &one(&w, "Message input"));
    assert_native_caret_visible(&w);
    assert_eq!(
        w.get_composer_text(),
        expected,
        "resize/picker changed draft body"
    );
    // Direct picker property changes do not invoke the app's keyboard-toggle
    // callback. Focus the actual composer before sending its native IME events.
    let field = one(&w, "Message input");
    click_at(
        &w,
        field.absolute_position().x + 20.0,
        field.absolute_position().y + 20.0,
    );
    pump_size(&w, PHONE_W, 460.0);
    input.set_selection_offsets(
        &item.window_adapter().unwrap(),
        &item,
        expected.len() as i32,
        expected.len() as i32,
    );
    assert!(
        input.has_focus(),
        "composition fixture must focus the composer"
    );
    // Actual native composition: the active endpoint is independent of text.
    let composing = " composed words 漢字 e\u{301} 👩‍👩‍👧‍👦\n".repeat(18);
    composition(&w, &composing, false);
    assert_eq!(
        input.preedit_text().as_str(),
        composing,
        "native IME event missed the composer"
    );
    pump_size(&w, PHONE_W, 460.0);
    assert_tail_visible(&raw, &one(&w, "Message input"));
    assert_native_caret_visible(&w);
    assert_eq!(w.get_composer_text(), expected);
    composition_selection(&w, &composing, Some(0..0));
    pump_size(&w, PHONE_W, 460.0);
    assert_native_caret_visible(&w);
    assert_eq!(w.get_composer_text(), expected, "scroll committed preedit");
    composition_selection(
        &w,
        &composing,
        Some(composing.len() as i32..composing.len() as i32),
    );
    pump_size(&w, PHONE_W, 460.0);
    assert_native_caret_visible(&w);
    let end_y = raw.absolute_position().y;
    composition_selection(&w, &composing, Some(0..0));
    pump_size(&w, PHONE_W, 460.0);
    assert_native_caret_visible(&w);
    assert!(
        raw.absolute_position().y > end_y + 40.0,
        "same-preedit end to start did not scroll"
    );
    assert_eq!(w.get_composer_text(), expected);
    composition(&w, &composing, true);
    pump_size(&w, PHONE_W, 460.0);
    // Slint normalizes committed key-event text to NFC; preedit stays verbatim.
    let committed_composing = composing.replace("e\u{301}", "é");
    assert_eq!(
        w.get_composer_text().as_str(),
        format!("{expected}{committed_composing}")
    );

    // Full-word wrapping at a mid-word insertion, with committed suffix present.
    // The short terminal fragment alone fits where the complete word cannot.
    let body = format!(
        "{}alphaBETAlongword 漢字 e\u{301} 👩‍👩‍👧‍👦\nEND",
        "rows before wrap\n".repeat(12)
    );
    w.set_composer_text(body.clone().into());
    pump_size(&w, 360.0, 380.0);
    let item = composer_native_item(&w);
    let input = item.downcast::<i_slint_core::items::TextInput>().unwrap();
    let input = input.as_pin_ref();
    let insertion = body.find("BETA").unwrap() as i32;
    let window = item.window_adapter().unwrap();
    input.set_selection_offsets(&window, &item, insertion, insertion);
    let middle = " words fit until terminalfrag";
    composition_selection(&w, middle, Some(middle.len() as i32..middle.len() as i32));
    pump_size(&w, 360.0, 380.0);
    assert_native_caret_visible(&w);
    assert_eq!(w.get_composer_text().as_str(), body);
    let mut prefix_wrap_differs = false;
    for width in (280..=440).step_by(8) {
        pump_size(&w, width as f32, 460.0);
        assert_native_caret_visible(&w);
        let prefix = format!("{}{}", &body[..insertion as usize], middle);
        let probe = Box::pin(i_slint_core::items::TextInput::default());
        let probe = probe.as_ref();
        probe.text.set(prefix.clone().into());
        probe.width.set(input.width());
        probe.height.set(input.height());
        probe.font_family.set(input.font_family());
        probe.font_size.set(input.font_size());
        probe.font_weight.set(input.font_weight());
        probe.font_italic.set(input.font_italic());
        probe.letter_spacing.set(input.letter_spacing());
        probe.wrap.set(input.wrap());
        probe.single_line.set(false);
        probe.text_cursor_width.set(input.text_cursor_width());
        let prefix_rect =
            window
                .renderer()
                .text_input_cursor_rect_for_byte_offset(probe, &item, prefix.len());
        let full_rect = input.visual_cursor_rect(&window, &item);
        if full_rect.origin.y > prefix_rect.origin.y + full_rect.size.height * 0.5 {
            prefix_wrap_differs = true;
            break;
        }
    }
    assert!(
        prefix_wrap_differs,
        "mid-word fixture must demonstrate full versus truncated-prefix wrapping"
    );
    for preedit in [
        "漢字仮名交じり文".repeat(30),
        "e\u{301} 👩‍👩‍👧‍👦 ".repeat(30),
        "wrapped\n\n".repeat(12),
    ] {
        for endpoint in [0, preedit.len() as i32] {
            composition_selection(&w, &preedit, Some(endpoint..endpoint));
            pump_size(&w, PHONE_W, 460.0);
            assert_native_caret_visible(&w);
            assert_eq!(w.get_composer_text().as_str(), body);
            assert_eq!(input.cursor_position_byte_offset(), insertion);
            assert_eq!(input.anchor_position_byte_offset(), insertion);
            assert_eq!(input.preedit_text().as_str(), preedit);
        }
    }
    composition_selection(&w, "", None);
    input.set_selection_offsets(&window, &item, body.len() as i32, body.len() as i32);
    w.set_composer_is_replying(true);
    w.set_composer_reply_preview("Accessory cap proof".into());
    pump_size(&w, PHONE_W, 460.0);
    assert_composer_caps(&w, false, 48.0);
    assert_native_caret_visible(&w);
    w.set_composer_emoji_picker_visible(true);
    pump_size(&w, PHONE_W, PHONE_H);
    assert_composer_caps(&w, true, 48.0);
    assert_native_caret_visible(&w);
    assert_eq!(w.get_composer_text().as_str(), body);
    w.set_composer_emoji_picker_visible(false);
    w.set_composer_is_replying(false);

    // Desktop pointer events must still reach raw TextInput through the
    // noninteractive viewport; arrows edit where the caret actually moved.
    w.set_is_mobile_platform(false);
    w.set_composer_text("".into());
    pump_size(&w, 1000.0, 800.0);
    // Focus a different native input first, proving pointer focus restoration.
    w.set_composer_emoji_picker_visible(true);
    pump_size(&w, 1000.0, 800.0);
    let search = testing::ElementHandle::find_by_element_id(&w, "ChatLayout::drawer-search")
        .find(|e| e.size().height > 0.0)
        .expect("drawer search input");
    click_at(
        &w,
        search.absolute_position().x + 10.0,
        search.absolute_position().y + 10.0,
    );
    assert!(
        !composer_native_item(&w)
            .downcast::<i_slint_core::items::TextInput>()
            .unwrap()
            .as_pin_ref()
            .has_focus()
    );
    let field = one(&w, "Message input");
    click_at(
        &w,
        field.absolute_position().x + 20.0,
        field.absolute_position().y + 20.0,
    );
    assert!(
        composer_native_item(&w)
            .downcast::<i_slint_core::items::TextInput>()
            .unwrap()
            .as_pin_ref()
            .has_focus()
    );
    key(&w, "abc".into());
    key(&w, slint::platform::Key::LeftArrow.into());
    key(&w, "X".into());
    assert_eq!(
        w.get_composer_text().as_str(),
        "abXc",
        "desktop pointer/caret regression"
    );
}

fn pump_size(w: &MainWindow, width: f32, height: f32) {
    w.window().set_size(slint::LogicalSize::new(width, height));
    slint::Timer::single_shot(std::time::Duration::from_millis(200), || {
        slint::quit_event_loop().unwrap();
    });
    w.show().unwrap();
    slint::run_event_loop().unwrap();
}

fn key(w: &MainWindow, text: slint::SharedString) {
    use slint::platform::WindowEvent;
    w.window()
        .dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
    w.window().dispatch_event(WindowEvent::KeyReleased { text });
}

fn click_at(w: &MainWindow, x: f32, y: f32) {
    use slint::platform::{PointerEventButton, WindowEvent};
    let position = slint::LogicalPosition::new(x, y);
    let button = PointerEventButton::Left;
    w.window()
        .dispatch_event(WindowEvent::PointerPressed { position, button });
    w.window()
        .dispatch_event(WindowEvent::PointerReleased { position, button });
}

fn assert_tail_visible(raw: &testing::ElementHandle, field: &testing::ElementHandle) {
    let bottom = raw.absolute_position().y + raw.size().height;
    let field_bottom = field.absolute_position().y + field.size().height;
    assert!(
        bottom <= field_bottom + 1.0 && bottom > field_bottom - 32.0,
        "end of wrapped input is outside its viewport: input bottom={bottom}, field bottom={field_bottom}"
    );
}

fn composition(w: &MainWindow, text: &str, commit: bool) {
    use i_slint_core::input::{InternalKeyEvent, KeyEventType};
    let mut event = InternalKeyEvent::default();
    if commit {
        event.event_type = KeyEventType::CommitComposition;
        event.key_event.text = text.into();
    } else {
        event.event_type = KeyEventType::UpdateComposition;
        event.preedit_text = text.into();
    }
    i_slint_core::window::WindowInner::from_pub(w.window()).process_key_input(event);
}

fn composition_selection(w: &MainWindow, text: &str, selection: Option<std::ops::Range<i32>>) {
    use i_slint_core::input::{InternalKeyEvent, KeyEventType};
    let mut event = InternalKeyEvent::default();
    event.event_type = KeyEventType::UpdateComposition;
    event.preedit_text = text.into();
    event.preedit_selection = selection;
    i_slint_core::window::WindowInner::from_pub(w.window()).process_key_input(event);
}

#[track_caller]
fn composer_native_item(w: &MainWindow) -> i_slint_core::item_tree::ItemRc {
    use std::ops::ControlFlow;
    let field = one(w, "Message input");
    let (fx, fy, fw, fh) = rect(&field);
    // Accessibility omits an empty value; select the actual clipped input by
    // its composer bounds even while its committed draft is empty.
    let raw = testing::ElementHandle::find_by_element_id(w, "MessageComposer::text-input")
        .find(|e| {
            let (x, y, width, height) = rect(e);
            width > 0.0
                && height > 0.0
                && x >= fx - 0.5
                && x + width <= fx + fw + 0.5
                && y < fy + fh
                && y + height > fy
        })
        .expect("actual raw composer within message input");
    let raw_position = raw.absolute_position();
    let root = i_slint_core::window::WindowInner::from_pub(w.window())
        .window_item_rc()
        .unwrap();
    root.visit_descendants(|item| {
        if let Some(input) = item.downcast::<i_slint_core::items::TextInput>() {
            let native_position = item.map_to_window(Default::default());
            // ElementHandle includes the item's positioned origin;
            // map_to_window(default) excludes its own x/y offset.
            if input.as_pin_ref().text().as_str() == w.get_composer_text().as_str()
                && item.geometry().size.height > 0.0
                && (native_position.x - raw_position.x).abs() < 0.1
                && raw_position.y >= native_position.y - 0.1
                && raw_position.y < native_position.y + item.geometry().size.height
            {
                return ControlFlow::Break(item.clone());
            }
        }
        ControlFlow::Continue(())
    })
    .expect("native composer input")
}

fn assert_native_caret_visible(w: &MainWindow) {
    let item = composer_native_item(w);
    let input = item.downcast::<i_slint_core::items::TextInput>().unwrap();
    let input = input.as_pin_ref();
    let caret = input.visual_cursor_rect(&item.window_adapter().unwrap(), &item);
    assert!((input.visual_cursor_y().get() - caret.origin.y).abs() < 0.01);
    assert!((input.visual_cursor_height().get() - caret.size.height).abs() < 0.01);
    // Independently query the renderer at its actual drawn visual endpoint.
    // App input is plain text, so no password conversion is needed here.
    if let Some(drawn_offset) = input.visual_representation(None).cursor_position {
        let drawn = item
            .window_adapter()
            .unwrap()
            .renderer()
            .text_input_cursor_rect_for_byte_offset(input, &item, drawn_offset);
        assert_eq!(
            caret, drawn,
            "scroll geometry differs from native rendered caret"
        );
    }
    let position = item.map_to_window(caret.origin);
    let clip = testing::ElementHandle::find_by_element_id(w, "MessageComposer::input-scroll")
        .find(|e| e.size().height > 0.0)
        .expect("actual Flickable clip");
    let p = clip.absolute_position();
    let size = clip.size();
    assert!(caret.size.height > 0.0);
    assert!(
        position.x >= p.x - 0.5
            && position.x + caret.size.width <= p.x + size.width + 0.5
            && position.y >= p.y - 0.5
            && position.y + caret.size.height <= p.y + size.height + 0.5,
        "native caret {position:?} {:?} outside exact clip {p:?} {size:?}",
        caret.size
    );
}

fn assert_composer_caps(w: &MainWindow, drawer: bool, accessories: f32) {
    let pane = testing::ElementHandle::find_by_element_type_name(w, "ChatLayout")
        .find(|e| e.size().height > 0.0)
        .unwrap();
    let shell = testing::ElementHandle::find_by_element_type_name(w, "MessageComposer")
        .find(|e| e.size().height > 0.0)
        .unwrap();
    let item = composer_native_item(w);
    let input = item.downcast::<i_slint_core::items::TextInput>().unwrap();
    let input = input.as_pin_ref();
    let probe = Box::pin(i_slint_core::items::ComplexText::default());
    let probe = probe.as_ref();
    probe.text.set("Mg".into());
    probe.font_family.set(input.font_family());
    probe.font_size.set(input.font_size());
    probe.font_weight.set(input.font_weight());
    let line_height = item
        .window_adapter()
        .unwrap()
        .renderer()
        .text_size(probe, &item, None, i_slint_core::items::TextWrap::NoWrap)
        .height
        .ceil();
    let pane_height = pane.size().height;
    let available = pane_height
        - if drawer {
            340.0_f32.min(pane_height * 0.48)
        } else {
            0.0
        };
    // The accepted 48px floor is explicit for unusually short panes/accessories.
    let cap = 48.0_f32.max((line_height * 6.0 + 16.0).min(available * 0.4 - accessories - 8.0));
    assert!(
        shell.size().height <= cap + accessories + 8.5,
        "composer {:?} exceeds six-line/actual-pane cap {cap}, pane={available}, accessories={accessories}",
        shell.size()
    );
}

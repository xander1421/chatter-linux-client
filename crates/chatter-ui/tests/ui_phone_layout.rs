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
    w.run().unwrap();
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

    // A long draft must leave history visible with a phone-sized IME viewport.
    // Drive the actual raw input: no source-string or duplicate layout probe.
    chatter_ui::bin_ui::features::composer::wire_text_format(&w);
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
    let raw = testing::ElementHandle::find_by_element_id(&w, "text-input")
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
    key(&w, "!".into());
    assert_eq!(w.get_composer_text().as_str(), format!("{draft}!"));
    key(&w, "\n".into());
    key(&w, "tail".into());
    pump_size(&w, PHONE_W, 460.0);
    assert_tail_visible(&raw, &one(&w, "Message input"));
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
    // Rewrap and resize while preserving the same end caret.
    pump_size(&w, 360.0, 380.0);
    assert_tail_visible(&raw, &one(&w, "Message input"));
    let expected = w.get_composer_text();
    w.set_composer_emoji_picker_visible(true);
    pump_size(&w, PHONE_W, PHONE_H);
    w.set_composer_emoji_picker_visible(false);
    pump_size(&w, PHONE_W, 460.0);
    assert_tail_visible(&raw, &one(&w, "Message input"));
    assert_eq!(
        w.get_composer_text(),
        expected,
        "resize/picker changed draft body"
    );
    // Uncommitted wrapped text is visible without being written into the body.
    let composing = " composed words".repeat(18);
    composition(&w, &composing, false);
    pump_size(&w, PHONE_W, 460.0);
    assert_eq!(
        w.get_composer_text(),
        expected,
        "preedit was committed by scrolling"
    );
    assert_tail_visible(&raw, &one(&w, "Message input"));
    composition(&w, &composing, true);
    pump_size(&w, PHONE_W, 460.0);
    assert_eq!(
        w.get_composer_text().as_str(),
        format!("{expected}{composing}")
    );

    // Desktop pointer events must still reach raw TextInput through the
    // noninteractive viewport; arrows edit where the caret actually moved.
    w.set_is_mobile_platform(false);
    w.set_composer_text("".into());
    pump_size(&w, 1000.0, 800.0);
    let field = one(&w, "Message input");
    click_at(
        &w,
        field.absolute_position().x + 20.0,
        field.absolute_position().y + 20.0,
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
    w.run().unwrap();
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

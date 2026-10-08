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
use slint::{ComponentHandle, Model};

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
    assert!(spacing
        .contains("out property <length> radius-chrome-inner: radius-chrome - chrome-inset;"));
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
        .find(|e| e.type_name().is_some_and(|t| t == "MessageComposer") && e.size().height > 0.0)
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
    // The first real keystroke raises actions without replacing/focusing a
    // different native input; clearing restores the original compact row.
    let original_input = composer_native_item(&w);
    key(&w, "a".into());
    // No event-loop sleep: the raised field must already fit its shell on the
    // first layout, rather than waiting 150ms for an animated parent to catch up.
    i_slint_core::properties::ChangeTracker::run_change_handlers();
    slint::platform::update_timers_and_animations();
    assert_action_geometry(&w, true);
    assert!(composer_native_item(&w) == original_input);
    assert!(original_input
        .downcast::<i_slint_core::items::TextInput>()
        .unwrap()
        .as_pin_ref()
        .has_focus());
    pump_size(&w, PHONE_W, 460.0);
    assert_eq!(w.get_composer_text().as_str(), "a");
    assert!(composer_native_item(&w) == original_input);
    assert_action_geometry(&w, true);
    assert_composer_caps(&w, false, 0.0);
    key(&w, slint::platform::Key::Backspace.into());
    i_slint_core::properties::ChangeTracker::run_change_handlers();
    slint::platform::update_timers_and_animations();
    assert_action_geometry(&w, false);
    pump_size(&w, PHONE_W, 460.0);
    assert!(w.get_composer_text().is_empty());
    assert!(composer_native_item(&w) == original_input);
    assert_action_geometry(&w, false);

    // A send in flight is a pending row in the list, not a composer row: the
    // composer's geometry must not change with the status property at all.
    for (draft, raised) in [
        ("".to_owned(), false),
        ("status budget words ".repeat(100), true),
    ] {
        w.set_composer_text(draft.into());
        pump_size(&w, PHONE_W, 460.0);
        let field_without_status = one(&w, "Message input").size().height;
        let shell_without_status = composer_shell_height(&w);
        w.set_composer_send_status("Sending geometry fixture".into());
        pump_size(&w, PHONE_W, 460.0);
        assert_action_geometry(&w, raised);
        assert!((one(&w, "Message input").size().height - field_without_status).abs() < 0.5);
        assert!((composer_shell_height(&w) - shell_without_status).abs() < 0.5);
        w.set_composer_send_status("".into());
    }
    w.set_composer_text("".into());
    pump_size(&w, PHONE_W, 460.0);

    // Exercise the actual conditional width branches: a mesh room has Attach
    // but no mic, a mesh DM has neither, and an empty edit has no trailing action.
    let mesh = w.global::<chatter_ui::MeshGlobal>();
    for kind in ["room", "dm"] {
        mesh.set_active_id("geometry-mesh".into());
        mesh.set_active_kind(kind.into());
        pump_size(&w, PHONE_W, 460.0);
        assert_action_geometry(&w, false);
        w.set_composer_text("mesh draft".into());
        pump_size(&w, PHONE_W, 460.0);
        assert_action_geometry(&w, true);
        assert_eq!(w.get_composer_text().as_str(), "mesh draft");
        assert!(composer_native_item(&w) == original_input);
        w.set_composer_text("".into());
    }
    mesh.set_active_id("".into());
    mesh.set_active_kind("".into());
    w.set_composer_is_editing(true);
    pump_size(&w, PHONE_W, 460.0);
    assert_action_geometry(&w, false);
    w.set_composer_is_editing(false);
    pump_size(&w, PHONE_W, 460.0);
    assert_action_geometry(&w, false);

    // The real Slint toggle callback opens the drawer and its keyboard action
    // closes it/refocuses the same input; no network or emoji loader is needed.
    click_element(&w, "Emoji & GIF picker");
    pump_size(&w, PHONE_W, PHONE_H);
    assert!(w.get_composer_emoji_picker_visible());
    assert_action_geometry(&w, false);
    click_element(&w, "Show keyboard");
    pump_size(&w, PHONE_W, 460.0);
    assert!(!w.get_composer_emoji_picker_visible() && !w.get_composer_gif_picker_visible());
    assert!(composer_native_item(&w) == original_input);
    assert!(original_input
        .downcast::<i_slint_core::items::TextInput>()
        .unwrap()
        .as_pin_ref()
        .has_focus());
    assert!(w.get_composer_text().is_empty());
    // This geometry fixture has no native IME arrival. End its outstanding
    // reservation before shrinking into the next independent raised-row case;
    // otherwise the 340px drawer reserve clips the next tap in a 460px window.
    // Arrival/interruption/refusal timing is exercised by the handover helper.
    w.invoke_cancel_composer_keyboard_handover();
    i_slint_core::properties::ChangeTracker::run_change_handlers();

    w.set_composer_text("toggle draft".into());
    pump_size(&w, PHONE_W, 460.0);
    let native = original_input
        .downcast::<i_slint_core::items::TextInput>()
        .unwrap();
    let native = native.as_pin_ref();
    native.set_selection_offsets(
        &original_input.window_adapter().unwrap(),
        &original_input,
        6,
        6,
    );
    click_element(&w, "Emoji & GIF picker");
    pump_size(&w, PHONE_W, PHONE_H);
    assert!(w.get_composer_emoji_picker_visible());
    assert_action_geometry(&w, true);
    click_element(&w, "Show keyboard");
    pump_size(&w, PHONE_W, 460.0);
    assert!(!w.get_composer_emoji_picker_visible());
    assert_action_geometry(&w, true);
    assert_eq!(w.get_composer_text().as_str(), "toggle draft");
    assert!(composer_native_item(&w) == original_input);
    assert_eq!(native.cursor_position_byte_offset(), 6);
    assert_eq!(native.anchor_position_byte_offset(), 6);
    assert!(native.has_focus());
    w.invoke_cancel_composer_keyboard_handover();
    i_slint_core::properties::ChangeTracker::run_change_handlers();
    w.set_composer_text("".into());
    pump_size(&w, PHONE_W, 460.0);

    // Read actual items at every requested width: typing, attachment-only,
    // captions, overflow scrolling and pointer removal all keep Send lower.
    let removed = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let removed_callback = removed.clone();
    let weak = w.as_weak();
    w.on_remove_attachment(move |id| {
        removed_callback.borrow_mut().push(id.clone());
        let w = weak.unwrap();
        let remaining = w
            .get_pending_attachments()
            .iter()
            .filter(|attachment| attachment.id != id)
            .collect::<Vec<_>>();
        w.set_pending_attachments(slint::ModelRc::new(slint::VecModel::from(remaining)));
    });
    for (mobile, width, height) in [
        (true, 320.0, PHONE_H),
        (true, PHONE_W, PHONE_H),
        (true, 768.0, 1024.0),
        (false, 1280.0, 900.0),
    ] {
        w.set_is_mobile_platform(mobile);
        w.set_pending_attachments(slint::ModelRc::default());
        for draft in ["typing".to_owned(), "readable multiline words ".repeat(40)] {
            w.set_composer_text(draft.into());
            pump_size(&w, width, height);
            assert_action_geometry(&w, true);
            assert!(composer_native_item(&w) == original_input);
        }
        w.set_pending_attachments(slint::ModelRc::new(slint::VecModel::from(vec![
            chatter_ui::PendingAttachmentData {
                id: "geometry-only".into(),
                filename: "unsent.txt".into(),
                category: "document".into(),
                is_ready: true,
                ..Default::default()
            },
        ])));
        for caption in ["", "caption"] {
            w.set_composer_text(caption.into());
            pump_size(&w, width, height);
            assert_action_geometry(&w, true);
            assert_attachment_strip_geometry(&w);
            assert!(composer_native_item(&w) == original_input);
        }
        w.set_composer_text("".into());
        w.set_pending_attachments(slint::ModelRc::new(slint::VecModel::from(
            (0..10)
                .map(|i| chatter_ui::PendingAttachmentData {
                    id: format!("overflow-{i}").into(),
                    filename: format!("long-preview-{i}.txt").into(),
                    category: "document".into(),
                    is_ready: i != 1,
                    ..Default::default()
                })
                .collect::<Vec<_>>(),
        )));
        pump_size(&w, width, height);
        assert_attachment_strip_geometry(&w);
        let first_remove = one(&w, "Remove attachment");
        let before = rect(&first_remove).0;
        scroll_attachment_strip(&w, -200.0);
        pump_size(&w, width, height);
        assert!(
            rect(&first_remove).0 < before - 50.0,
            "attachment overflow did not scroll at {width}"
        );
        assert_action_geometry(&w, true);
        assert_attachment_strip_geometry(&w);
        let strip =
            testing::ElementHandle::find_by_element_id(&w, "MessageComposer::attachment-strip")
                .find(|e| e.size().width > 0.0 && e.size().height > 0.0)
                .unwrap();
        let (cx, _, cw, _) = rect(&strip);
        let remove = testing::ElementHandle::find_by_accessible_label(&w, "Remove attachment")
            .find(|e| {
                let (x, _, width, height) = rect(e);
                height > 0.0 && x >= cx && x + width <= cx + cw
            })
            .expect("fully visible remove target inside actual preview clip");
        let (x, y, rw, rh) = rect(&remove);
        let count = removed.borrow().len();
        click_at(&w, x + rw / 2.0, y + rh / 2.0);
        pump_size(&w, width, height);
        assert_eq!(
            removed.borrow().len(),
            count + 1,
            "preview remove pointer missed at {width}"
        );
        assert_eq!(w.get_pending_attachments().row_count(), 9);
        assert_action_geometry(&w, true);
        assert_attachment_strip_geometry(&w);
        assert!(composer_native_item(&w) == original_input);
    }
    w.set_is_mobile_platform(true);
    w.set_composer_text("".into());
    w.set_pending_attachments(slint::ModelRc::new(slint::VecModel::from(Vec::<
        chatter_ui::PendingAttachmentData,
    >::new())));
    pump_size(&w, PHONE_W, 460.0);
    assert_action_geometry(&w, false);
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
    assert!(composer_native_item(&w) == original_input);
    assert_action_geometry(&w, true);
    assert_composer_caps(&w, false, 0.0);
    composition_selection(&w, "", None);
    pump_size(&w, PHONE_W, 460.0);
    assert_action_geometry(&w, false);

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
        .find(|e| e.type_name().is_some_and(|t| t == "MessageComposer") && e.size().height > 0.0)
        .unwrap();
    assert!(
        shell.size().height <= 184.5,
        "composer occupies too much of the IME-reduced pane: {:?}",
        shell.size()
    );
    assert!(field.size().height >= 48.0, "input touch floor disappeared");
    assert_composer_caps(&w, false, 0.0);
    assert_action_geometry(&w, true);
    assert!(composer_native_item(&w) == original_input);
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
    assert_action_geometry(&w, true);
    assert_composer_caps(&w, false, 44.0);
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
    assert_action_geometry(&w, true);
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
    // Keep Unicode on the next line: fallback glyphs change the caret height
    // and origin, obscuring the Latin word's otherwise one-line wrap difference.
    let body = format!(
        "{}alphaBETAlongword\n漢字 e\u{301} 👩‍👩‍👧‍👦\nEND",
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
    assert!(input.has_focus());
    assert_eq!(input.preedit_text().as_str(), middle);
    assert_eq!(input.cursor_position_byte_offset(), insertion);
    assert_eq!(input.anchor_position_byte_offset(), insertion);
    let visual = input.visual_representation(None);
    assert_eq!(
        visual.cursor_position,
        Some(insertion as usize + middle.len())
    );
    assert_eq!(
        visual.text.as_str(),
        format!(
            "{}{}{}",
            &body[..insertion as usize],
            middle,
            &body[insertion as usize..]
        )
    );
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
            // Equal caret metrics and a leftward move prove an actual line
            // wrap, rather than an origin shift from a taller fallback font.
            assert!((full_rect.size.height - prefix_rect.size.height).abs() < 0.01);
            assert!(full_rect.origin.x < prefix_rect.origin.x);
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
    assert_action_geometry(&w, true);
    assert_native_caret_visible(&w);
    assert_eq!(w.get_composer_text().as_str(), body);
    w.set_composer_emoji_picker_visible(false);
    w.set_composer_gif_picker_visible(true);
    pump_size(&w, PHONE_W, PHONE_H);
    assert_action_geometry(&w, true);
    assert_composer_caps(&w, true, 48.0);
    assert_native_caret_visible(&w);
    assert_eq!(w.get_composer_text().as_str(), body);
    w.set_composer_gif_picker_visible(false);
    w.set_composer_is_replying(false);
    // Editing reserves its accessory and removes Attach without narrowing the
    // raised field. Native selection, text and identity survive this resize.
    w.set_composer_is_editing(true);
    pump_size(&w, PHONE_W, 460.0);
    assert_composer_caps(&w, false, 48.0);
    assert_action_geometry(&w, true);
    assert_native_caret_visible(&w);
    assert!(composer_native_item(&w) == item);
    assert_eq!(w.get_composer_text().as_str(), body);
    assert!(
        !testing::ElementHandle::find_by_accessible_label(&w, "Attach file")
            .any(|e| e.size().height > 0.0)
    );
    w.set_composer_is_editing(false);

    // The recording panel still owns the shell and its controls; ordinary
    // input is disabled without replacing it or consuming the draft.
    let recording = w.global::<chatter_ui::VoiceRecording>();
    recording.set_active(true);
    pump_size(&w, PHONE_W, 460.0);
    let panel = testing::ElementHandle::find_by_element_id(&w, "MessageComposer::recording-panel")
        .find(|e| e.size().height > 0.0)
        .unwrap();
    let shell = testing::ElementHandle::find_by_element_type_name(&w, "MessageComposer")
        .find(|e| e.type_name().is_some_and(|t| t == "MessageComposer") && e.size().height > 0.0)
        .unwrap();
    assert!((panel.size().height - shell.size().height).abs() < 0.5);
    assert!(one(&w, "Cancel voice message").size().height >= 48.0);
    assert!(one(&w, "Send voice message").size().height >= 48.0);
    assert!(!input.enabled());
    assert_eq!(w.get_composer_text().as_str(), body);
    // Observe the real button -> global cancel callback without starting a
    // microphone or constructing the authenticated service bridge.
    let cancelled = std::rc::Rc::new(std::cell::Cell::new(0));
    let cancel_count = cancelled.clone();
    let weak = w.as_weak();
    recording.on_cancel(move || {
        cancel_count.set(cancel_count.get() + 1);
        if let Some(w) = weak.upgrade() {
            w.global::<chatter_ui::VoiceRecording>().set_active(false);
            w.global::<chatter_ui::VoiceRecording>().set_busy(false);
        }
    });
    click_element(&w, "Cancel voice message");
    assert_eq!(cancelled.get(), 1);
    assert!(!recording.get_active());
    pump_size(&w, PHONE_W, 460.0);
    assert_action_geometry(&w, true);
    assert!(composer_native_item(&w) == item);

    // Desktop pointer events must still reach raw TextInput through the
    // noninteractive viewport; arrows edit where the caret actually moved.
    w.set_is_mobile_platform(false);
    w.set_composer_text("".into());
    pump_size(&w, 1000.0, 800.0);
    assert_action_geometry(&w, false);
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
    assert!(!composer_native_item(&w)
        .downcast::<i_slint_core::items::TextInput>()
        .unwrap()
        .as_pin_ref()
        .has_focus());
    let field = one(&w, "Message input");
    click_at(
        &w,
        field.absolute_position().x + 20.0,
        field.absolute_position().y + 20.0,
    );
    assert!(composer_native_item(&w)
        .downcast::<i_slint_core::items::TextInput>()
        .unwrap()
        .as_pin_ref()
        .has_focus());
    let desktop_input = composer_native_item(&w);
    key(&w, "a".into());
    assert_action_geometry(&w, true);
    key(&w, "b".into());
    key(&w, "c".into());
    assert_eq!(w.get_composer_text().as_str(), "abc");
    pump_size(&w, 1000.0, 800.0);
    assert!(composer_native_item(&w) == desktop_input);
    assert_action_geometry(&w, true);
    key(&w, slint::platform::Key::LeftArrow.into());
    key(&w, "X".into());
    assert_eq!(
        w.get_composer_text().as_str(),
        "abXc",
        "desktop pointer/caret regression"
    );
    key(&w, slint::platform::Key::End.into());
    for _ in 0..4 {
        key(&w, slint::platform::Key::Backspace.into());
    }
    assert_eq!(w.get_composer_text().as_str(), "");
    assert_action_geometry(&w, false);
    assert!(composer_native_item(&w) == desktop_input);
    key(&w, "z".into());
    assert_eq!(w.get_composer_text().as_str(), "z");
    assert_action_geometry(&w, true);
    attachment_and_keyboard_handover_boundaries(&w);
}

/// Exercise production picker handover and actual window IME geometry. The
/// native dialog is represented by window deactivation/activation; attachment
/// staging is represented by its real pending tray model, with no data erased.
fn attachment_and_keyboard_handover_boundaries(w: &MainWindow) {
    use slint::platform::WindowEvent;
    w.set_is_mobile_platform(true);
    w.set_mobile_show_chat(true);
    w.set_composer_text("ab😀cd".into());
    pump_size(w, PHONE_W, PHONE_H);
    let item = composer_native_item(w);
    let input = item.downcast::<i_slint_core::items::TextInput>().unwrap();
    let input = input.as_pin_ref();
    let adapter = item.window_adapter().unwrap();
    let callbacks = std::rc::Rc::new(std::cell::Cell::new(0));
    let observed = callbacks.clone();
    let weak = w.as_weak();
    // Same production preparation invoked at the actual host callback boundary.
    w.on_attachment_clicked(move || {
        observed.set(observed.get() + 1);
        weak.upgrade().unwrap().invoke_prepare_attachment_picker();
        i_slint_core::properties::ChangeTracker::run_change_handlers();
    });
    for (action, selected) in [
        ("Attach file", false),
        ("Attach file", true),
        ("Add attachment", false),
    ] {
        let field = one(w, "Message input");
        click_at(
            w,
            field.absolute_position().x + 20.0,
            field.absolute_position().y + 20.0,
        );
        input.set_selection_offsets(&adapter, &item, 6, 2); // reversed UTF-8 selection
        if action == "Add attachment" {
            scroll_attachment_strip(w, -10000.0);
            pump_size(w, PHONE_W, PHONE_H);
        }
        let before = callbacks.get();
        click_element(w, action);
        assert_eq!(callbacks.get(), before + 1);
        assert_eq!(w.get_composer_text().as_str(), "ab😀cd");
        assert!(!input.has_focus());
        w.window()
            .dispatch_event(WindowEvent::WindowActiveChanged(false));
        if selected {
            w.set_pending_attachments(slint::ModelRc::new(slint::VecModel::from(vec![
                chatter_ui::PendingAttachmentData {
                    id: "retained-photo".into(),
                    filename: "unsent-photo.jpg".into(),
                    category: "image".into(),
                    is_ready: true,
                    ..Default::default()
                },
            ])));
        }
        w.window()
            .dispatch_event(WindowEvent::WindowActiveChanged(true));
        w.invoke_finish_attachment_picker(true);
        pump_size(w, PHONE_W, PHONE_H);
        assert!(composer_native_item(w) == item);
        assert_eq!(w.get_composer_text().as_str(), "ab😀cd");
        assert_eq!(
            (
                input.anchor_position_byte_offset(),
                input.cursor_position_byte_offset()
            ),
            (6, 2)
        );
        assert!(input.has_focus());
        if selected || action == "Add attachment" {
            assert_eq!(w.get_pending_attachments().row_count(), 1);
            assert_eq!(
                w.get_pending_attachments().row_data(0).unwrap().id.as_str(),
                "retained-photo"
            );
        }
    }
    // Android pause/resume without a picker also retains committed Unicode
    // and the native reversed selection (WindowActivation preserves anchors).
    input.set_selection_offsets(&adapter, &item, 6, 2);
    w.window()
        .dispatch_event(WindowEvent::WindowActiveChanged(false));
    w.window()
        .dispatch_event(WindowEvent::WindowActiveChanged(true));
    assert_eq!(w.get_composer_text().as_str(), "ab😀cd");
    assert_eq!(
        (
            input.anchor_position_byte_offset(),
            input.cursor_position_byte_offset()
        ),
        (6, 2)
    );
    // Preedit must be settled before the dialog destroys native focus.
    input.set_selection_offsets(&adapter, &item, 2, 2);
    composition(w, "é", false);
    click_element(w, "Attach file");
    assert_eq!(w.get_composer_text().as_str(), "abé😀cd");
    w.invoke_finish_attachment_picker(true);
    pump_size(w, PHONE_W, PHONE_H);
    assert_eq!(input.cursor_position_byte_offset(), 4);
    assert_eq!(input.preedit_text().as_str(), "");
    // A return for a departed conversation must not seize the new input.
    click_element(w, "Attach file");
    w.set_composer_text("another conversation".into());
    w.invoke_finish_attachment_picker(false);
    pump_size(w, PHONE_W, PHONE_H);
    assert!(!input.has_focus());
    assert_eq!(w.get_composer_text().as_str(), "another conversation");

    let window_item = i_slint_core::window::WindowInner::from_pub(w.window())
        .window_item_rc()
        .unwrap();
    let window = window_item
        .downcast::<i_slint_core::items::WindowItem>()
        .unwrap();
    let window = window.as_pin_ref();
    let keyboard = |height: f32| {
        window
            .virtual_keyboard_size
            .set(i_slint_core::lengths::LogicalSize::new(PHONE_W, height));
        i_slint_core::properties::ChangeTracker::run_change_handlers();
        slint::platform::update_timers_and_animations();
    };
    let field_bottom = || {
        let field = one(w, "Message input");
        field.absolute_position().y + field.size().height
    };
    w.set_pending_attachments(slint::ModelRc::new(slint::VecModel::from(Vec::<
        chatter_ui::PendingAttachmentData,
    >::new())));
    w.set_composer_emoji_list(slint::ModelRc::new(slint::VecModel::from(vec![
        chatter_ui::EmojiData {
            emoji: "😀".into(),
            name: "fixture emoji".into(),
            category: 0,
        },
    ])));
    w.set_composer_gif_picker_results(slint::ModelRc::new(slint::VecModel::from(vec![
        chatter_ui::GifData {
            id: "fixture-gif".into(),
            width: 100,
            height: 100,
            ..Default::default()
        },
    ])));
    let drawer_element = |id: &str| {
        testing::ElementHandle::find_by_element_id(w, id)
            // Results may be bounded to zero on the explicit short-pane case;
            // the per-size useful-area assertions below remain authoritative.
            .find(|e| {
                e.size().width > 0.0
                    && (e.size().height > 0.0
                        || (id == "ChatLayout::picker-results" && e.size().height == 0.0))
            })
            .unwrap_or_else(|| panic!("visible drawer item {id}"))
    };
    let focused_search = || {
        use std::ops::ControlFlow;
        window_item
            .visit_descendants(|item| {
                if let Some(input) = item.downcast::<i_slint_core::items::TextInput>() {
                    if input.as_pin_ref().has_focus() {
                        return ControlFlow::Break(item.clone());
                    }
                }
                ControlFlow::Continue(())
            })
            .expect("focused native drawer search")
    };
    // Search owns a visible drawer ABOVE the actual IME. Exercise the real
    // search input and tab callbacks; a zero-height clipped picker cannot pass.
    for (width, height, ime) in [
        (320.0, 640.0, 260.0),
        (PHONE_W, PHONE_H, 340.0),
        (768.0, 1024.0, 360.0),
        (320.0, 640.0, 360.0), // short remaining pane: both tabs may hit the cap
    ] {
        keyboard(0.0);
        w.set_composer_text("".into());
        w.set_composer_emoji_picker_visible(true);
        w.set_composer_gif_picker_visible(false);
        w.set_composer_emoji_search("".into());
        w.set_composer_gif_picker_search("".into());
        pump_size(w, width, height);
        let baseline = rect(&drawer_element("ChatLayout::picker-drawer"));
        click_element(w, "Search emoji and GIFs");
        let search_item = focused_search();
        // Focus alone, without actual IME, must keep the bottom baseline.
        pump_size(w, width, height);
        assert!((rect(&drawer_element("ChatLayout::picker-drawer")).1 - baseline.1).abs() < 0.5);
        keyboard(ime);
        pump_size(w, width, height);
        let mut emoji_top = 0.0;
        let mut emoji_height = 0.0;
        for (tab, typed) in [("Emoji", "e"), ("GIFs", "g")] {
            if tab == "GIFs" {
                click_element(w, tab);
            }
            key(w, typed.into());
            pump_size(w, width, height);
            assert_eq!(
                if tab == "Emoji" {
                    w.get_composer_emoji_search()
                } else {
                    w.get_composer_gif_picker_search()
                }
                .as_str(),
                typed,
                "search must retain focus across IME resize and tab switch"
            );
            let (dx, dy, dw, dh) = rect(&drawer_element("ChatLayout::picker-drawer"));
            let (sx, sy, sw, sh) = rect(&one(w, "Search emoji and GIFs"));
            let (rx, ry, rw, rh) = rect(&drawer_element("ChatLayout::picker-results"));
            let (_, ty, _, th) = rect(&one(w, tab));
            assert!(
                focused_search() == search_item,
                "tab/IME resize replaced search input"
            );
            assert!(dy < baseline.1 - 1.0, "search did not move UP with IME");
            let (_, hy, _, hh) = rect(&drawer_element("ChatLayout::top-chrome"));
            let (_, cy, _, _) = rect(&drawer_element("ChatLayout::composer"));
            assert!(cy >= hy + hh - 0.5, "drawer pushed composer through header");
            assert!(dy + dh <= height - ime + 0.5, "drawer overlaps actual IME");
            if tab == "Emoji" {
                emoji_top = dy;
                emoji_height = dh;
            } else {
                assert!(dy <= emoji_top + 0.5 && dh >= emoji_height - 0.5);
                if height - ime >= 380.0 {
                    assert!(
                        dy < emoji_top - 1.0,
                        "GIF search must move HIGHER than Emoji at {width}x{height}/{ime}: emoji top={emoji_top}, height={emoji_height}; GIF top={dy}, height={dh}"
                    );
                }
            }
            assert!(sx >= dx && sx + sw <= dx + dw && sy >= dy && sh > 0.0);
            assert!(rx >= dx && rx + rw <= dx + dw + 0.5 && ry >= sy + sh);
            assert!(
                rh >= if height - ime >= 380.0 { 48.0 } else { 0.0 },
                "search leaves no useful results at {width}x{height}: {rh}"
            );
            assert!(ry + rh <= ty + 0.5 && ty + th <= dy + dh + 0.5,
                "drawer containment {width}x{height}/IME{ime}/{tab}: drawer {dy},{dh}; results {ry},{rh}; tab {ty},{th}; composer {cy}; header {hy},{hh}");
            let grid = testing::ElementHandle::find_by_element_id(
                w,
                if tab == "Emoji" {
                    "EmojiPicker::emoji-grid"
                } else {
                    "GifPicker::gif-grid"
                },
            )
            .next()
            .unwrap_or_else(|| panic!("actual {tab} grid missing at {width}x{height}/IME{ime}: emoji={}, gif={}, loading={}, results={}; drawer={dy},{dh}; results-rect={ry},{rh}; tab={ty},{th}", w.get_composer_emoji_picker_visible(), w.get_composer_gif_picker_visible(), w.get_composer_gif_picker_loading(), w.get_composer_gif_picker_results().row_count()));
            let (_, gy, _, gh) = rect(&grid);
            assert!(
                gh >= if height - ime >= 380.0 { 32.0 } else { 0.0 }
                    && gy >= ry
                    && gy + gh <= ry + rh + 0.5,
                "actual {tab} results viewport is not useful/visible: {gh}"
            );
        }
        let native = search_item
            .downcast::<i_slint_core::items::TextInput>()
            .unwrap();
        let native = native.as_pin_ref();
        let caret = native.cursor_position_byte_offset();
        let anchor = native.anchor_position_byte_offset();
        let gif_results = w.get_composer_gif_picker_results();
        let first_gif = gif_results.row_data(0).expect("fixture GIF result");
        keyboard(0.0);
        pump_size(w, width, height);
        let closed = rect(&drawer_element("ChatLayout::picker-drawer"));
        assert!(
            (closed.1 - baseline.1).abs() < 0.5 && (closed.3 - baseline.3).abs() < 0.5,
            "keyboard close must move DOWN to original baseline despite retained focus"
        );
        assert!(focused_search() == search_item);
        assert_eq!(
            (
                native.cursor_position_byte_offset(),
                native.anchor_position_byte_offset()
            ),
            (caret, anchor)
        );
        assert_eq!(w.get_composer_emoji_search().as_str(), "e");
        assert_eq!(w.get_composer_gif_picker_search().as_str(), "g");
        assert!(w.get_composer_gif_picker_visible() && !w.get_composer_emoji_picker_visible());
        assert_eq!(
            w.get_composer_gif_picker_results().row_count(),
            gif_results.row_count()
        );
        let retained_gif = w
            .get_composer_gif_picker_results()
            .row_data(0)
            .expect("retained GIF result");
        assert_eq!(
            (retained_gif.id, retained_gif.full_url),
            (first_gif.id, first_gif.full_url)
        );
        // Reopen the IME on the same input, without a new pointer focus event.
        keyboard(ime);
        pump_size(w, width, height);
        assert!(rect(&drawer_element("ChatLayout::picker-drawer")).1 < closed.1 - 1.0);
        key(w, "x".into());
        assert_eq!(w.get_composer_gif_picker_search().as_str(), "gx");
        keyboard(0.0);
        w.set_composer_emoji_picker_visible(false);
        w.set_composer_gif_picker_visible(false);
        pump_size(w, width, height);
        // Destroy/reopen with a live inset: stale search focus must not expand.
        keyboard(ime);
        w.set_composer_emoji_picker_visible(true);
        pump_size(w, width, height);
        let reopened = testing::ElementHandle::find_by_element_id(w, "ChatLayout::picker-drawer")
            .next()
            .expect("reopened drawer");
        let (_, _, _, unfocused_height) = rect(&reopened);
        assert!(
            unfocused_height <= (baseline.3 - ime).max(0.0) + 0.5,
            "destroy/reopen retained stale search-focus expansion"
        );
        w.set_composer_emoji_picker_visible(false);
        keyboard(0.0);
        pump_size(w, width, height);
        if width == PHONE_W {
            // The production chat-navigation handler closes both tabs and
            // destroys the drawer. Reopening must require fresh search focus.
            w.set_composer_gif_picker_visible(true);
            pump_size(w, width, height);
            click_element(w, "Search emoji and GIFs");
            keyboard(ime);
            pump_size(w, width, height);
            w.set_mobile_show_chat(false);
            pump_size(w, width, height);
            assert!(!w.get_composer_emoji_picker_visible() && !w.get_composer_gif_picker_visible());
            w.set_mobile_show_chat(true);
            w.set_composer_emoji_picker_visible(true);
            pump_size(w, width, height);
            let reopened =
                testing::ElementHandle::find_by_element_id(w, "ChatLayout::picker-drawer")
                    .next()
                    .expect("drawer after chat navigation");
            assert!(reopened.size().height <= (baseline.3 - ime).max(0.0) + 0.5);
            w.set_composer_emoji_picker_visible(false);
            keyboard(0.0);
            pump_size(w, width, height);
        }
    }
    keyboard(300.0);
    pump_size(w, PHONE_W, PHONE_H);
    click_element(w, "Emoji & GIF picker");
    i_slint_core::properties::ChangeTracker::run_change_handlers();
    let while_hiding = field_bottom();
    keyboard(0.0);
    pump_size(w, PHONE_W, PHONE_H);
    assert!(
        (field_bottom() - while_hiding).abs() < 1.0,
        "keyboard-to-drawer double-counted occupied space"
    );
    let drawer_bottom = field_bottom();
    click_element(w, "Show keyboard");
    i_slint_core::properties::ChangeTracker::run_change_handlers();
    assert!(!w.get_composer_emoji_picker_visible());
    assert!(
        (field_bottom() - drawer_bottom).abs() < 1.0,
        "drawer-to-keyboard exposed a full-height frame before IME insets"
    );
    keyboard(300.0);
    pump_size(w, PHONE_W, PHONE_H); // real settle timer releases the handover
    let with_keyboard = field_bottom();
    keyboard(0.0);
    pump_size(w, PHONE_W, PHONE_H);
    assert!(
        field_bottom() > with_keyboard + 250.0,
        "keyboard handover left a sticky spacer"
    );
    // A positive inset interrupted by zero must cancel its settle timer,
    // retaining the drawer reservation past the old positive deadline.
    click_element(w, "Emoji & GIF picker");
    pump_size(w, PHONE_W, PHONE_H);
    let drawer_bottom = field_bottom();
    click_element(w, "Show keyboard");
    i_slint_core::properties::ChangeTracker::run_change_handlers();
    keyboard(100.0); // shorter than the drawer
    assert!((field_bottom() - drawer_bottom).abs() < 1.0);
    keyboard(0.0);
    pump_size(w, PHONE_W, PHONE_H); // 200 ms exceeds the cancelled 120 ms settle
    assert!(
        (field_bottom() - drawer_bottom).abs() < 1.0,
        "interrupted positive inset released the drawer reservation"
    );
    keyboard(100.0);
    assert!(
        (field_bottom() - drawer_bottom).abs() < 1.0,
        "resumed short keyboard released reservation before settling"
    );
    pump_size(w, PHONE_W, PHONE_H);
    let short_keyboard_bottom = field_bottom();
    assert!(
        short_keyboard_bottom > drawer_bottom + 150.0,
        "settled short keyboard retained the taller drawer reservation"
    );
    keyboard(0.0);
    pump_size(w, PHONE_W, PHONE_H);
    assert!(
        (field_bottom() - short_keyboard_bottom - 100.0).abs() < 1.0,
        "short keyboard handover left a sticky spacer"
    );
    // Back/cancel must release even when the keyboard request is refused.
    click_element(w, "Emoji & GIF picker");
    pump_size(w, PHONE_W, PHONE_H);
    click_element(w, "Show keyboard");
    i_slint_core::properties::ChangeTracker::run_change_handlers();
    let reserved = field_bottom();
    w.invoke_release_text_focus();
    i_slint_core::properties::ChangeTracker::run_change_handlers();
    assert!(field_bottom() > reserved + 250.0);
    // The production pause listener calls this cancellation boundary; it must
    // also drop an outstanding reservation before navigation/backgrounding.
    click_element(w, "Emoji & GIF picker");
    pump_size(w, PHONE_W, PHONE_H);
    click_element(w, "Show keyboard");
    i_slint_core::properties::ChangeTracker::run_change_handlers();
    let reserved = field_bottom();
    w.invoke_cancel_composer_keyboard_handover();
    i_slint_core::properties::ChangeTracker::run_change_handlers();
    assert!(field_bottom() > reserved + 250.0);
    click_element(w, "Emoji & GIF picker");
    pump_size(w, PHONE_W, PHONE_H);
    click_element(w, "Show keyboard");
    i_slint_core::properties::ChangeTracker::run_change_handlers();
    let reserved = field_bottom();
    w.set_channel_id("picker-handover-next".into());
    i_slint_core::properties::ChangeTracker::run_change_handlers();
    assert!(field_bottom() > reserved + 250.0);
    click_element(w, "Emoji & GIF picker");
    pump_size(w, PHONE_W, PHONE_H);
    click_element(w, "Show keyboard");
    i_slint_core::properties::ChangeTracker::run_change_handlers();
    let reserved = field_bottom();
    slint::Timer::single_shot(std::time::Duration::from_millis(1600), || {
        slint::quit_event_loop().unwrap();
    });
    slint::run_event_loop().unwrap();
    assert!(
        field_bottom() > reserved + 250.0,
        "refused keyboard request left a sticky spacer"
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
    // The type query includes inherited material elements; require the root.
    let shell = testing::ElementHandle::find_by_element_type_name(w, "MessageComposer")
        .find(|e| e.type_name().is_some_and(|t| t == "MessageComposer") && e.size().height > 0.0)
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
    let toolbar = if input.text() != ""
        || input.preedit_text() != ""
        || w.get_pending_attachments().row_count() > 0
    {
        60.0
    } else {
        0.0
    };
    // The accepted 48px floor is explicit for unusually short panes/accessories.
    let cap =
        48.0_f32.max((line_height * 6.0 + 16.0).min(available * 0.4 - accessories - toolbar - 8.0));
    assert!(
        shell.size().height <= cap + accessories + toolbar + 8.5,
        "composer {:?} exceeds six-line/actual-pane cap {cap}, pane={available}, accessories={accessories}",
        shell.size()
    );
}

fn composer_shell_height(w: &MainWindow) -> f32 {
    testing::ElementHandle::find_by_element_type_name(w, "MessageComposer")
        .find(|e| e.type_name().is_some_and(|t| t == "MessageComposer") && e.size().height > 0.0)
        .expect("actual composer root")
        .size()
        .height
}

/// Check actual controls against the actual field: no duplicate layout model.
fn assert_action_geometry(w: &MainWindow, raised: bool) {
    let shell = testing::ElementHandle::find_by_element_type_name(w, "MessageComposer")
        .find(|e| e.type_name().is_some_and(|t| t == "MessageComposer") && e.size().height > 0.0)
        .unwrap();
    let (sx, sy, sw, sh) = rect(&shell);
    let (fx, fy, fw, fh) = rect(&one(w, "Message input"));
    let emoji_label =
        if w.get_composer_emoji_picker_visible() || w.get_composer_gif_picker_visible() {
            "Show keyboard"
        } else {
            "Emoji & GIF picker"
        };
    let (ex, ey, ew, eh) = rect(&one(w, emoji_label));
    let mesh = w.global::<chatter_ui::MeshGlobal>();
    let attachment_visible = !w.get_composer_is_editing()
        && (mesh.get_active_id().is_empty() || mesh.get_active_kind() != "dm");
    let send_visible = raised || w.get_pending_attachments().row_count() > 0;
    let mic_visible =
        !send_visible && !w.get_composer_is_editing() && mesh.get_active_id().is_empty();
    let terminal_label = if send_visible {
        Some("Send message")
    } else if mic_visible {
        Some("Record voice message")
    } else {
        None
    };
    let terminal = terminal_label.map(|label| rect(&one(w, label)));
    assert!((ex - sx - 4.0).abs() < 0.5);
    assert!(ew >= 48.0 && eh >= 48.0);
    assert!(fx >= sx && fx + fw <= sx + sw + 0.5 && fy + fh <= sy + sh + 0.5);
    if raised {
        assert!((fx - sx - 4.0).abs() < 0.5);
        assert!(
            (fw - (sw - 8.0 - 52.0)).abs() < 0.5,
            "populated field must reserve the lower Send target and gap"
        );
        // 4px bottom inset + 4px inter-shell gap + 4px field inset.
        assert!((fy - (ey + eh) - 12.0).abs() < 0.5);
        // Require primary surface metadata, never an inherited material layer.
        let surface = |id: &str| {
            testing::ElementHandle::find_by_element_id(w, id)
                .find(|e| {
                    e.type_name().is_some_and(|t| t == "GlassSurface") && e.size().height > 0.0
                })
                .expect("raised composer surface")
        };
        let (lx, ly, lw, lh) = rect(&surface("MessageComposer::left-action-surface"));
        let right_surface =
            attachment_visible.then(|| rect(&surface("MessageComposer::right-action-surface")));
        let (ix, iy, iw, ih) = rect(&surface("MessageComposer::field-surface"));
        assert!((ex - lx - 4.0).abs() < 0.5 && (ey - ly - 4.0).abs() < 0.5);
        assert!((lw - 56.0).abs() < 0.5 && (lh - 56.0).abs() < 0.5);
        if let Some((rx, ry, rw, rh)) = right_surface {
            assert!(
                rx > lx + lw,
                "button shells must leave a transparent middle"
            );
            assert!((rh - 56.0).abs() < 0.5 && (ry - ly).abs() < 0.5);
            assert!(
                (rw - 56.0).abs() < 0.5,
                "top right shell contains Attach only"
            );
        }
        assert!((iy - ly - lh - 4.0).abs() < 0.5);
        assert!((ix - sx).abs() < 0.5 && (iw - sw).abs() < 0.5);
        assert!((fx - ix - 4.0).abs() < 0.5 && (fy - iy - 4.0).abs() < 0.5);
        assert!((ih - fh - 8.0).abs() < 0.5);
    } else {
        assert!((fx - (ex + ew) - 4.0).abs() < 0.5);
        assert!((ey - fy).abs() < 0.5);
        let occupied = 52.0
            + if attachment_visible { 52.0 } else { 0.0 }
            + if terminal.is_some() { 52.0 } else { 0.0 };
        assert!(
            (fw - (sw - 8.0 - occupied)).abs() < 0.5,
            "empty field reserves hidden actions"
        );
    }
    if let Some((tx, ty, tw, th)) = terminal {
        assert!((sx + sw - tx - tw - 4.0).abs() < 0.5);
        assert!(tw >= 48.0 && th >= 48.0);
        if raised {
            assert!(
                (ty + th - fy - fh).abs() < 0.5,
                "Send must stay at the lower field bottom"
            );
            assert!(
                (tx - fx - fw - 4.0).abs() < 0.5,
                "field and Send targets must not overlap"
            );
            assert!(
                ty >= ey + eh + 12.0,
                "raised top row must never contain Send"
            );
        } else {
            assert!((ty - ey).abs() < 0.5);
        }
    }
    if attachment_visible {
        let (ax, ay, aw, ah) = rect(&one(w, "Attach file"));
        if raised {
            assert!((sx + sw - ax - aw - 4.0).abs() < 0.5);
        } else if let Some((tx, _, _, _)) = terminal {
            assert!((tx - ax - aw - 4.0).abs() < 0.5);
        } else {
            assert!((sx + sw - ax - aw - 4.0).abs() < 0.5);
        }
        assert!((ay - ey).abs() < 0.5 && aw >= 48.0 && ah >= 48.0);
        if raised {
            assert!(ay + ah <= fy);
        }
    }
    for (label, expected) in [
        ("Attach file", attachment_visible),
        ("Send message", send_visible),
        ("Record voice message", mic_visible),
    ] {
        assert_eq!(
            testing::ElementHandle::find_by_accessible_label(w, label)
                .filter(|e| e.size().height > 0.0)
                .count(),
            usize::from(expected),
            "unexpected action {label}"
        );
    }
    assert!(composer_native_item(w)
        .downcast::<i_slint_core::items::TextInput>()
        .is_some());
}

fn click_element(w: &MainWindow, label: &str) {
    let (x, y, width, height) = rect(&one(w, label));
    click_at(w, x + width / 2.0, y + height / 2.0);
}

/// Query the actual clip and surfaces so a tray above the row cannot pass.
fn assert_attachment_strip_geometry(w: &MainWindow) {
    let element = |id: &str| {
        testing::ElementHandle::find_by_element_id(w, id)
            .find(|e| e.size().width > 0.0 && e.size().height > 0.0)
            .expect("visible attachment row element")
    };
    let (x, y, width, height) = rect(&element("MessageComposer::attachment-strip"));
    let (lx, ly, lw, lh) = rect(&element("MessageComposer::left-action-surface"));
    let (rx, ry, _, rh) = rect(&element("MessageComposer::right-action-surface"));
    let (_, fy, _, _) = rect(&one(w, "Message input"));
    assert!((x - lx - lw - 4.0).abs() < 0.5);
    assert!((rx - x - width - 4.0).abs() < 0.5);
    assert!((y - ly).abs() < 0.5 && (y - ry).abs() < 0.5);
    assert!((height - lh).abs() < 0.5 && (height - rh).abs() < 0.5);
    assert!(width > 0.0 && y + height < fy);
    let scroll = element("MessageComposer::attachment-scroll");
    let (cx, cy, cw, ch) = rect(&scroll);
    assert!((cx - x).abs() < 0.5 && (cy - y).abs() < 0.5);
    assert!((cw - width).abs() < 0.5 && (ch - height).abs() < 0.5);
    for remove in testing::ElementHandle::find_by_accessible_label(w, "Remove attachment") {
        let (_, ry, rw, rh) = rect(&remove);
        assert!(rw >= 48.0 && rh >= 48.0);
        assert!(ry >= y - 0.5 && ry + rh <= y + height + 0.5);
    }
}

fn scroll_attachment_strip(w: &MainWindow, delta_x: f32) {
    let strip = testing::ElementHandle::find_by_element_id(w, "MessageComposer::attachment-strip")
        .find(|e| e.size().width > 0.0 && e.size().height > 0.0)
        .expect("attachment strip");
    let (x, y, width, height) = rect(&strip);
    w.window()
        .dispatch_event(slint::platform::WindowEvent::PointerScrolled {
            position: slint::LogicalPosition::new(x + width / 2.0, y + height / 2.0),
            delta_x,
            delta_y: 0.0,
        });
}

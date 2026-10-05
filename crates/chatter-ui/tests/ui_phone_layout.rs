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
    w.window().set_size(slint::LogicalSize::new(PHONE_W, PHONE_H));
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
    assert!(screen_h > 400.0, "SettingsScreen has no real height ({screen_h})");
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
        let flick = kids.iter().find(|k| k.0 == "Flickable").expect("Flickable").1;
        let content = kids.iter().find(|k| k.0 == "VerticalLayout").expect("rows layout").1;
        let row0 = kids.iter().filter(|k| k.0 == "VerticalLayout").nth(1).expect("row 0").1;
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
    assert!(flick.1 + flick.3 <= PHONE_H + 0.5, "records list runs off the screen");
    assert!(content.3 > flick.3, "40 records do not overflow the list, so it cannot scroll");
    let before = content.1;
    w.window().dispatch_event(slint::platform::WindowEvent::PointerScrolled {
        position: slint::LogicalPosition::new(200.0, flick.1 + 200.0),
        delta_x: 0.0,
        delta_y: -300.0,
    });
    pump(&w);
    let (_, scrolled, _) = records(&w);
    let after = scrolled.1;
    println!("records content y before scroll={before}, after={after}");
    assert!(after < before - 50.0, "the records list did not scroll (y {before} -> {after})");
}

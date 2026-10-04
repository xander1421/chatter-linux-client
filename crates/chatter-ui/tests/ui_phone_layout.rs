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

use chatter_ui::MainWindow;
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
}

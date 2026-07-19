//! VALIDATED-UI layout regression — geometry-level proofs.
//!
//! `ui_wiring.rs` proves callbacks reach core; this file proves elements LAND
//! ON SCREEN. Separate test binary because the integration testing backend
//! (needed for real layout passes) is once-per-process and `ui_wiring.rs`
//! already claims it for the settings-modal routing test.
//!
//! Regression under test: the thread panel opened (state set, panel visible in
//! the fullscreen-call layout) but never appeared in the NORMAL desktop layout
//! — reported 2026-07-19 with screenshots. A wiring test can't see this class
//! of bug: the conditional instantiates fine; it's the layout pass that must
//! place it inside the window.

use chatter_ui::MainWindow;
use i_slint_backend_testing as testing;
use slint::ComponentHandle;

const WIN_W: f32 = 1400.0;
const WIN_H: f32 = 900.0;

/// Force a real layout pass: resize to the given size and spin the event loop
/// briefly so conditional sub-trees materialize and get geometry. Varying the
/// size between pumps forces a full relayout (a same-size event is a no-op).
fn pump_at(w: &MainWindow, width: f32, height: f32) {
    let scale = w.window().scale_factor();
    w.window().dispatch_event(slint::platform::WindowEvent::Resized {
        size: slint::LogicalSize::new(width, height).to_physical(scale).to_logical(scale),
    });
    slint::Timer::single_shot(std::time::Duration::from_millis(80), || {
        slint::quit_event_loop().unwrap();
    });
    w.run().unwrap();
}

fn pump(w: &MainWindow) {
    pump_at(w, WIN_W, WIN_H);
}

#[test]
fn thread_panel_lands_inside_the_normal_desktop_layout() {
    testing::init_integration_test_with_system_time();
    let w = MainWindow::new().expect("MainWindow::new");
    w.set_current_view("main".into());

    let count =
        || testing::ElementHandle::find_by_element_type_name(&w, "ThreadPanel").count();

    // Negative control: panel closed → not in the tree.
    pump(&w);
    assert_eq!(count(), 0, "ThreadPanel present before thread-panel-visible was set");

    // Open the panel exactly the way the Rust thread handler does.
    w.set_thread_panel_visible(true);
    w.set_thread_root_sender_name("Test Sender".into());
    w.set_thread_root_content_preview("root message".into());
    w.set_thread_channel_name("test-channel".into());
    pump(&w);

    let panel = testing::ElementHandle::find_by_element_type_name(&w, "ThreadPanel")
        .next()
        .expect("thread-panel-visible=true did not surface a visible ThreadPanel");

    let pos = panel.absolute_position();
    let size = panel.size();
    // NOTE: the testing backend ignores Resized events — assert against the
    // ACTUAL window width, not the requested one.
    let win = w.window().size().to_logical(w.window().scale_factor());
    println!(
        "window={}x{} panel: x={} y={} w={} h={}",
        win.width, win.height, pos.x, pos.y, size.width, size.height
    );

    // The panel must occupy real width…
    assert!(
        size.width >= 200.0,
        "ThreadPanel laid out with width {} — squeezed out of the layout",
        size.width
    );
    // …and sit fully INSIDE the window (the reported bug: panel pushed past the
    // right edge and clipped invisible while the chat column kept full width).
    assert!(
        pos.x >= 0.0 && pos.x + size.width <= win.width + 0.5,
        "ThreadPanel laid out OFF-WINDOW: x={} width={} window-width={}",
        pos.x,
        size.width,
        win.width
    );
    // Desktop layout: panel docks at the right edge.
    assert!(
        (pos.x + size.width) >= win.width - 40.0,
        "ThreadPanel not docked at the right edge: x={} width={} window-width={}",
        pos.x,
        size.width,
        win.width
    );
}

//! Exercise the real edge gesture, pointer capture, and responsive width policy.
use chatter_ui::{DesktopChrome, MainWindow, ModalHostState, SearchPane, SidebarPane, SidebarRowData};
use i_slint_backend_testing::{self as testing, ElementHandle};
use slint::{ComponentHandle, LogicalPosition, LogicalSize, ModelRc, VecModel};
use slint::platform::{PointerEventButton, WindowEvent};
use std::{cell::Cell, rc::Rc, time::Duration};

fn settle(w: &MainWindow) {
    i_slint_core::properties::ChangeTracker::run_change_handlers();
    slint::Timer::single_shot(Duration::from_millis(350), || {
        slint::quit_event_loop().unwrap();
    });
    w.run().unwrap();
}

fn size(w: &MainWindow, width: f32) {
    let size = LogicalSize::new(width, 800.0);
    w.window().set_size(size);
    w.window().dispatch_event(WindowEvent::Resized { size });
    settle(w);
}

fn drag(w: &MainWindow, targets: &[f32]) {
    let start = LogicalPosition::new(8.0 + w.get_sidebar_panel_width(), 350.0);
    w.window().dispatch_event(WindowEvent::PointerMoved { position: start });
    w.window().dispatch_event(WindowEvent::PointerPressed {
        position: start, button: PointerEventButton::Left,
    });
    for target in targets {
        w.window().dispatch_event(WindowEvent::PointerMoved {
            position: LogicalPosition::new(8.0 + target, start.y),
        });
        settle(w);
    }
    w.window().dispatch_event(WindowEvent::PointerReleased {
        position: LogicalPosition::new(8.0 + targets.last().unwrap(), start.y),
        button: PointerEventButton::Left,
    });
    settle(w);
}

fn width(w: &MainWindow, expected: f32) {
    assert!((w.get_sidebar_panel_width() - expected).abs() < 1.0,
        "expected {expected}, got {}", w.get_sidebar_panel_width());
}

#[test]
fn edge_drag_survives_collapse_and_preserves_manual_width_across_window_sizes() {
    testing::init_integration_test_with_system_time();
    let w = MainWindow::new().unwrap();
    w.set_current_view("main".into());
    chatter_ui::bin_ui::features::back::wire(&w);
    let sidebar = w.global::<SidebarPane>();
    // Production sidebar wiring populates the virtualized row model. This
    // headless window only wires Back, so provide the quick-link rows here.
    let rows: Vec<_> = ["search", "saved", "friends", "calls", "bottom"]
        .into_iter()
        .map(|kind| SidebarRowData {
            key: kind.into(),
            kind: kind.into(),
            ..Default::default()
        })
        .collect();
    sidebar.set_flat_rows(ModelRc::new(VecModel::from(rows)));
    w.global::<DesktopChrome>().set_macos(true);
    w.global::<DesktopChrome>().set_frame_inset(8.0);
    w.set_macos_titlebar_height(34.0);
    w.set_macos_traffic_lights_width(80.0);
    w.show().unwrap();
    size(&w, 1100.0);
    let saves = Rc::new(Cell::new(0));
    let saved = saves.clone();
    sidebar.on_toggle_sidebar(move || saved.set(saved.get() + 1));
    width(&w, 256.0);
    assert!(ElementHandle::find_by_accessible_label(&w, "Resize sidebar").next().is_some());
    for label in ["Expand sidebar", "Collapse sidebar"] {
        assert!(ElementHandle::find_by_accessible_label(&w, label).next().is_none());
    }

    // Modal accessibility intentionally hides the underlying page. Keep the
    // original element handle to inspect its geometry behind the overlay.
    let edge = ElementHandle::find_by_accessible_label(&w, "Resize sidebar")
        .next().unwrap();
    let edge_position = || edge.absolute_position();
    let before_overlay = edge_position();
    assert!((before_overlay.y - 8.0).abs() < 1.0, "extra native titlebar band: {before_overlay:?}");
    w.global::<ModalHostState>().invoke_nav_push("create-section".into());
    settle(&w);
    assert_eq!(edge_position(), before_overlay, "modal moved the base layout");
    w.global::<ModalHostState>().invoke_nav_pop();
    w.global::<SearchPane>().set_unified_search_open(true);
    settle(&w);
    assert_eq!(edge_position(), before_overlay, "search moved the base layout");
    w.global::<SearchPane>().set_unified_search_open(false);
    w.global::<ModalHostState>().set_settings_open(true);
    settle(&w);
    assert_eq!(edge_position(), before_overlay, "settings moved the base layout");
    w.global::<ModalHostState>().set_settings_open(false);
    settle(&w);

    drag(&w, &[300.0, 360.0]);
    width(&w, 360.0);
    assert_eq!(saves.get(), 1, "save once on release, not on every move");
    drag(&w, &[240.0, 100.0]);
    width(&w, 68.0);
    assert!(sidebar.get_sidebar_user_collapsed());
    // The desktop rail keeps its navigation; the Shield stays in the header.
    for label in ["Profile", "Settings", "Activity", "Friends", "Calls"] {
        assert!(ElementHandle::find_by_accessible_label(&w, label).next().is_some(),
            "{label} should be reachable from the collapsed rail");
    }
    assert!(ElementHandle::find_by_accessible_label(&w, "Anon mode").next().is_none());
    // Retain the last expanded width, rather than saving the 68px rail.
    assert_eq!(sidebar.get_sidebar_preferred_width(), 240.0);
    // Keep the same pointer capture while the handle jumps from rail to list.
    drag(&w, &[200.0, 300.0]);
    width(&w, 300.0);
    assert!(!sidebar.get_sidebar_user_collapsed());
    size(&w, 700.0);
    width(&w, 68.0);
    size(&w, 1100.0);
    width(&w, 300.0);

    drag(&w, &[100.0]);
    size(&w, 700.0);
    size(&w, 1100.0);
    width(&w, 68.0);
    drag(&w, &[300.0, 900.0]);
    width(&w, 480.0);
    size(&w, 800.0);
    width(&w, 464.0);
    assert_eq!(sidebar.get_sidebar_preferred_width(), 480.0);
    size(&w, 1100.0);
    width(&w, 480.0);
}

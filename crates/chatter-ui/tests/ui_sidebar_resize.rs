//! Exercise the real edge gesture, pointer capture, and responsive width policy.
use chatter_ui::{
    ChatRowData, DesktopChrome, MainWindow, ModalHostState, SearchPane, SidebarCard,
    SidebarCardRow, SidebarCards, SidebarPane,
};
use i_slint_backend_testing::{self as testing, ElementHandle};
use slint::platform::{PointerEventButton, WindowEvent};
use slint::{ComponentHandle, LogicalPosition, LogicalSize, ModelRc, VecModel};
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
    w.window()
        .dispatch_event(WindowEvent::PointerMoved { position: start });
    w.window().dispatch_event(WindowEvent::PointerPressed {
        position: start,
        button: PointerEventButton::Left,
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
    assert!(
        (w.get_sidebar_panel_width() - expected).abs() < 1.0,
        "expected {expected}, got {}",
        w.get_sidebar_panel_width()
    );
}

fn assert_card_order(w: &MainWindow) {
    let card_y = |label: &str| {
        ElementHandle::find_by_accessible_label(w, label)
            .find(|element| element.accessible_role() == Some(testing::AccessibleRole::Groupbox))
            .map(|element| element.absolute_position().y)
            .unwrap_or_else(|| panic!("missing accessible sidebar card {label}"))
    };
    let saved = card_y("Saved messages");
    let chats = card_y("Chats");
    let section = card_y("Weekend plans");
    assert!(
        ElementHandle::find_by_accessible_label(w, "Katherine Johnson")
            .next()
            .is_none(),
        "a collapsed section must hide its rows from the accessibility tree"
    );
    assert!(
        saved < chats && chats < section,
        "expected Saved, Chats, then user sections"
    );
}

#[test]
fn edge_drag_survives_collapse_and_preserves_manual_width_across_window_sizes() {
    testing::init_integration_test_with_system_time();
    let w = MainWindow::new().unwrap();
    w.set_current_view("main".into());
    chatter_ui::bin_ui::features::back::wire(&w);
    let sidebar = w.global::<SidebarPane>();
    // This headless window wires Back only. Seed the production card model
    // so both responsive layouts render the same conversations.
    let row = |id: &str, kind: &str, name: &str| SidebarCardRow {
        conversation_id: id.into(),
        kind: kind.into(),
        name: name.into(),
        short_name: name.split_whitespace().next().unwrap().into(),
        unread: 2,
        muted: id == "unfiled-chat",
        chat_row: ChatRowData {
            conversation_id: id.into(),
            display_name: name.into(),
            is_pinned: id == "pinned-chat",
            accessible_label_text: name.into(),
            ..Default::default()
        },
        ..Default::default()
    };
    let rows = |rows: Vec<SidebarCardRow>| ModelRc::new(VecModel::from(rows));
    w.global::<SidebarCards>()
        .set_cards(ModelRc::new(VecModel::from(vec![
            SidebarCard {
                id: "saved".into(),
                kind: "saved".into(),
                title: "Saved messages".into(),
                rows: rows(vec![row("saved-notes", "saved", "Saved messages")]),
                ..Default::default()
            },
            SidebarCard {
                id: "chats".into(),
                kind: "chats".into(),
                title: "Chats".into(),
                pinned: rows(vec![row("pinned-chat", "dm", "Ada Lovelace")]),
                rows: rows(vec![row("unfiled-chat", "dm", "Grace Hopper")]),
                ..Default::default()
            },
            SidebarCard {
                id: "filed".into(),
                kind: "section".into(),
                title: "Weekend plans".into(),
                emoji: "🌿".into(),
                collapsed: true,
                rows: rows(vec![row("filed-chat", "dm", "Katherine Johnson")]),
                ..Default::default()
            },
        ])));
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
    assert_card_order(&w);
    let chat_row_y = |label| {
        let mut rows = ElementHandle::find_by_accessible_label(&w, label)
            .filter(|element| element.accessible_role() == Some(testing::AccessibleRole::ListItem));
        let position = rows.next().expect("missing chat row").absolute_position().y;
        assert!(rows.next().is_none(), "{label} should appear once");
        position
    };
    assert!(chat_row_y("Ada Lovelace") < chat_row_y("Grace Hopper"));
    for label in ["Profile", "Friends", "Calls", "Activity", "Settings"] {
        assert!(
            ElementHandle::find_by_accessible_label(&w, label)
                .next()
                .is_some(),
            "{label} should be reachable from the expanded sidebar footer"
        );
    }
    assert!(
        ElementHandle::find_by_accessible_label(&w, "Resize sidebar")
            .next()
            .is_some()
    );
    for label in ["Expand sidebar", "Collapse sidebar"] {
        assert!(
            ElementHandle::find_by_accessible_label(&w, label)
                .next()
                .is_none()
        );
    }

    let drop_zone = || {
        ElementHandle::find_by_accessible_label(&w, "Drop here for a new section")
            .find(|element| element.accessible_role() == Some(testing::AccessibleRole::Groupbox))
    };
    assert!(drop_zone().is_none());
    let cards = w.global::<SidebarCards>();
    cards.set_drag_row_id("unfiled-chat".into());
    cards.set_dragging(true);
    settle(&w);
    assert!(drop_zone().is_some());
    cards.set_dragging(false);
    cards.set_drag_row_id("".into());
    settle(&w);
    assert!(drop_zone().is_none());

    // Modal accessibility intentionally hides the underlying page. Keep the
    // original element handle to inspect its geometry behind the overlay.
    let edge = ElementHandle::find_by_accessible_label(&w, "Resize sidebar")
        .next()
        .unwrap();
    let edge_position = || edge.absolute_position();
    let before_overlay = edge_position();
    assert!(
        (before_overlay.y - 8.0).abs() < 1.0,
        "extra native titlebar band: {before_overlay:?}"
    );
    w.global::<ModalHostState>()
        .invoke_nav_push("create-section".into());
    settle(&w);
    assert_eq!(
        edge_position(),
        before_overlay,
        "modal moved the base layout"
    );
    w.global::<ModalHostState>().invoke_nav_pop();
    w.global::<SearchPane>().set_unified_search_open(true);
    settle(&w);
    assert_eq!(
        edge_position(),
        before_overlay,
        "search moved the base layout"
    );
    w.global::<SearchPane>().set_unified_search_open(false);
    w.global::<ModalHostState>().set_settings_open(true);
    settle(&w);
    assert_eq!(
        edge_position(),
        before_overlay,
        "settings moved the base layout"
    );
    w.global::<ModalHostState>().set_settings_open(false);
    settle(&w);

    drag(&w, &[300.0, 360.0]);
    width(&w, 360.0);
    assert_eq!(saves.get(), 1, "save once on release, not on every move");
    drag(&w, &[240.0, 100.0]);
    width(&w, 68.0);
    assert!(sidebar.get_sidebar_user_collapsed());
    assert_card_order(&w);
    for label in ["Ada Lovelace", "Grace Hopper"] {
        assert!(
            ElementHandle::find_by_accessible_label(&w, label)
                .any(|element| element.accessible_role() == Some(testing::AccessibleRole::Button)),
            "missing rail avatar {label}"
        );
    }
    for label in [
        "Profile",
        "Settings",
        "Activity",
        "Friends",
        "Calls",
        "Anon mode",
    ] {
        assert!(
            ElementHandle::find_by_accessible_label(&w, label)
                .next()
                .is_none(),
            "{label} should be absent from the collapsed desktop rail"
        );
    }
    for label in ["Search", "Create", "Expand sidebar"] {
        assert!(
            ElementHandle::find_by_accessible_label(&w, label)
                .next()
                .is_some(),
            "{label} should be reachable from the collapsed desktop rail"
        );
    }
    // Retain the last expanded width, rather than saving the 68px rail.
    assert_eq!(sidebar.get_sidebar_preferred_width(), 240.0);
    // Keep the same pointer capture while the handle jumps from rail to list.
    drag(&w, &[200.0, 300.0]);
    width(&w, 300.0);
    assert!(!sidebar.get_sidebar_user_collapsed());
    size(&w, 700.0);
    width(&w, 68.0);
    ElementHandle::find_by_accessible_label(&w, "Expand sidebar")
        .next()
        .unwrap()
        .invoke_accessible_default_action();
    settle(&w);
    assert!(w.get_sidebar_panel_width() > 68.0);

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

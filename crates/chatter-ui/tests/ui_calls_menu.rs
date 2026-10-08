//! RC73: exercise real pointer hit testing, including the floating list that
//! extends behind the Calls header. Run with --features ui-test-debug.
use chatter_ui::{CallHistoryRow, MainWindow};
use i_slint_backend_testing as testing;
use slint::platform::{PointerEventButton, WindowEvent};
use slint::{ComponentHandle, Model, ModelRc, VecModel};

// The isolated target deliberately excludes the application/profile runtime.
// Observe its cleanup boundary while executing the actual Back coordinator.
mod bin_ui {
    pub mod features {
        pub mod modals {
            pub mod profile {
                thread_local! {
                    pub static RETIRED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
                }
                pub fn retire_avatar_edit() {
                    RETIRED.with(|count| count.set(count.get() + 1));
                }
            }
        }
    }
}

// Include the actual navigation coordinator so the isolated Slint-only target
// can run the same Back code without application/core/platform initialization.
#[path = "../src/bin_ui/features/back.rs"]
mod back;

fn key(w: &MainWindow, key: slint::platform::Key) {
    let text: slint::SharedString = key.into();
    w.window()
        .dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
    w.window().dispatch_event(WindowEvent::KeyReleased { text });
}

fn settle(w: &MainWindow, width: f32, height: f32) {
    w.window().set_size(slint::LogicalSize::new(width, height));
    slint::Timer::single_shot(std::time::Duration::from_millis(200), || {
        slint::quit_event_loop().unwrap();
    });
    w.show().unwrap();
    slint::run_event_loop().unwrap();
}

fn labelled(w: &MainWindow, label: &str) -> Option<testing::ElementHandle> {
    let screen =
        testing::ElementHandle::find_by_element_type_name(w, "CallHistoryScreen").next()?;
    let mut found = None;
    screen.visit_descendants(|e| {
        if e.accessible_label().as_deref() == Some(label)
            && e.size().width > 0.0
            && e.size().height > 0.0
        {
            found = Some(e);
        }
        std::ops::ControlFlow::<()>::Continue(())
    });
    found
}

fn click(w: &MainWindow, e: &testing::ElementHandle) {
    let p = e.absolute_position();
    let s = e.size();
    let position = slint::LogicalPosition::new(p.x + s.width / 2.0, p.y + s.height / 2.0);
    click_position(w, position);
}

fn click_position(w: &MainWindow, position: slint::LogicalPosition) {
    let button = PointerEventButton::Left;
    w.window()
        .dispatch_event(WindowEvent::PointerMoved { position });
    w.window()
        .dispatch_event(WindowEvent::PointerPressed { position, button });
    w.window()
        .dispatch_event(WindowEvent::PointerReleased { position, button });
}

#[test]
fn calls_overflow_uses_real_header_hit_target_and_card_anchor() {
    assert!(
        chatter_ui::SLINT_DEBUG_INFO_ENABLED,
        "requires ui-test-debug"
    );
    testing::init_integration_test_with_system_time();
    let w = MainWindow::new().unwrap();
    back::wire(&w);
    back::push(&w, "edit-profile");
    w.set_edit_profile_loading(true);
    w.invoke_back_pressed();
    assert_eq!(w.get_modal_stack().row_count(), 0);
    assert!(!w.get_edit_profile_loading());
    bin_ui::features::modals::profile::RETIRED.with(|count| assert_eq!(count.get(), 1));
    w.set_current_view("main".into());
    w.set_connection_status("connected".into());
    w.set_call_history_rows(ModelRc::new(VecModel::from(
        (0..24)
            .map(|i| CallHistoryRow {
                id: format!("call-{i}").into(),
                conversation_id: "fixture".into(),
                conversation_type: "server_dm".into(),
                name: format!("Caller {i}").into(),
                detail: "Incoming · 0:16".into(),
                timestamp: "08:29".into(),
                can_call_back: true,
                ..Default::default()
            })
            .collect::<Vec<_>>(),
    )));
    let cleared = std::rc::Rc::new(std::cell::Cell::new(false));
    let observed = cleared.clone();
    w.on_call_history_clear(move || observed.set(true));

    let row_actions = std::rc::Rc::new(std::cell::Cell::new(0));
    let observed = row_actions.clone();
    w.on_call_history_open(move |_, _| observed.set(observed.get() + 1));
    let observed = row_actions.clone();
    w.on_call_history_redial(move |_, _, _| observed.set(observed.get() + 1));

    for (mobile, width, height) in [
        (true, 360.0, 640.0),
        (true, 412.0, 891.0),
        (false, 1400.0, 900.0),
    ] {
        w.set_active_tab("chats".into());
        settle(&w, width, height);
        w.set_is_mobile_platform(mobile);
        w.set_active_tab("calls".into());
        settle(&w, width, height);
        if !mobile {
            let card = labelled(&w, "Calls dialog").expect("Calls desktop card");
            let position = card.absolute_position();
            let size = card.size();
            assert!(
                (size.height - 800.0_f32.min(height - 96.0)).abs() < 1.0,
                "desktop call list must fill its capped viewport"
            );
            assert!(position.y >= 0.0 && position.y + size.height <= height);
        }
        let more = labelled(&w, "More options").expect("Calls header action");
        let p = more.absolute_position();
        let s = more.size();
        println!(
            "mobile={mobile} more x={} y={} w={} h={}",
            p.x, p.y, s.width, s.height
        );
        assert!(labelled(&w, "Clear call history").is_none());
        click(&w, &more);
        settle(&w, width, height);
        assert_eq!(
            w.get_active_tab().as_str(),
            "calls",
            "header event reached an underlying conversation row"
        );
        let menu = labelled(&w, "Clear call history").expect("header tap must open overflow");
        let mp = menu.absolute_position();
        let ms = menu.size();
        println!("menu x={} y={} w={} h={}", mp.x, mp.y, ms.width, ms.height);
        assert!(mp.y >= p.y + s.height - 0.5, "menu overlaps header action");
        assert!(
            (mp.x + ms.width - p.x - s.width).abs() < 1.0,
            "menu trailing edge must align with overflow, including desktop close button"
        );
        assert!(mp.x >= 0.0 && mp.y + ms.height <= height);
        // RC77/S9: the real header owns a local menu but Android Back is
        // coordinated at MainWindow. Exercise the actual callback/key route.
        assert!(w.get_calls_more_menu_visible());
        key(&w, slint::platform::Key::Back);
        settle(&w, width, height);
        assert!(!w.get_calls_more_menu_visible());
        assert!(labelled(&w, "Clear call history").is_none());
        assert_eq!(w.get_active_tab().as_str(), "calls");
        assert_eq!(row_actions.get(), 0, "Back leaked to a call row");
        assert!(!cleared.get());
        click(&w, &more);
        settle(&w, width, height);
        key(&w, slint::platform::Key::Escape);
        settle(&w, width, height);
        assert!(labelled(&w, "Clear call history").is_none());
        assert_eq!(w.get_active_tab().as_str(), "calls");

        // Preserve selection before menu, then menu before tab navigation.
        click(&w, &more);
        settle(&w, width, height);
        w.set_selection_mode(true);
        w.invoke_back_pressed();
        assert!(!w.get_selection_mode());
        assert!(w.get_calls_more_menu_visible());
        w.invoke_back_pressed();
        assert!(!w.get_calls_more_menu_visible());
        assert_eq!(w.get_active_tab().as_str(), "calls");
        w.invoke_back_pressed();
        assert_eq!(w.get_active_tab().as_str(), "chats");
        w.set_active_tab("calls".into());
        settle(&w, width, height);
        let more = labelled(&w, "More options").unwrap();
        click(&w, &more);
        settle(&w, width, height);

        // A body tap below the menu must dismiss its scrim rather than
        // reopening an underlying conversation row.
        click_position(
            &w,
            slint::LogicalPosition::new(mp.x + ms.width / 2.0, mp.y + ms.height + 24.0),
        );
        settle(&w, width, height);
        assert!(labelled(&w, "Clear call history").is_none());
        assert_eq!(w.get_active_tab().as_str(), "calls");
        click(&w, &more);
        settle(&w, width, height);
        assert!(labelled(&w, "Clear call history").is_some());

        // The same button closes it, then reopens it; no history mutation.
        click(&w, &more);
        settle(&w, width, height);
        assert!(labelled(&w, "Clear call history").is_none());
        click(&w, &more);
        settle(&w, width, height);
        click(&w, &labelled(&w, "Clear call history").unwrap());
        settle(&w, width, height);
        assert!(labelled(&w, "Clear call history").is_none());
        assert!(
            labelled(&w, "Cancel").is_some(),
            "menu only reveals confirmation"
        );
        assert!(
            !cleared.get(),
            "opening overflow/confirmation must not clear history"
        );
        let prompt = labelled(&w, "Delete every call in this list from this device? This cannot be undone.")
            .expect("confirmation prompt");
        let screen = testing::ElementHandle::find_by_element_type_name(&w, "CallHistoryScreen")
            .next()
            .unwrap();
        let mut list_bounds = None;
        screen.visit_descendants(|e| {
            if e.type_name().as_deref() == Some("Flickable") && e.size().height > 0.0 {
                list_bounds = Some((e.absolute_position(), e.size()));
            }
            std::ops::ControlFlow::<()>::Continue(())
        });
        let (list_position, list_size) = list_bounds.expect("real call list viewport");
        assert!(
            list_position.y + list_size.height <= prompt.absolute_position().y,
            "call rows paint through the confirmation: list bottom {} > prompt top {}",
            list_position.y + list_size.height,
            prompt.absolute_position().y,
        );
        // Probe text and empty space beside the buttons. With 24 real rows,
        // these coordinates reach live rows/redials unless the confirmation
        // reserves and shields its whole surface.
        let before = row_actions.get();
        click(&w, &prompt);
        let cancel = labelled(&w, "Cancel").unwrap();
        click_position(
            &w,
            slint::LogicalPosition::new(
                list_position.x + 2.0,
                cancel.absolute_position().y + cancel.size().height / 2.0,
            ),
        );
        assert_eq!(
            row_actions.get(),
            before,
            "confirmation leaked a row action"
        );
        assert!(labelled(&w, "Cancel").is_some());
        click(&w, &cancel);
        settle(&w, width, height);

        assert!(
            labelled(&w, "Cancel").is_none(),
            "Cancel dismisses confirmation"
        );
        assert!(!cleared.get(), "Cancel must never clear history");
        assert_eq!(row_actions.get(), before, "Cancel leaked a row action");

        // A stacked screen still pops before its underlying Calls tab.
        click(&w, &more);
        settle(&w, width, height);
        back::push(&w, "keyboard-shortcuts");
        settle(&w, width, height);
        assert!(!w.get_calls_more_menu_visible());
        w.invoke_back_pressed();
        assert_eq!(w.get_modal_stack().row_count(), 0);
        assert_eq!(w.get_active_tab().as_str(), "calls");

        // Mounted local state must also reset when the owner changes view or
        // conversation; reopening Calls must not inherit stale overflow.
        for change in 0..4 {
            let more = labelled(&w, "More options").unwrap();
            click(&w, &more);
            settle(&w, width, height);
            assert!(w.get_calls_more_menu_visible());
            match change {
                0 => w.set_channel_id(format!("fixture-{width}-{change}").into()),
                1 => w.set_conversation_kind(chatter_ui::ConversationKind::Channel),
                2 => w.set_current_view("loading".into()),
                _ => w.set_active_tab("chats".into()),
            }
            settle(&w, width, height);
            assert!(!w.get_calls_more_menu_visible());
            w.set_current_view("main".into());
            w.set_active_tab("calls".into());
            settle(&w, width, height);
            assert!(labelled(&w, "Clear call history").is_none());
            w.set_conversation_kind(chatter_ui::ConversationKind::ServerDm);
        }
        let more = labelled(&w, "More options").unwrap();

        if !mobile {
            click(&w, &more);
            settle(&w, width, height);
            assert!(labelled(&w, "Clear call history").is_some());
            // The shared modal backdrop must dismiss overflow before Calls.
            click_position(&w, slint::LogicalPosition::new(8.0, 8.0));
            settle(&w, width, height);
            assert!(labelled(&w, "Clear call history").is_none());
            assert_eq!(w.get_active_tab().as_str(), "calls");
            click_position(&w, slint::LogicalPosition::new(8.0, 8.0));
            settle(&w, width, height);
            assert_eq!(w.get_active_tab().as_str(), "chats");
        }
    }
}

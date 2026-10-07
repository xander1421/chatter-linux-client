//! RC71: exercise real pointer hit testing, including the floating list that
//! extends behind the Calls header. Run with --features ui-test-debug.
use chatter_ui::{CallHistoryRow, MainWindow};
use i_slint_backend_testing as testing;
use slint::platform::{PointerEventButton, WindowEvent};
use slint::{ComponentHandle, ModelRc, VecModel};

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

    for (mobile, width, height) in [(true, 412.0, 891.0), (false, 1400.0, 900.0)] {
        w.set_active_tab("chats".into());
        settle(&w, width, height);
        w.set_is_mobile_platform(mobile);
        w.set_active_tab("calls".into());
        settle(&w, width, height);
        if !mobile {
            let card = labelled(&w, "Modal dialog").expect("Calls desktop card");
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
        click(&w, &labelled(&w, "Cancel").unwrap());
        settle(&w, width, height);

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

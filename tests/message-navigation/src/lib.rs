slint::include_modules!();
#[cfg(test)]
mod tests {
    use super::*;
    use slint::{
        ComponentHandle, Model,
        platform::{
            Platform, WindowAdapter,
            software_renderer::{MinimalSoftwareWindow, RepaintBufferType},
        },
    };
    use std::{
        rc::Rc,
        time::{Duration, Instant},
    };
    struct Headless(Rc<MinimalSoftwareWindow>);
    impl Platform for Headless {
        fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
            Ok(self.0.clone())
        }
        fn duration_since_start(&self) -> Duration {
            START.with(|s| s.elapsed())
        }
    }
    thread_local! { static START: Instant = Instant::now(); }
    #[test]
    fn production_navigation_lifecycle() {
        let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        slint::platform::set_platform(Box::new(Headless(adapter.clone()))).unwrap();
        let w = Probe::new().unwrap();
        let rows = Rc::new(slint::VecModel::from(
            (0..120)
                .map(|i| MessageItemData {
                    message_id: format!("m{i}").into(),
                    content: format!("message {i}").into(),
                    ..Default::default()
                })
                .collect::<Vec<_>>(),
        ));
        w.set_messages(rows.clone().into());
        w.show().unwrap();
        adapter.set_size(slint::PhysicalSize::new(400, 300));
        let pointer_x = std::cell::Cell::new(1.0);
        let frame = || {
            slint::platform::update_timers_and_animations();
            i_slint_core::properties::ChangeTracker::run_change_handlers();
            let x = pointer_x.replace(3.0 - pointer_x.get());
            // Like the existing ui_unread_pill harness, run the window's
            // instantiation/change-handler loop between timer ticks.
            w.window()
                .dispatch_event(slint::platform::WindowEvent::PointerMoved {
                    position: slint::LogicalPosition::new(x, 1.0),
                });
            w.window().request_redraw();
            adapter.draw_if_needed(|r| {
                r.render(&mut vec![slint::Rgb8Pixel::default(); 400 * 300], 400);
            });
        };
        frame();
        w.invoke_seek(60);
        for _ in 0..80 {
            std::thread::sleep(Duration::from_millis(17));
            frame();
        }
        assert_eq!(w.get_target(), -1, "measured landing must finish");
        assert!(w.get_measured(), "landing must use actual target geometry");
        assert_eq!(
            w.get_stable(),
            3,
            "landing retains three stable measured passes"
        );
        assert!(!w.get_deadline(), "landed command stops its deadline timer");
        assert!(w.get_position() > 0., "distant seek must move viewport");
        w.invoke_seek(80);
        frame();
        w.window()
            .dispatch_event(slint::platform::WindowEvent::PointerScrolled {
                position: slint::LogicalPosition::new(200., 150.),
                delta_x: 0.,
                delta_y: 60.,
            });
        frame();
        assert_eq!(w.get_target(), -1, "real user wheel cancels navigation");
        w.invoke_seek(80);
        w.set_active(false);
        frame();
        assert_eq!(w.get_target(), -1, "inactive page cancels");
        w.set_active(true);
        w.invoke_seek(80);
        w.set_loading(true);
        frame();
        assert_eq!(w.get_target(), -1, "loading cancels");
        w.set_loading(false);
        frame();
        w.invoke_seek(80);
        rows.set_row_data(
            80,
            MessageItemData {
                message_id: "replacement".into(),
                ..Default::default()
            },
        );
        frame();
        assert_eq!(w.get_target(), -1, "same-count target replacement cancels");
        w.invoke_seek(80);
        frame();
        let replacement = Rc::new(slint::VecModel::from(
            (0..rows.row_count())
                .map(|i| rows.row_data(i).unwrap())
                .collect::<Vec<_>>(),
        ));
        w.set_messages(replacement.clone().into());
        frame();
        assert_eq!(
            w.get_target(),
            -1,
            "same-ID replacement model cancels old geometry"
        );
        w.invoke_seek(80);
        w.invoke_cancel();
        frame();
        assert_eq!(w.get_target(), -1);
        w.invoke_seek(999);
        frame();
        assert_eq!(w.get_target(), -1);
        // Opening on unread still uses the measured target, and a search
        // issued before loading completes overrides the opening anchor.
        w.set_loading(true);
        frame();
        // Install a fresh page in the same UI turn as its unread anchor.
        let replacement = Rc::new(slint::VecModel::from(
            (0..replacement.row_count())
                .map(|i| replacement.row_data(i).unwrap())
                .collect::<Vec<_>>(),
        ));
        w.set_messages(replacement.clone().into());
        w.set_unread(40);
        w.set_loading(false);
        frame();
        let unread_start = Instant::now();
        while w.get_target() >= 0 && unread_start.elapsed() < Duration::from_millis(5300) {
            std::thread::sleep(Duration::from_millis(17));
            frame();
        }
        assert_eq!(w.get_target(), -1, "unread landing");
        assert!(
            w.get_bubble_y().abs() < 2.,
            "unread bubble stays at its measured opening anchor: y={} stable={}",
            w.get_bubble_y(),
            w.get_stable()
        );
        assert!(
            !w.get_deadline(),
            "unread seek must stop work even if geometry keeps rebasing"
        );
        assert!(w.get_measured());
        assert_eq!(w.get_unread(), 40);
        assert!(
            !w.get_bottom(),
            "unread opening must not consume the divider"
        );
        w.set_loading(true);
        frame();
        w.invoke_seek(70);
        assert_eq!(w.get_target(), -1, "loading retains only intent");
        w.set_loading(false);
        frame();
        assert_eq!(w.get_target(), 70, "search intent survives loading");
        w.invoke_cancel();
        frame();
        w.set_bottom(true);
        frame();
        for _ in 0..12 {
            std::thread::sleep(Duration::from_millis(17));
            frame();
        }
        let tail_position = w.get_position();
        replacement.push(MessageItemData {
            message_id: "new-tail".into(),
            content: "incoming".into(),
            ..Default::default()
        });
        for _ in 0..12 {
            std::thread::sleep(Duration::from_millis(17));
            frame();
        }
        assert!(w.get_bottom());
        assert!(
            w.get_position() > tail_position,
            "tail follower tracks append"
        );
        w.invoke_seek(30);
        for _ in 0..100 {
            std::thread::sleep(Duration::from_millis(17));
            frame();
        }
        let history_position = w.get_position();
        replacement.push(MessageItemData {
            message_id: "another-tail".into(),
            content: "incoming".into(),
            ..Default::default()
        });
        for _ in 0..12 {
            std::thread::sleep(Duration::from_millis(17));
            frame();
        }
        assert!(!w.get_bottom());
        assert!(
            (w.get_position() - history_position).abs() < 2.,
            "history reader does not follow append"
        );
        // No render passes: the target cannot provide geometry, but its timer
        // must expire independently of animation/frame polling.
        w.invoke_seek(85);
        assert!(w.get_deadline());
        std::thread::sleep(Duration::from_millis(3100));
        slint::platform::update_timers_and_animations();
        w.invoke_seek(90);
        std::thread::sleep(Duration::from_millis(2100));
        slint::platform::update_timers_and_animations();
        assert_eq!(
            w.get_target(),
            90,
            "old command deadline cannot expire a replacement"
        );
        std::thread::sleep(Duration::from_millis(3100));
        slint::platform::update_timers_and_animations();
        frame();
        assert_eq!(w.get_target(), -1, "unmeasurable target deadline");
        assert!(!w.get_deadline());
    }
}

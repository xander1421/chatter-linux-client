//! Runs the Android queue fence and IME geometry publisher against the real phone UI.
#[path = "../../../vendor/i-slint-backend-android-activity/inset_dispatch.rs"]
mod inset_dispatch;

use chatter_ui::{ConversationKind, MainWindow, PendingAttachmentData};
use i_slint_backend_testing as testing;
use slint::{ComponentHandle, Model};

fn settle(_w: &MainWindow) {
    i_slint_core::properties::ChangeTracker::run_change_handlers();
    slint::Timer::single_shot(std::time::Duration::from_millis(40), || {
        slint::quit_event_loop().unwrap();
    });
    slint::run_event_loop().unwrap();
}

fn publish(w: &MainWindow, size: [f32; 2], ime: f32, bars: f32) {
    use i_slint_core::api::{PhysicalPosition, PhysicalSize};
    use i_slint_core::lengths::PhysicalEdges;
    inset_dispatch::publish_window_insets(
        w.window(),
        true,
        PhysicalPosition::new(0, 0),
        PhysicalSize::new(size[0] as u32, size[1] as u32),
        PhysicalEdges::new(24, bars as i32, 0, 0),
        PhysicalEdges::new(0, ime as i32, 0, 0),
    );
}

fn composer_bottom(w: &MainWindow) -> f32 {
    let field = testing::ElementHandle::find_by_element_type_name(w, "MessageComposer")
        .find(|e| {
            e.type_name().is_some_and(|name| name == "MessageComposer") && e.size().height > 0.0
        })
        .expect("visible composer");
    field.absolute_position().y + field.size().height
}
fn visible_type(w: &MainWindow, name: &str) -> bool {
    testing::ElementHandle::find_by_element_type_name(w, name)
        .any(|e| e.size().height > 0.0 && e.size().width > 0.0)
}
fn draft(w: &MainWindow) {
    assert_eq!(
        w.global::<chatter_ui::ChatPane>()
            .get_composer_text()
            .as_str(),
        "staged 😀 draft"
    );
    assert_eq!(
        w.global::<chatter_ui::ChatPane>()
            .get_pending_attachments()
            .row_count(),
        1
    );
    assert_eq!(
        w.global::<chatter_ui::ChatPane>()
            .get_pending_attachments()
            .row_data(0)
            .unwrap()
            .id
            .as_str(),
        "staged-saf"
    );
    assert!(
        !w.global::<chatter_ui::ChatPane>()
            .get_composer_emoji_picker_visible()
            && !w
                .global::<chatter_ui::ChatPane>()
                .get_composer_gif_picker_visible()
    );
}

#[test]
fn authoritative_return_repairs_phone_geometry_without_erasing_staged_draft() {
    assert!(chatter_ui::SLINT_DEBUG_INFO_ENABLED);
    testing::init_integration_test_with_system_time();
    let w = MainWindow::new().unwrap();
    w.set_current_view("main".into());
    w.global::<chatter_ui::SidebarPane>()
        .set_active_tab("chats".into());
    w.set_is_mobile_platform(true);
    w.global::<chatter_ui::ChatPane>()
        .set_conversation_kind(ConversationKind::ServerDm);
    w.global::<chatter_ui::ChatPane>()
        .set_channel_name("Fixture".into());
    w.global::<chatter_ui::SidebarPane>()
        .set_mobile_show_chat(true);
    w.global::<chatter_ui::ChatPane>()
        .set_composer_text("staged 😀 draft".into());
    w.show().unwrap();
    let fence = inset_dispatch::InsetDispatchFence::new();
    let mut sequence = 0;
    for size in [[432.0, 936.0], [412.0, 891.0], [360.0, 780.0]] {
        let next = |sequence: &mut u64| {
            *sequence += 1;
            *sequence
        };
        publish(&w, size, 0.0, 24.0);
        settle(&w);
        let bare_bottom = composer_bottom(&w);
        // Actual rendered composer includes only the intended mobile thumb spacer below it.
        assert!(
            (bare_bottom - (size[1] - 24.0 - 8.0)).abs() < 1.0,
            "unexpected bottom anchor: {bare_bottom} at {size:?}"
        );
        w.invoke_prepare_attachment_picker();
        w.window()
            .dispatch_event(slint::platform::WindowEvent::WindowActiveChanged(false));
        w.global::<chatter_ui::ChatPane>()
            .set_pending_attachments(slint::ModelRc::new(slint::VecModel::from(vec![
                PendingAttachmentData {
                    id: "staged-saf".into(),
                    filename: "staged.pdf".into(),
                    category: "file".into(),
                    is_ready: true,
                    ..Default::default()
                },
            ])));
        // The interrupted hide leaves a positive property; no final zero is injected here.
        publish(&w, size, 176.0, 24.0);
        settle(&w);
        assert!((composer_bottom(&w) - (bare_bottom - 176.0)).abs() < 1.0);
        let old_frame = next(&mut sequence);
        fence.receive(old_frame);
        w.global::<chatter_ui::SidebarPane>()
            .set_mobile_show_chat(false); // immediate Back to Chats before completion
        w.invoke_finish_attachment_picker(false);
        settle(&w);
        assert!(!visible_type(&w, "MobileBottomNav"));
        // Return supplies current root bounds + hidden IME; shared JNI fence discards old queued frame.
        let current = next(&mut sequence);
        fence.receive(current);
        w.window()
            .dispatch_event(slint::platform::WindowEvent::WindowActiveChanged(true));
        assert!(fence.apply(current, || publish(&w, size, 0.0, 24.0)));
        assert!(!fence.apply(old_frame, || publish(&w, size, 176.0, 24.0)));
        settle(&w);
        assert!(visible_type(&w, "MobileBottomNav"));
        assert!(
            testing::ElementHandle::find_by_accessible_label(&w, "Create")
                .any(|e| e.size().height > 0.0)
        );
        w.global::<chatter_ui::SidebarPane>()
            .set_mobile_show_chat(true);
        settle(&w);
        assert!(
            (composer_bottom(&w) - bare_bottom).abs() < 1.0,
            "attachment altered bottom anchor"
        );
        draft(&w);
        // Independent stale viewport case: IME is already zero, but root is reduced.
        publish(&w, [size[0], size[1] - 176.0], 0.0, 24.0);
        settle(&w);
        assert!((composer_bottom(&w) - (bare_bottom - 176.0)).abs() < 1.0);
        // Legacy snapshots must not override the existing native viewport.
        inset_dispatch::publish_window_insets(
            w.window(),
            false,
            i_slint_core::api::PhysicalPosition::new(0, 0),
            i_slint_core::api::PhysicalSize::new(size[0] as u32, size[1] as u32),
            i_slint_core::lengths::PhysicalEdges::new(24, 24, 0, 0),
            i_slint_core::lengths::PhysicalEdges::default(),
        );
        settle(&w);
        assert!((composer_bottom(&w) - (bare_bottom - 176.0)).abs() < 1.0);
        let current = next(&mut sequence);
        fence.receive(current);
        fence.apply(current, || publish(&w, size, 0.0, 24.0));
        settle(&w);
        assert!((composer_bottom(&w) - bare_bottom).abs() < 1.0);
        // A visible returned IME is preserved, rather than unconditionally cleared.
        let current = next(&mut sequence);
        fence.receive(current);
        fence.apply(current, || publish(&w, size, 220.0, 24.0));
        settle(&w);
        assert!((composer_bottom(&w) - (bare_bottom - 220.0)).abs() < 1.0);
        draft(&w);
        publish(&w, size, 0.0, 24.0);
        w.global::<chatter_ui::ChatPane>()
            .set_pending_attachments(slint::ModelRc::new(slint::VecModel::from(Vec::<
                PendingAttachmentData,
            >::new(
            ))));
        settle(&w);
    }
    // All four IME edges and physical scaling use the production conversion.
    assert_eq!(
        inset_dispatch::keyboard_rect([0.0, 0.0], [864.0, 1872.0], [0.0, 440.0, 0.0, 0.0], 2.0),
        ([0.0, 716.0], [432.0, 220.0])
    );
    assert_eq!(
        inset_dispatch::keyboard_rect([0.0, 0.0], [432.0, 936.0], [50.0, 0.0, 0.0, 0.0], 1.0),
        ([0.0, 0.0], [432.0, 50.0])
    );
    assert_eq!(
        inset_dispatch::keyboard_rect([0.0, 0.0], [432.0, 936.0], [0.0, 0.0, 50.0, 0.0], 1.0),
        ([0.0, 0.0], [50.0, 936.0])
    );
    assert_eq!(
        inset_dispatch::keyboard_rect([0.0, 0.0], [432.0, 936.0], [0.0, 0.0, 0.0, 50.0], 1.0),
        ([382.0, 0.0], [50.0, 936.0])
    );
}

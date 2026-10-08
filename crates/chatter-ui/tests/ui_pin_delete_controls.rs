//! RC82: one selection trash action, scope eligibility and explicit confirmation.
use chatter_ui::{
    Appearance, ConversationKind, MainWindow, MessageMenuActions, PinnedItem, SelectionChrome,
};
use i_slint_backend_testing as testing;
use slint::{
    ComponentHandle, ModelRc, VecModel,
    platform::{PointerEventButton, WindowEvent},
};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

fn settle(w: &MainWindow) {
    w.window().set_size(slint::LogicalSize::new(412.0, 891.0));
    w.show().unwrap();
    slint::Timer::single_shot(std::time::Duration::from_millis(50), || {
        slint::quit_event_loop().unwrap()
    });
    slint::run_event_loop().unwrap();
}
fn visible(w: &MainWindow, label: &str) -> Option<testing::ElementHandle> {
    testing::ElementHandle::find_by_accessible_label(w, label)
        .find(|e| e.size().width > 0.0 && e.size().height > 0.0)
}
fn click(w: &MainWindow, label: &str) {
    let e = visible(w, label).unwrap_or_else(|| panic!("missing {label}"));
    let p = e.absolute_position();
    let s = e.size();
    let position = slint::LogicalPosition::new(p.x + s.width / 2.0, p.y + s.height / 2.0);
    let button = PointerEventButton::Left;
    w.window()
        .dispatch_event(WindowEvent::PointerMoved { position });
    w.window()
        .dispatch_event(WindowEvent::PointerPressed { position, button });
    w.window()
        .dispatch_event(WindowEvent::PointerReleased { position, button });
    settle(w);
}
#[test]
fn selection_scope_requires_eligibility_and_confirmation_in_both_themes() {
    assert!(chatter_ui::SLINT_DEBUG_INFO_ENABLED);
    testing::init_integration_test_with_system_time();
    let w = MainWindow::new().unwrap();
    w.set_current_view("main".into());
    w.set_is_mobile_platform(true);
    w.set_mobile_show_chat(true);
    w.set_conversation_kind(ConversationKind::ServerDm);
    w.set_channel_name("Owned pin/delete fixture".into());
    let weak = w.as_weak();
    w.on_back_pressed(move || {
        if let Some(w) = weak.upgrade() {
            chatter_ui::bin_ui::features::back::handle_back(&w);
        }
    });
    let local = Rc::new(Cell::new(0));
    let both = Rc::new(Cell::new(0));
    let l = local.clone();
    w.on_selection_delete_clicked(move || l.set(l.get() + 1));
    let b = both.clone();
    w.global::<SelectionChrome>()
        .on_delete_everyone(move || b.set(b.get() + 1));
    let thumbnail =
        slint::Image::from_rgb8(slint::SharedPixelBuffer::<slint::Rgb8Pixel>::new(120, 60));
    w.set_has_pinned_messages(true);
    w.set_pinned_thumbnail(thumbnail.clone());
    w.set_pinned_message_preview("A pinned landscape image".into());
    w.set_pinned_items(ModelRc::new(VecModel::from(vec![PinnedItem {
        message_id: "pin".into(),
        preview: "A long pinned message\nwith multiple lines\nto make a taller row".into(),
        thumbnail,
    }])));
    for dark in [false, true] {
        w.global::<Appearance>()
            .set_theme(if dark { "dark" } else { "light" }.into());
        w.set_selection_mode(false);
        settle(&w);
        let bar = testing::ElementHandle::find_by_element_type_name(&w, "PinnedBar")
            .next()
            .unwrap();
        let image = visible(&w, "Pinned message image").unwrap();
        assert!(
            (image.absolute_position().y + image.size().height / 2.0
                - bar.absolute_position().y
                - bar.size().height / 2.0)
                .abs()
                < 1.0,
            "banner thumbnail must be vertically centered"
        );
        assert_eq!(image.size().width, 36.0);
        assert_eq!(image.size().height, 36.0);
        chatter_ui::bin_ui::features::back::push(&w, "pinned-messages");
        settle(&w);
        let images: Vec<_> =
            testing::ElementHandle::find_by_accessible_label(&w, "Pinned message image")
                .filter(|e| e.size().width == 40.0)
                .collect();
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].size().height, 40.0);
        let row = visible(
            &w,
            "A long pinned message\nwith multiple lines\nto make a taller row",
        )
        .unwrap();
        assert!(
            (images[0].absolute_position().y + 20.0
                - row.absolute_position().y
                - row.size().height / 2.0)
                .abs()
                < 1.0,
            "rich pin thumbnail must be vertically centered"
        );
        chatter_ui::bin_ui::features::back::close(&w, "pinned-messages");
        settle(&w);
        for eligible in [false, true] {
            w.set_selection_mode(true);
            w.set_selected_message_count(2);
            w.global::<SelectionChrome>()
                .set_can_delete_everyone(eligible);
            settle(&w);
            assert!(visible(&w, "Delete selected for everyone").is_none());
            assert!(visible(&w, "Delete selected for me").is_none());
            click(&w, "Delete selected");
            assert_eq!(visible(&w, "Delete for both").is_some(), eligible);
            let old = (local.get(), both.get());
            click(
                &w,
                if eligible {
                    "Delete for both"
                } else {
                    "Delete for me"
                },
            );
            assert_eq!(
                (local.get(), both.get()),
                old,
                "scope choice must not delete"
            );
            click(&w, "Cancel");
            assert_eq!((local.get(), both.get()), old, "cancel must not delete");
            click(&w, "Delete selected");
            let label = if eligible {
                "Delete for both"
            } else {
                "Delete for me"
            };
            click(&w, label);
            click(&w, label);
            assert_eq!(
                (local.get(), both.get()),
                if eligible {
                    (old.0, old.1 + 1)
                } else {
                    (old.0 + 1, old.1)
                }
            );
            assert_eq!(w.get_delete_confirm_count(), 0);
        }
    }
    w.set_selection_mode(false);
    let single = Rc::new(Cell::new(0));
    let recorded = single.clone();
    w.on_delete_message(move |_, _| recorded.set(recorded.get() + 1));
    for remote in [false, true] {
        let flags = w.global::<MessageMenuActions>();
        flags.set_delete_for_me(true);
        flags.set_delete_for_everyone(remote);
        w.set_context_menu_sender_id("author".into());
        w.set_context_menu_timestamp_ns("123".into());
        w.set_context_menu_visible(true);
        settle(&w);
        click(&w, "More");
        click(&w, "Delete");
        assert_eq!(visible(&w, "Delete for both").is_some(), remote);
        assert!(chatter_ui::bin_ui::features::back::handle_back(&w));
        settle(&w);
        assert_eq!(w.get_delete_confirm_count(), 0);
        assert_eq!(single.get(), 0, "Back cancels the scope choice");
    }

    // Every remote label must name the true destructive audience. Use real
    // pointer clicks through both stages and inspect the final target payload.
    let remote_target = Rc::new(RefCell::new(Vec::<(String, String)>::new()));
    let capture = remote_target.clone();
    w.on_delete_everyone_message(move |author, stamp| {
        capture
            .borrow_mut()
            .push((author.to_string(), stamp.to_string()));
    });
    let local_target = Rc::new(RefCell::new(Vec::<(String, String)>::new()));
    let capture = local_target.clone();
    w.on_delete_message(move |author, stamp| {
        capture
            .borrow_mut()
            .push((author.to_string(), stamp.to_string()));
    });
    for (kind, label) in [
        (ConversationKind::ServerDm, "Delete for both"),
        (ConversationKind::AnonDm, "Delete for both"),
        (ConversationKind::Channel, "Delete for everyone"),
        (ConversationKind::AnonGroup, "Delete for everyone"),
    ] {
        w.set_conversation_kind(kind);
        let flags = w.global::<MessageMenuActions>();
        flags.set_delete_for_me(true);
        flags.set_delete_for_everyone(true);
        for remote in [false, true] {
            w.set_context_menu_sender_id("exact-author".into());
            w.set_context_menu_timestamp_ns("987654321".into());
            w.set_context_menu_visible(true);
            settle(&w);
            click(&w, "More");
            click(&w, "Delete");
            assert!(visible(&w, label).is_some());
            let wrong = if label == "Delete for both" {
                "Delete for everyone"
            } else {
                "Delete for both"
            };
            assert!(visible(&w, wrong).is_none());
            let before = (remote_target.borrow().len(), local_target.borrow().len());
            let choice = if remote { label } else { "Delete for me" };
            click(&w, choice);
            assert_eq!(
                (remote_target.borrow().len(), local_target.borrow().len()),
                before
            );
            click(&w, choice);
            let target = if remote {
                remote_target.borrow()
            } else {
                local_target.borrow()
            };
            assert_eq!(
                target.last(),
                Some(&("exact-author".into(), "987654321".into()))
            );
            assert_eq!(
                (remote_target.borrow().len(), local_target.borrow().len()),
                if remote {
                    (before.0 + 1, before.1)
                } else {
                    (before.0, before.1 + 1)
                }
            );
        }
        // Actual Escape dispatch must dismiss either stage without deletion.
        for confirm in [false, true] {
            w.set_context_menu_visible(true);
            settle(&w);
            click(&w, "More");
            click(&w, "Delete");
            if confirm {
                click(&w, label);
            }
            let before = (remote_target.borrow().len(), local_target.borrow().len());
            let text: slint::SharedString = slint::platform::Key::Escape.into();
            w.window()
                .dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
            w.window().dispatch_event(WindowEvent::KeyReleased { text });
            settle(&w);
            assert_eq!(
                w.get_delete_confirm_count(),
                0,
                "Escape cancels either delete stage"
            );
            assert_eq!(
                (remote_target.borrow().len(), local_target.borrow().len()),
                before
            );
        }
    }
    w.set_conversation_kind(ConversationKind::ServerDm);

    // The real wire must refuse a stale eligibility snapshot when a selected
    // row disappeared before confirmation, without deleting another subset.
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let _entered = runtime.enter();
    let fixture = tempfile::tempdir().unwrap();
    let client = chatter_core::client::ChatterClient::builder()
        .data_dir(fixture.path().to_string_lossy().to_string())
        .build()
        .unwrap();
    chatter_ui::bin_ui::features::chat::wire(&w, &client);
    w.set_messages(ModelRc::new(VecModel::from(vec![
        chatter_ui::MessageItemData {
            message_id: "removed-call-message".into(),
            ..Default::default()
        },
    ])));
    w.invoke_message_selection_toggled("removed-call-message".into());
    w.set_messages(ModelRc::new(
        VecModel::<chatter_ui::MessageItemData>::default(),
    ));
    w.global::<SelectionChrome>().set_can_delete_everyone(true);
    let sent = Rc::new(Cell::new(0));
    let observed = sent.clone();
    w.on_delete_everyone_message(move |_, _| observed.set(observed.get() + 1));
    click(&w, "Delete selected");
    click(&w, "Delete for both");
    click(&w, "Delete for both");
    assert_eq!(sent.get(), 0);
    assert!(!w.global::<SelectionChrome>().get_can_delete_everyone());
    assert!(
        w.get_selection_mode(),
        "rejected deletion keeps the selection"
    );
    assert!(w.get_toast_message().contains("can no longer"));
}

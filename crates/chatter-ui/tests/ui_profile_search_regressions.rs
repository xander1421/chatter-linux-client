//! Owned profile/search boundaries; no real browser, network lookup or private DB.
#![cfg(feature = "ui-test-debug")]
use std::cell::RefCell;
use std::rc::Rc;

use chatter_ui::{MainWindow, SearchResult, SharedContentItem};
use i_slint_backend_testing as testing;
use slint::{ComponentHandle, Model, ModelRc, VecModel};

#[test]
fn selected_second_url_and_search_clear_use_production_callbacks() {
    // One integration test in this process owns the testing backend.
    testing::init_integration_test_with_system_time();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    let fixture = tempfile::tempdir().unwrap();
    // Off the real keyring: the marker must exist before the client opens.
    let db_path = chatter_core::system::Directories::with_base_dir(fixture.path().to_path_buf())
        .unwrap()
        .database_path();
    std::fs::write(
        db_path
            .parent()
            .unwrap()
            .join(chatter_core::system::keyring::NO_KEYRING_MARKER),
        b"",
    )
    .unwrap();
    let client = chatter_core::client::ChatterClient::builder()
        .data_dir(fixture.path().to_string_lossy().into_owned())
        .build()
        .unwrap();
    let w = MainWindow::new().unwrap();
    chatter_ui::bin_ui::features::modals::profile::wire_profile_actions(&w, &client);
    let opened = Rc::new(RefCell::new(Vec::<String>::new()));
    let captured = opened.clone();
    // Replace the platform action, leaving production profile callback intact.
    w.global::<chatter_ui::ChatPane>()
        .on_open_message_link(move |url| captured.borrow_mut().push(url.to_string()));
    let urls = [
        "https://example.invalid/first",
        "https://example.invalid/second",
    ];
    w.global::<chatter_ui::ModalHostState>()
        .set_profile_shared_items(ModelRc::new(VecModel::from(
            urls.iter()
                .map(|url| SharedContentItem {
                    message_id: "two-url-message".into(),
                    category: "link".into(),
                    subtitle: (*url).into(),
                    ..Default::default()
                })
                .collect::<Vec<_>>(),
        )));
    w.global::<chatter_ui::ModalHostState>()
        .invoke_profile_open_shared_item(
            "two-url-message".into(),
            "".into(),
            "link".into(),
            urls[1].into(),
        );
    assert_eq!(*opened.borrow(), [urls[1]]);
    w.global::<chatter_ui::ModalHostState>()
        .invoke_profile_open_shared_item(
            "two-url-message".into(),
            "".into(),
            "link".into(),
            "https://example.invalid/stale".into(),
        );
    assert_eq!(
        opened.borrow().len(),
        1,
        "an obsolete row must not invoke the opener"
    );

    chatter_ui::bin_ui::features::unified_search::wire(&w, &client);
    w.global::<chatter_ui::SearchPane>()
        .set_unified_search_open(true);
    w.global::<chatter_ui::SearchPane>()
        .invoke_unified_search_opened();
    w.global::<chatter_ui::SearchPane>()
        .set_unified_search_person_status("found".into());
    w.global::<chatter_ui::SearchPane>()
        .set_unified_search_person_id("stale-person".into());
    w.global::<chatter_ui::SearchPane>()
        .set_unified_search_person_name("stale name".into());
    w.global::<chatter_ui::SearchPane>()
        .set_unified_search_person_handle("stale.handle".into());
    w.global::<chatter_ui::SearchPane>()
        .set_unified_search_searching(true);
    w.global::<chatter_ui::SearchPane>()
        .set_unified_search_messages(ModelRc::new(VecModel::from(vec![SearchResult {
            message_id: "stale-message".into(),
            ..Default::default()
        }])));
    w.global::<chatter_ui::SearchPane>()
        .invoke_unified_search_changed("".into());
    assert!(
        !w.global::<chatter_ui::SearchPane>()
            .get_unified_search_searching()
    );
    assert_eq!(
        w.global::<chatter_ui::SearchPane>()
            .get_unified_search_messages()
            .row_count(),
        0
    );
    assert!(
        w.global::<chatter_ui::SearchPane>()
            .get_unified_search_person_id()
            .is_empty()
    );
    assert!(
        w.global::<chatter_ui::SearchPane>()
            .get_unified_search_person_status()
            .is_empty()
    );
    let navigated = Rc::new(RefCell::new(0));
    let navigation_count = navigated.clone();
    w.global::<chatter_ui::SidebarPane>()
        .on_channel_selected(move |_| *navigation_count.borrow_mut() += 1);
    let modal_before = w.get_active_modal();
    w.global::<chatter_ui::SearchPane>()
        .invoke_unified_search_person_selected("11111111-1111-4111-8111-111111111111".into());
    assert_eq!(
        w.get_active_modal(),
        modal_before,
        "clear cannot open an obsolete person"
    );
    w.global::<chatter_ui::SearchPane>()
        .invoke_unified_search_message_selected("stale-message".into());
    w.global::<chatter_ui::SearchPane>()
        .invoke_unified_search_conversation_selected("stale-channel".into(), "channel".into());
    assert_eq!(*navigated.borrow(), 0, "clear leaves no selectable old row");
    w.global::<chatter_ui::SearchPane>()
        .set_unified_search_person_status("searching".into());
    w.global::<chatter_ui::SearchPane>()
        .set_unified_search_searching(true);
    w.global::<chatter_ui::SearchPane>()
        .set_unified_search_open(false);
    w.global::<chatter_ui::SearchPane>()
        .invoke_unified_search_closed();
    assert!(
        !w.global::<chatter_ui::SearchPane>()
            .get_unified_search_searching()
    );
    assert!(
        w.global::<chatter_ui::SearchPane>()
            .get_unified_search_person_status()
            .is_empty()
    );

    // Empty-query hint shares the input origin on phone and desktop, including
    // while the input owns focus. Keep this in the one backend-owning test.
    w.set_current_view("main".into());
    w.global::<chatter_ui::SearchPane>()
        .set_unified_search_query("".into());
    w.global::<chatter_ui::SearchPane>()
        .set_unified_search_open(true);
    w.global::<chatter_ui::SearchPane>()
        .invoke_unified_search_opened();
    for width in [360.0, 1024.0] {
        w.window().set_size(slint::LogicalSize::new(width, 700.0));
        w.show().unwrap();
        slint::Timer::single_shot(std::time::Duration::from_millis(20), || {
            slint::quit_event_loop().unwrap();
        });
        w.run().unwrap();
        let input = testing::ElementHandle::find_by_element_id(&w, "UnifiedSearch::search-input")
            .next()
            .expect("search input is rendered");
        let placeholder =
            testing::ElementHandle::find_by_element_id(&w, "UnifiedSearch::search-placeholder")
                .next()
                .expect("focused empty field keeps its hint");
        assert!(input.size().width > 100.0);
        assert!(
            (placeholder.absolute_position().x - input.absolute_position().x).abs() < 1.0,
            "placeholder must start where typed text starts at width {width}"
        );
        assert!((placeholder.size().width - input.size().width).abs() < 1.0);
    }

    // Check that both Slint row paths forward the URL discriminator and the
    // root binding preserves it; production Rust must use the safe shared action.
    let modal = include_str!("../../../ui/modals/user_profile.slint");
    assert_eq!(modal.matches("root.open-shared-item(item.message-id, item.attachment-id, item.category, item.subtitle)").count(), 2);
    let app = include_str!("../../../ui/app_modal_host.slint");
    assert!(app.contains(
        "ModalHostState.profile-open-shared-item(message-id, attachment-id, category, selected-url)"
    ));
    let profile = include_str!("../src/bin_ui/features/modals/profile.rs");
    assert!(
        profile.contains("w.global::<crate::ChatPane>()")
            && profile.contains(".invoke_open_message_link(url.into())")
    );
    assert!(!profile.contains("ExternalOpener"));
}

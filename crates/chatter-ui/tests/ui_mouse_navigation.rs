//! Drive the callback used by the desktop window's side-button event filter.
use chatter_ui::bin_ui::features::back;
use chatter_ui::{AppLockPane, ChatPane, MainWindow, SettingsPane, SidebarPane};
use slint::ComponentHandle;

fn settle() {
    i_slint_core::properties::ChangeTracker::run_change_handlers();
    slint::Timer::single_shot(std::time::Duration::from_millis(20), || {
        slint::quit_event_loop().unwrap();
    });
    slint::run_event_loop().unwrap();
}

#[test]
fn side_buttons_revisit_menus_and_settings_but_do_not_replay_forms() {
    i_slint_backend_testing::init_integration_test_with_system_time();
    let w = MainWindow::new().unwrap();
    w.set_current_view("main".into());
    w.set_is_mobile_platform(false);
    w.show().unwrap();
    back::wire(&w);
    settle();
    let sidebar = w.global::<SidebarPane>();
    sidebar.set_active_tab("contacts".into());
    settle();
    sidebar.set_active_tab("calls".into());
    settle();
    w.invoke_mouse_navigation(false);
    settle();
    assert_eq!(sidebar.get_active_tab(), "contacts");
    w.invoke_mouse_navigation(true);
    settle();
    assert_eq!(sidebar.get_active_tab(), "calls");

    back::push(&w, "settings");
    settle();
    let settings = w.global::<SettingsPane>();
    settings.set_navigation_section("appearance".into());
    settle();
    settings.set_navigation_section("privacy".into());
    settle();
    w.invoke_mouse_navigation(false);
    settle();
    assert_eq!(w.get_active_modal(), "settings");
    assert_eq!(settings.get_navigation_section(), "appearance");
    w.invoke_mouse_navigation(true);
    settle();
    assert_eq!(settings.get_navigation_section(), "privacy");

    back::push(&w, "sessions");
    settle();
    w.invoke_mouse_navigation(false);
    settle();
    assert_eq!(w.get_active_modal(), "settings");
    w.invoke_mouse_navigation(true);
    settle();
    assert_eq!(w.get_active_modal(), "sessions");
    w.invoke_mouse_navigation(false);
    settle();

    // A popup dismisses before its owning menu navigates.
    w.global::<ChatPane>().set_reaction_picker_visible(true);
    w.invoke_mouse_navigation(false);
    settle();
    assert!(!w.global::<ChatPane>().get_reaction_picker_visible());
    assert_eq!(settings.get_navigation_section(), "privacy");

    w.invoke_mouse_navigation(false);
    settle();
    settings.set_navigation_section("notifications".into());
    settle();
    w.invoke_mouse_navigation(true);
    settle();
    assert_eq!(settings.get_navigation_section(), "notifications");

    back::push(&w, "create-channel");
    settle();
    w.invoke_mouse_navigation(false);
    settle();
    assert_eq!(w.get_active_modal(), "settings");
    w.invoke_mouse_navigation(true);
    settle();
    assert_eq!(w.get_active_modal(), "settings");

    // Logout/lock clears destinations from the preceding session.
    w.global::<AppLockPane>().set_locked(true);
    settle();
    w.global::<AppLockPane>().set_locked(false);
    settle();
    w.invoke_mouse_navigation(true);
    settle();
    assert_eq!(w.get_active_modal(), "settings");
    w.invoke_mouse_navigation(false);
    settle();
    assert_eq!(w.get_active_modal(), "none");
    w.invoke_mouse_navigation(false);
    settle();
    assert_eq!(w.get_active_modal(), "none");
    w.invoke_mouse_navigation(true);
    settle();
    assert_eq!(w.get_active_modal(), "none");
    assert_eq!(sidebar.get_active_tab(), "calls");
    w.invoke_mouse_navigation(true);
    settle();
    assert_eq!(w.get_active_modal(), "settings");
}

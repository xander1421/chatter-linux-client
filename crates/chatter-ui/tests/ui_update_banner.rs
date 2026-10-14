//! The signed updater's visible states stay reachable in the sidebar.
use chatter_ui::{MainWindow, UpdateGlobal};
use i_slint_backend_testing::{self as testing, ElementHandle};
use slint::ComponentHandle;

fn pump(window: &MainWindow) {
    slint::Timer::single_shot(std::time::Duration::from_millis(30), || {
        slint::quit_event_loop().unwrap();
    });
    window.run().unwrap();
}

fn visible(window: &MainWindow, label: &str) -> bool {
    ElementHandle::find_by_accessible_label(window, label)
        .any(|element| element.size().width > 0.0 && element.size().height > 0.0)
}

fn bounds(window: &MainWindow, label: &str) -> (f32, f32) {
    let element = ElementHandle::find_by_accessible_label(window, label)
        .next()
        .unwrap_or_else(|| panic!("{label} is absent"));
    (element.absolute_position().y, element.size().height)
}

#[test]
fn update_banner_changes_action_across_download_and_restart() {
    testing::init_integration_test_with_system_time();
    let window = MainWindow::new().unwrap();
    window.set_current_view("main".into());
    window
        .window()
        .set_size(slint::LogicalSize::new(1100.0, 800.0));
    window.show().unwrap();
    let update = window.global::<UpdateGlobal>();
    update.set_version("0.1.0-rc122".into());
    update.set_state("available".into());
    pump(&window);
    assert!(visible(&window, "Update available"));
    assert!(visible(&window, "Update"));

    update.set_state("downloading".into());
    update.set_progress("Downloading and verifying…".into());
    pump(&window);
    assert!(visible(&window, "Downloading and verifying…"));

    update.set_state("ready".into());
    pump(&window);
    assert!(visible(&window, "Ready to restart"));
    assert!(visible(&window, "Restart"));

    update.set_state("available".into());
    update.set_play_distribution(true);
    pump(&window);
    assert!(visible(&window, "Update in Play Store"));

    update.set_state("error".into());
    update.set_error("Invalid update signature".into());
    pump(&window);
    assert!(visible(&window, "Update failed"));
    assert!(visible(&window, "Retry"));

    // The phone dock sits at the foot of a 740px window. Both the update
    // heading and its action must remain above it in every updater state.
    window.set_is_mobile_platform(true);
    window.window().set_size(slint::LogicalSize::new(360.0, 740.0));
    pump(&window);
    for label in ["Update failed", "Retry"] {
        let (top, height) = bounds(&window, label);
        assert!(top >= 0.0 && top + height < 676.0, "{label} behind the dock: {top}+{height}");
    }
    update.set_state("available".into());
    update.set_play_distribution(false);
    pump(&window);
    for label in ["Update available", "Update"] {
        let (top, height) = bounds(&window, label);
        assert!(top >= 0.0 && top + height < 676.0, "{label} behind the dock: {top}+{height}");
    }
}

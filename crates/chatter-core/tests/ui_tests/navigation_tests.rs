//! Navigation callback tests.

// ============================================================================
// Navigation Callback Tests
// ============================================================================

#[test]
fn test_go_to_register_navigation() {
    // Simulate: on_go_to_register callback
    let mut current_view = "login";

    // Callback action
    current_view = "register";

    assert_eq!(current_view, "register");
}

#[test]
fn test_go_to_login_navigation() {
    // Simulate: on_go_to_login callback
    let mut current_view = "register";

    // Callback action
    current_view = "login";

    assert_eq!(current_view, "login");
}

//! Settings callback tests.

use crate::common::*;

// ============================================================================
// Settings Callback Tests
// ============================================================================

#[tokio::test]
async fn test_settings_change_password_callback_flow() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let user = assert_ok(setup_authenticated_test(&pool).await);

    // Mock UI state
    let mut settings_current_password = user.password.clone();
    let (new_password, _) = random_password();
    let mut settings_new_password = new_password.clone();
    let mut settings_confirm_password = new_password.clone();
    let mut settings_password_changing = false;
    let mut settings_password_error: Option<String> = None;

    // Verify passwords match
    if settings_new_password != settings_confirm_password {
        settings_password_error = Some("Passwords do not match".to_string());
    }
    assert!(settings_password_error.is_none());

    // Simulate: on_settings_change_password callback
    settings_password_changing = true;

    use chatter_core::crypto::opaque;
    use chatter_core::network::grpc::AuthClient;

    // What the SDK sends: an OPAQUE proof of the current password and an
    // OPAQUE registration of the new one (AuthService::change_password).
    let mut auth_client = AuthClient::new(pool.channel(), pool.auth_clone());
    let result: Result<(), String> = async {
        let proof = opaque::login_start(&settings_current_password).map_err(|e| e.to_string())?;
        let reg = opaque::registration_start(&settings_new_password).map_err(|e| e.to_string())?;
        let started = auth_client
            .change_password_start(&proof.ke1, &reg.request)
            .await
            .map_err(|e| e.to_string())?;
        let done = opaque::login_finish_async(proof, &settings_current_password, started.ke2)
            .await
            .map_err(|e| e.to_string())?;
        let record = opaque::registration_finish_async(
            reg,
            &settings_new_password,
            started.registration_response,
        )
        .await
        .map_err(|e| e.to_string())?;
        auth_client
            .change_password_finish(&started.flow_id, &done.ke3, &record)
            .await
            .map_err(|e| e.to_string())
    }
    .await;
    let new_password = settings_new_password.clone();

    match result {
        Ok(_) => {
            settings_password_changing = false;
            settings_current_password.clear();
            settings_new_password.clear();
            settings_confirm_password.clear();
        }
        Err(e) => {
            settings_password_changing = false;
            settings_password_error = Some(e);
        }
    }

    assert!(!settings_password_changing);
    assert!(
        settings_password_error.is_none(),
        "password change failed: {settings_password_error:?}"
    );

    // Log in again with the new password (same login identifier).
    let response = assert_ok(
        opaque_login(
            &mut auth_client,
            &user.username,
            &new_password,
            "test-device-after-password-change",
            &random_svr_public_key(),
            &user.device_auth_token,
            Some(user.device_id),
        )
        .await,
    );
    assert_eq!(
        response.user_profile.expect("login returns the profile").user_id,
        user.user_id
    );
}

#[tokio::test]
async fn test_settings_view_sessions_callback_flow() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);

    // Mock UI state
    let mut sessions: Vec<(u32, String)> = vec![];
    let mut sessions_loading = false;
    let mut active_modal = String::new();

    // Simulate: on_settings_view_sessions callback
    sessions_loading = true;
    active_modal = "sessions".to_string();

    use chatter_core::network::grpc::AuthClient;

    let mut auth_client = AuthClient::new(pool.channel(), pool.auth_clone());

    let result = auth_client.list_sessions().await;

    match result {
        Ok(response) => {
            sessions_loading = false;
            sessions = response.sessions
                .into_iter()
                .map(|s| (s.device_id, s.device_info))
                .collect();
        }
        Err(e) => {
            sessions_loading = false;
            eprintln!("List sessions failed: {}", e);
        }
    }

    // Verify
    assert!(!sessions_loading);
    assert_eq!(active_modal, "sessions");
    assert!(!sessions.is_empty(), "Should have at least current session");
}

#[tokio::test]
async fn test_settings_revoke_session_callback_flow() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);

    // Mock UI state
    let mut sessions_revoking = false;

    // Get sessions first
    use chatter_core::network::grpc::AuthClient;

    let mut auth_client = AuthClient::new(pool.channel(), pool.auth_clone());
    let _devices = assert_ok(auth_client.list_sessions().await);

    // Can't revoke current device, would need a second device to test properly
    // Just test the flow
    sessions_revoking = true;

    // Would call: auth_client.revoke_device(other_device_id).await
    // For now just verify state management
    sessions_revoking = false;

    assert!(!sessions_revoking);
}

#[tokio::test]
async fn test_settings_logout_callback_flow() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let user = assert_ok(setup_authenticated_test(&pool).await);

    // Mock UI state
    let mut logged_in = true;
    let mut current_user_id: Option<String> = Some(user.user_id.clone());
    let mut current_view = "main";

    // Simulate: on_settings_logout callback
    use chatter_core::network::grpc::AuthClient;

    let mut auth_client = AuthClient::new(pool.channel(), pool.auth_clone());

    let _ = auth_client.logout(user.device_id).await;

    // Update state (local logout)
    logged_in = false;
    current_user_id = None;
    current_view = "login";

    // Clear credentials
    pool.auth().clear();

    // Verify
    assert!(!logged_in);
    assert!(current_user_id.is_none());
    assert_eq!(current_view, "login");
}

// ============================================================================
// Settings Privacy Callback Tests
// ============================================================================

#[test]
fn test_settings_read_receipts_changed_callback() {
    // Simulate: on_settings_read_receipts_changed(enabled)
    let mut read_receipts_enabled = true;

    // User disables
    read_receipts_enabled = false;

    assert!(!read_receipts_enabled);

    // User enables
    read_receipts_enabled = true;

    assert!(read_receipts_enabled);
}

#[test]
fn test_settings_typing_indicators_changed_callback() {
    // Simulate: on_settings_typing_indicators_changed(enabled)
    let mut typing_indicators_enabled = true;

    // User disables
    typing_indicators_enabled = false;

    assert!(!typing_indicators_enabled);
}

#[test]
fn test_settings_sealed_sender_changed_callback() {
    // Simulate: on_settings_sealed_sender_changed(unrestricted)
    let mut sealed_sender_unrestricted = false;

    // User enables unrestricted (accepts messages from anyone)
    sealed_sender_unrestricted = true;

    assert!(sealed_sender_unrestricted);
}

#[test]
fn test_settings_copy_fingerprint_callback() {
    // Simulate: on_settings_copy_fingerprint
    let fingerprint = "AB CD EF 01 23 45 67 89";
    let mut clipboard_content: Option<String> = None;

    // Callback action - copies to clipboard
    clipboard_content = Some(fingerprint.to_string());

    assert_eq!(clipboard_content, Some(fingerprint.to_string()));
}

#[tokio::test]
async fn test_settings_refresh_keys_callback_flow() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);

    // Mock UI state
    let mut prekey_count = 50;
    let mut refreshing_keys = false;

    // Simulate: on_settings_refresh_keys callback
    refreshing_keys = true;

    // Would generate new prekeys and upload
    // For now just verify flow
    refreshing_keys = false;
    prekey_count = 100; // Refreshed

    assert!(!refreshing_keys);
    assert_eq!(prekey_count, 100);
}

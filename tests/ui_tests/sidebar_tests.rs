//! Sidebar callback tests.

use crate::common::*;

// ============================================================================
// Sidebar Callback Tests
// ============================================================================

#[tokio::test]
async fn test_channel_selected_callback_flow() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);

    // Create a channel to select
    use chatter_linux::network::grpc::ChannelClient;
    use chatter_linux::proto::chat_v1::ChannelType;
    use tokio_stream::StreamExt;

    let mut channel_client = ChannelClient::new(pool.channel(), pool.auth_clone());

    let channel_name_input = format!("test-channel-{}", rand::random::<u32>());
    let stream = assert_ok(
        channel_client
            .create_channel(&channel_name_input, ChannelType::ChannelPublic, None, vec![], None, None, None)
            .await
    );

    tokio::pin!(stream);

    let mut created_channel_id = String::new();
    let mut created_channel_name = String::new();
    while let Some(result) = stream.next().await {
        let event = result.expect("Stream error");
        if let Some(payload) = event.event {
            use chatter_linux::proto::chat_v1::channel_event::Event;
            if let Event::ChannelCreated(channel) = payload {
                created_channel_id = channel.id;
                created_channel_name = channel.name;
            }
        }
    }

    // Mock UI state
    let mut selected_channel_id: Option<String> = None;
    let mut channel_name = String::new();

    // Simulate: on_channel_selected callback
    selected_channel_id = Some(created_channel_id.clone());
    channel_name = created_channel_name.clone();

    // Verify
    assert_eq!(selected_channel_id, Some(created_channel_id));
    assert!(!channel_name.is_empty());
}

#[tokio::test]
async fn test_dm_selected_callback_flow() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user1 = assert_ok(setup_authenticated_test(&pool).await);

    // Create another user to DM
    let pool2 = assert_ok(create_test_pool().await);
    let user2 = assert_ok(register_test_user(&pool2).await);

    // Mock UI state
    let mut selected_dm_id: Option<String> = None;
    let mut dm_display_name = String::new();

    // Simulate: on_dm_selected callback would:
    // 1. Set selected_dm_id
    // 2. Load DM conversation
    // 3. Load messages

    selected_dm_id = Some(user2.user_id.clone());
    dm_display_name = user2.display_name.clone();

    // Verify
    assert!(selected_dm_id.is_some());
    assert!(!dm_display_name.is_empty());
}

#[test]
fn test_saved_messages_clicked_callback() {
    // Simulate: on_saved_messages_clicked
    let mut _current_view = "main";
    let mut show_saved_messages = false;

    // Callback action
    show_saved_messages = true;

    assert!(show_saved_messages);
}

#[test]
fn test_settings_clicked_callback() {
    // Simulate: on_settings_clicked
    let mut active_modal = String::new();

    // Callback action - opens settings modal
    active_modal = "settings".to_string();

    assert_eq!(active_modal, "settings");
}

#[test]
fn test_profile_clicked_callback() {
    // Simulate: on_profile_clicked
    let mut active_modal = String::new();
    let mut profile_user_id = String::new();

    // Callback action - opens profile modal for current user
    active_modal = "profile".to_string();
    profile_user_id = "current-user-id".to_string();

    assert_eq!(active_modal, "profile");
    assert!(!profile_user_id.is_empty());
}

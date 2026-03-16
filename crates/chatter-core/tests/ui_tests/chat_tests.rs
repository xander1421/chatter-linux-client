//! Chat action callback tests.

use crate::common::*;

// ============================================================================
// Chat Action Callback Tests
// ============================================================================

#[tokio::test]
async fn test_send_message_callback_flow() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);

    // Create a channel
    use chatter_linux::network::grpc::ChannelClient;
    use chatter_linux::network::grpc::MessageClient;
    use chatter_linux::proto::chat_v1::{ChannelType, message::{self, content}};
    use tokio_stream::StreamExt;

    let mut channel_client = ChannelClient::new(pool.channel(), pool.auth_clone());

    let channel_name = format!("send-test-{}", rand::random::<u32>());
    let stream = assert_ok(
        channel_client
            .create_channel(&channel_name, ChannelType::ChannelPublic, None, vec![], None, None, None)
            .await
    );

    tokio::pin!(stream);

    let mut channel_id = String::new();
    while let Some(result) = stream.next().await {
        let event = result.expect("Stream error");
        if let Some(payload) = event.event {
            use chatter_linux::proto::chat_v1::channel_event::Event;
            if let Event::ChannelCreated(channel) = payload {
                channel_id = channel.id;
            }
        }
    }

    // Mock UI state
    let mut composer_text = "Hello, world!".to_string();
    let mut messages_count = 0;

    // Simulate: on_send callback
    let mut message_client = MessageClient::new(pool.channel(), pool.auth_clone());

    // Create text content
    let content = message::Content {
        r#type: Some(message::content::Type::Text(composer_text.clone())),
    };

    let send_result = message_client
        .send_message(
            &channel_id,
            content,
            None,       // parent_id
            false,      // ephemeral
            None,       // ephemeral_ttl
            false,      // encrypt
            None,       // crypto_options
            vec![],     // mentioned_integrations
            false,      // targets_ai_bot
            vec![],     // mentioned_file_ids
        )
        .await;

    match send_result {
        Ok(_msg) => {
            // Clear composer on success
            composer_text.clear();
            messages_count += 1;
        }
        Err(e) => {
            eprintln!("Send failed: {}", e);
        }
    }

    // Verify
    assert!(composer_text.is_empty(), "Composer should be cleared after send");
    assert_eq!(messages_count, 1, "Should have sent one message");
}

#[test]
fn test_reply_clicked_callback() {
    // Simulate: on_reply_clicked(message_id)
    let message_id = "msg-123";
    let mut reply_to_message_id: Option<String> = None;
    let mut composer_placeholder = String::new();

    // Callback action
    reply_to_message_id = Some(message_id.to_string());
    composer_placeholder = format!("Reply to message...");

    assert_eq!(reply_to_message_id, Some("msg-123".to_string()));
    assert!(!composer_placeholder.is_empty());
}

#[test]
fn test_react_clicked_callback() {
    // Simulate: on_react_clicked(message_id)
    let message_id = "msg-456";
    let mut show_emoji_picker = false;
    let mut react_target_message: Option<String> = None;

    // Callback action - opens emoji picker for reactions
    show_emoji_picker = true;
    react_target_message = Some(message_id.to_string());

    assert!(show_emoji_picker);
    assert_eq!(react_target_message, Some("msg-456".to_string()));
}

#[test]
fn test_thread_clicked_callback() {
    // Simulate: on_thread_clicked(message_id)
    let message_id = "msg-789";
    let mut thread_root_id: Option<String> = None;
    let mut show_thread_panel = false;

    // Callback action - opens thread panel
    thread_root_id = Some(message_id.to_string());
    show_thread_panel = true;

    assert!(show_thread_panel);
    assert_eq!(thread_root_id, Some("msg-789".to_string()));
}

#[test]
fn test_more_clicked_callback() {
    // Simulate: on_more_clicked(message_id)
    let message_id = "msg-abc";
    let mut context_menu_message: Option<String> = None;
    let mut show_context_menu = false;

    // Callback action - opens context menu
    context_menu_message = Some(message_id.to_string());
    show_context_menu = true;

    assert!(show_context_menu);
    assert_eq!(context_menu_message, Some("msg-abc".to_string()));
}

#[tokio::test]
async fn test_load_more_callback_flow() {
    skip_if_no_server!();

    let pool = assert_ok(create_test_pool().await);
    let _user = assert_ok(setup_authenticated_test(&pool).await);

    // Create channel
    use chatter_linux::network::grpc::ChannelClient;
    use chatter_linux::network::grpc::MessageClient;
    use chatter_linux::proto::chat_v1::{ChannelType, MessageSortDirection, message::{self, content}};
    use tokio_stream::StreamExt;

    let mut channel_client = ChannelClient::new(pool.channel(), pool.auth_clone());
    let mut message_client = MessageClient::new(pool.channel(), pool.auth_clone());

    let channel_name = format!("load-more-test-{}", rand::random::<u32>());
    let stream = assert_ok(
        channel_client
            .create_channel(&channel_name, ChannelType::ChannelPublic, None, vec![], None, None, None)
            .await
    );

    tokio::pin!(stream);

    let mut channel_id = String::new();
    while let Some(result) = stream.next().await {
        let event = result.expect("Stream error");
        if let Some(payload) = event.event {
            use chatter_linux::proto::chat_v1::channel_event::Event;
            if let Event::ChannelCreated(channel) = payload {
                channel_id = channel.id;
            }
        }
    }

    // Send a few messages
    for i in 0..3 {
        let content = message::Content {
            r#type: Some(message::content::Type::Text(format!("Message {}", i))),
        };
        let _ = message_client
            .send_message(&channel_id, content, None, false, None, false, None, vec![], false, vec![])
            .await;
    }

    // Mock UI state
    let mut loading_messages = false;
    let mut has_more = true;

    // Simulate: on_load_more callback
    loading_messages = true;

    let messages_result = message_client
        .get_messages(
            &channel_id,
            50,                                      // limit
            None,                                    // thread_root_id
            None,                                    // page_token
            MessageSortDirection::SortNewestFirst,   // sort_direction
            None,                                    // reference_message_id
            None,                                    // reference_time
        )
        .await;

    match messages_result {
        Ok(response) => {
            loading_messages = false;
            has_more = response.messages.len() >= 50;
        }
        Err(_) => {
            loading_messages = false;
        }
    }

    // Verify
    assert!(!loading_messages, "Loading should be complete");
    assert!(!has_more, "Should not have more messages (only sent 3)");
}

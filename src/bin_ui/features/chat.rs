//! Chat callbacks.
//!
//! Handles sending messages.

use slint::{ComponentHandle, SharedString};

use chatter_linux::app::AppContext;
use chatter_linux::network::grpc::message_client::MessageClient;

use crate::MainWindow;

/// Wire chat callbacks (send message).
pub fn wire(window: &MainWindow, ctx: &AppContext) {
    let message_service = ctx.message_service();
    let chat_state = ctx.chat_state.clone();
    let auth_state = ctx.auth_state.clone();
    let grpc_pool = ctx.grpc_pool.clone();

    let weak_window = window.as_weak();

    window.on_send(move |text: SharedString| {
        let text = text.to_string();
        if text.trim().is_empty() {
            return;
        }

        let message_service = message_service.clone();
        let chat_state = chat_state.clone();
        let auth_state = auth_state.clone();
        let grpc_pool = grpc_pool.clone();
        let weak = weak_window.clone();

        tokio::task::spawn_blocking(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("Failed to create runtime for message send");

            rt.block_on(async {
                let channel_id = match chat_state.get().active_channel_id {
                    Some(id) => id,
                    None => {
                        tracing::warn!("Cannot send message: no active channel");
                        return;
                    }
                };

                if !auth_state.is_logged_in() {
                    tracing::warn!("Cannot send message: not logged in");
                    return;
                }

                let mut message_client =
                    MessageClient::new(grpc_pool.channel(), grpc_pool.auth_clone());

                let mut auth_client = chatter_linux::network::grpc::auth_client::AuthClient::new(
                    grpc_pool.channel(),
                    grpc_pool.auth_clone(),
                );

                match message_service
                    .send_message(
                        &mut message_client,
                        &mut auth_client,
                        &channel_id,
                        &text,
                        vec![],
                        None,
                        vec![],
                        false,
                    )
                    .await
                {
                    Ok(_) => {
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                w.set_composer_text("".into());
                            }
                        });
                    }
                    Err(e) => {
                        tracing::error!("Failed to send message: {}", e);
                    }
                }
            });
        });
    });
}

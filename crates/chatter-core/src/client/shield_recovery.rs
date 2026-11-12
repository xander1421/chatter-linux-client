//! First-contact Shield post recovery: the worker lives with the façade
//! because each retry goes through `ChatterClient::retry_failed_message` (the
//! same admission journal as a manual retry).
//!
//! It holds only a `Weak<ChatterClient>` and upgrades it once per tick, so it
//! never keeps the client alive.

use std::sync::Arc;

use super::ChatterClient;
use crate::services::message::shield_retry::retry_parked_with;

pub(crate) fn spawn_shield_send_recovery(
    client: &Arc<ChatterClient>,
) -> tokio::task::JoinHandle<()> {
    let weak = Arc::downgrade(client);
    tokio::spawn(async move {
        loop {
            crate::app::core_services::jittered_sleep().await;
            let Some(client) = weak.upgrade() else { break };
            let state = client.auth_state().get();
            let Some(owner) = state.current_user_id else {
                continue;
            };
            if let Err(error) = retry_parked_with(client.messages(), owner, |id| {
                let client = client.clone();
                async move {
                    // Same admission journal as manual retry: overlapping
                    // retries cannot create a second submission of the row.
                    let message = match client.retry_failed_message(&id.to_string()).await {
                        Ok(message) => Some(message),
                        Err(error) => {
                            tracing::debug!(message_id = %id, %error, "Parked Shield post retry did not complete");
                            client.messages().get_message_local(&id).ok().flatten()
                        }
                    };
                    if let Some(message) = message {
                        client.event_bus().emit_domain(crate::events::DomainEvent::MessageDeliveryChanged {
                            message_id: message.id,
                            channel_id: message.conversation_id,
                            status: message.delivery_status,
                        });
                    }
                }
            }).await {
                tracing::warn!(%error, "Could not read parked Shield posts");
            }
        }
    })
}

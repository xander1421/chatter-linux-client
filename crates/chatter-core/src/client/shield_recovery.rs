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
    let mut keys = client.event_bus().subscribe_domain();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = crate::app::core_services::jittered_sleep() => {}
                _ = crate::services::message::core::wait_for_key_verification(&mut keys) => {}
            }
            let Some(client) = weak.upgrade() else { break };
            let state = client.auth_state().get();
            let Some(owner) = state.current_user_id else {
                continue;
            };
            let secret = {
                let db = &client.services.database;
                (|| -> Result<Vec<String>, crate::error::StorageError> {
                    let conn = db.read_connection()?;
                    let mut stmt = conn.prepare(
                        "SELECT id FROM messages WHERE conversation_type='anon_dm' AND sender_id=?1
                         AND delivery_status='failed' AND failure_reason=?2 AND is_deleted=0
                         ORDER BY sent_at,id LIMIT 4",
                    )?;
                    Ok(stmt.query_map(rusqlite::params![owner.to_string(),
                        crate::services::message::core::VERIFYING_KEY], |row| row.get(0))?
                        .collect::<Result<_, _>>()?)
                })()
            };
            match secret {
                Ok(ids) => for id in ids {
                    if let Ok(message) = client.retry_failed_message(&id).await {
                        client.event_bus().emit_domain(crate::events::DomainEvent::MessageDeliveryChanged {
                            message_id: message.id,
                            channel_id: message.conversation_id,
                            status: message.delivery_status,
                        });
                    }
                },
                Err(error) => tracing::warn!(%error, "Could not read parked secret posts"),
            }
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

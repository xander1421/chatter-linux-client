//! First-contact Shield posts that have not spent or submitted anything.
//!
//! Refills never wake this worker. Its independent 5–15 minute jitter preserves
//! the same request/spend separation as the anonymous-group parked-send loop.
use std::sync::Arc;

use crate::domain::ids::{MessageId, UserId};
use crate::services::message::{MessageError, MessageService};

pub(crate) fn spawn_shield_send_recovery(
    client: &Arc<crate::client::ChatterClient>,
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

/// One independently scheduled retry tick. This reads only local pool state;
/// neither an empty pool nor a queued post can initiate a token request.
pub(super) async fn retry_parked_with<F, Fut>(
    service: &MessageService,
    owner: UserId,
    mut retry: F,
) -> Result<usize, MessageError>
where
    F: FnMut(MessageId) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    if !service
        .shield_mode
        .load(std::sync::atomic::Ordering::Acquire)
        || service.current_user_id()? != owner
        || service
            .token_manager
            .available_token_count()
            .map_err(|e| MessageError::DmError(e.to_string()))?
            == 0
    {
        return Ok(0);
    }
    let ids = {
        let conn = service.database.read_connection()?;
        let mut stmt = conn
            .prepare(
                "SELECT id FROM messages WHERE conversation_type='server_dm' AND sender_id=?1
             AND delivery_status='failed' AND failure_reason=?2 AND is_deleted=0
             ORDER BY sent_at,id LIMIT 4",
            )
            .map_err(crate::error::StorageError::from)?;
        let mut rows = stmt
            .query(rusqlite::params![
                owner.to_string(),
                super::core::SHIELD_WAITING_FOR_TOKENS
            ])
            .map_err(crate::error::StorageError::from)?;
        crate::storage::repositories::repo_utils::collect_decoded_rows(
            &mut rows,
            "parked_shield_post",
            |row| {
                row.get::<_, String>(0)?.parse().map_err(|_| {
                    crate::error::StorageError::InvalidData("Invalid message ID".into())
                })
            },
        )?
    };
    let count = ids.len();
    for id in ids {
        retry(id).await;
    }
    Ok(count)
}

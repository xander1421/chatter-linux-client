//! First-contact Shield posts that have not spent or submitted anything.
//!
//! The worker lives in `client/shield_recovery.rs`. Refills never wake it. Its
//! independent 5–15 minute jitter preserves the same request/spend separation
//! as the anonymous-group parked-send loop.

use crate::domain::ids::{MessageId, UserId};
use crate::services::message::{MessageError, MessageService};


/// One independently scheduled retry tick. This reads only local pool state;
/// neither an empty pool nor a queued post can initiate a token request.
pub(crate) async fn retry_parked_with<F, Fut>(
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
    {
        return Ok(0);
    }
    let tokens = service.token_manager.available_token_count()
        .map_err(|e| MessageError::DmError(e.to_string()))? > 0;
    let ids = {
        let conn = service.database.read_connection()?;
        let mut stmt = conn
            .prepare(
                "SELECT id FROM messages WHERE conversation_type='server_dm' AND sender_id=?1
             AND delivery_status='failed' AND (failure_reason=?2 OR (?3 AND failure_reason=?4))
             AND is_deleted=0
             ORDER BY sent_at,id LIMIT 4",
            )
            .map_err(crate::error::StorageError::from)?;
        let mut rows = stmt
            .query(rusqlite::params![
                owner.to_string(),
                super::core::SHIELD_VERIFYING_KEY,
                tokens,
                super::core::SHIELD_WAITING_FOR_TOKENS,
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

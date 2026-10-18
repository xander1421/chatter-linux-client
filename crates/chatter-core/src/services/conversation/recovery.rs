use super::*;

/// Retry only failed, session-local receipt timestamps. The outgoing message
/// journals use the same connection subscription plus a short fallback tick:
/// the tick matters when a gRPC failure precedes the reachability monitor.
pub(crate) fn spawn_receipt_recovery(client: &Arc<ChatterClient>) -> tokio::task::JoinHandle<()> {
    let weak = Arc::downgrade(client);
    let mut connectivity = client.offline_state().subscribe();
    let initially_connected = client.offline_state().connection_state().is_connected();
    let initial_revision = client.auth_state().get().session_revision;
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(5));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut retry_delay = std::time::Duration::from_secs(5);
        let mut retry_after = tokio::time::Instant::now();
        let mut was_connected = initially_connected;
        let mut last_revision = initial_revision;
        // The first tick has no pending receipts; later sends use the next tick.
        loop {
            tokio::select! {
                _ = ticker.tick() => {},
                _ = connectivity.recv() => {},
            }
            let Some(client) = weak.upgrade() else { break };
            let connected = client.offline_state().connection_state().is_connected();
            if connected && !was_connected {
                // A real reconnect should flush pending receipts promptly.
                retry_after = tokio::time::Instant::now();
                retry_delay = std::time::Duration::from_secs(5);
            }
            was_connected = connected;
            let auth = client.auth_state().get();
            if auth.session_revision != last_revision {
                last_revision = auth.session_revision;
                retry_after = tokio::time::Instant::now();
                retry_delay = std::time::Duration::from_secs(5);
            }
            let Some(_) = auth.current_user_id else {
                client.pending_receipts.lock().clear();
                client.sent_receipts.lock().clear();
                continue;
            };
            let entries = {
                let mut pending = client.pending_receipts.lock();
                pending.retain(|entry| {
                    entry.revision == auth.session_revision
                        && (!matches!(entry.conversation, ActiveConversation::ServerDm { .. })
                            || entry.shield == client.anon_mode_enabled())
                });
                if !client.read_receipts_enabled().unwrap_or(true) {
                    pending.retain(|entry| !entry.read);
                }
                pending.clone()
            };
            if !connected || tokio::time::Instant::now() < retry_after {
                continue;
            }
            for entry in entries {
                if client.auth_state().get().session_revision != entry.revision {
                    break;
                }
                if matches!(entry.conversation, ActiveConversation::ServerDm { .. })
                    && entry.shield != client.anon_mode_enabled()
                {
                    continue;
                }
                if entry.read && !client.chat_state().is_app_visible() {
                    continue;
                }
                let timestamps: Vec<_> = entry.timestamps.iter().copied().collect();
                let handle = ConversationHandle::new(entry.conversation, client.clone());
                let result = if entry.read {
                    handle.send_read_receipt_once(&timestamps).await
                } else {
                    handle.send_delivery_receipt_once(&timestamps).await
                };
                if client.auth_state().get().session_revision != entry.revision {
                    break;
                }
                let server_unreachable = matches!(
                    &result,
                    Err(ConversationError::Message(
                        crate::services::MessageError::Network(
                            crate::error::NetworkError::Unavailable
                                | crate::error::NetworkError::ConnectionFailed(_)
                                | crate::error::NetworkError::Timeout
                        )
                    ))
                );
                if result.is_ok()
                    || result
                        .as_ref()
                        .err()
                        .is_some_and(|error| !retryable_receipt_error(error))
                {
                    if result.is_ok() {
                        record_sent_receipt(&client, &entry);
                    }
                    let mut pending = client.pending_receipts.lock();
                    if let Some(current) = pending.iter_mut().find(|current| {
                        current.conversation == entry.conversation
                            && current.read == entry.read
                            && current.revision == entry.revision
                            && current.shield == entry.shield
                    }) {
                        for timestamp in &timestamps {
                            current.timestamps.remove(timestamp);
                        }
                    }
                    pending.retain(|current| !current.timestamps.is_empty());
                }
                if server_unreachable {
                    // One failed RPC is enough evidence that the server is down.
                    // Keep all pending entries, including this one, for a later
                    // tick instead of depositing each receipt into the outage.
                    retry_after = tokio::time::Instant::now() + retry_delay;
                    retry_delay = (retry_delay * 2).min(std::time::Duration::from_secs(60));
                    break;
                }
                if result.is_ok() {
                    retry_delay = std::time::Duration::from_secs(5);
                    retry_after = tokio::time::Instant::now();
                }
            }
        }
    })
}

/// Lifecycle-owned recovery; at most four due ORIGINAL batches per tick, with
/// exponential backoff and paused denied/exhausted batches retained locally.
pub(crate) fn spawn_delete_batch_recovery(
    client: &Arc<ChatterClient>,
) -> tokio::task::JoinHandle<()> {
    let weak = Arc::downgrade(client);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            let Some(client) = weak.upgrade() else {
                break;
            };
            let state = client.auth_state().get();
            let Some(account) = state.current_user_id else {
                continue;
            };
            let journal=crate::storage::repositories::delete_batch_retry_repo::DeleteBatchRetryRepository::new(client.messages().database.clone());
            let due = match journal.due(&account) {
                Ok(due) => due,
                Err(error) => {
                    tracing::warn!(%error,"Cannot load deletion retries");
                    continue;
                }
            };
            for (scope, batch) in due {
                let Some((kind, id)) = scope.split_once(':') else {
                    continue;
                };
                let conversation = match kind {
                    "channel" => id.parse().ok().map(ActiveConversation::Channel),
                    "server_dm" => id
                        .parse()
                        .ok()
                        .map(|peer_id| ActiveConversation::ServerDm { peer_id }),
                    "anon_dm" => id.parse().ok().map(ActiveConversation::AnonDm),
                    "anon_group" => id.parse().ok().map(ActiveConversation::AnonGroup),
                    _ => None,
                };
                let Some(conversation) = conversation else {
                    continue;
                };
                let targets = batch
                    .targets
                    .iter()
                    .map(|t| (t.author, t.timestamp))
                    .collect::<Vec<_>>();
                if let Err(error) = client
                    .conversation(conversation)
                    .send_delete_batch_in_session(&targets, Some((account, state.session_revision)))
                    .await
                {
                    tracing::warn!(%error,%scope,batch_id=%batch.id,"Deletion retry remains pending");
                }
            }
        }
    })
}

//! Message indexer.
//!
//! Background service that auto-indexes messages for semantic search.
//! Listens for new messages and generates embeddings via the local AI
//! backend (Ollama). Designed for E2E encrypted channels where the
//! server cannot perform search.

use std::sync::Arc;

use tokio::sync::{broadcast, mpsc};

use crate::inbox::handlers::DomainEvent;
use crate::services::semantic_search::SemanticSearchService;

/// Batch size for embedding generation.
const INDEX_BATCH_SIZE: usize = 20;

/// A message to be indexed.
#[derive(Debug, Clone)]
pub struct IndexableMessage {
    pub message_id: String,
    pub channel_id: String,
    pub content: String,
}

/// Message indexer that processes incoming messages in batches.
///
/// Receives messages via a channel and indexes them asynchronously
/// using the SemanticSearchService. Batches messages to reduce
/// the number of Ollama API calls.
pub struct MessageIndexer {
    tx: mpsc::Sender<IndexableMessage>,
}

impl MessageIndexer {
    /// Spawn a new MessageIndexer background task.
    ///
    /// Returns the indexer handle (for sending messages) and a JoinHandle
    /// for the background task.
    pub fn spawn(
        search_service: Arc<SemanticSearchService>,
    ) -> (Self, tokio::task::JoinHandle<()>) {
        let (tx, rx) = mpsc::channel::<IndexableMessage>(500);

        let handle = tokio::spawn(indexer_loop(rx, search_service));

        (Self { tx }, handle)
    }

    /// Submit a message for indexing.
    ///
    /// Non-blocking — returns immediately. If the indexer is overloaded
    /// (buffer full), the message is silently dropped.
    pub fn submit(&self, message: IndexableMessage) {
        // Use try_send to avoid blocking the caller
        let _ = self.tx.try_send(message);
    }

    /// Check if the indexer channel is still open.
    pub fn is_active(&self) -> bool {
        !self.tx.is_closed()
    }

    /// Spawn a listener that forwards `DomainEvent::MessageReceived` events
    /// to this indexer.
    ///
    /// Extracts plaintext from received messages and submits them for indexing.
    /// Encrypted, system, and call messages are silently skipped.
    pub fn subscribe_events(
        &self,
        mut domain_rx: broadcast::Receiver<DomainEvent>,
    ) -> tokio::task::JoinHandle<()> {
        let tx = self.tx.clone();

        tokio::spawn(async move {
            loop {
                match domain_rx.recv().await {
                    Ok(DomainEvent::MessageReceived { message }) => {
                        // Extract indexable text (plain, mixed-text, or AI-generated)
                        if let Some(text) = message.as_text() {
                            if text.trim().len() >= 5 {
                                let _ = tx
                                    .try_send(IndexableMessage {
                                        message_id: message.id.0.to_string(),
                                        channel_id: message.conversation_id.0.to_string(),
                                        content: text.to_string(),
                                    });
                            }
                        }
                    }
                    Ok(_) => { /* ignore other events */ }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(
                            skipped = n,
                            "MessageIndexer lagged — some messages may not be indexed"
                        );
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        tracing::debug!("MessageIndexer event listener stopping — bus closed");
                        break;
                    }
                }
            }
        })
    }
}

/// Background loop that batches and indexes messages.
async fn indexer_loop(
    mut rx: mpsc::Receiver<IndexableMessage>,
    search_service: Arc<SemanticSearchService>,
) {
    let mut batch: Vec<IndexableMessage> = Vec::with_capacity(INDEX_BATCH_SIZE);

    loop {
        // Wait for the first message (blocks until available)
        let msg = match rx.recv().await {
            Some(msg) => msg,
            None => break, // Channel closed, indexer shut down
        };
        batch.push(msg);

        // Drain any additional pending messages up to batch size
        while batch.len() < INDEX_BATCH_SIZE {
            match rx.try_recv() {
                Ok(msg) => batch.push(msg),
                Err(_) => break, // No more pending messages
            }
        }

        // Index the batch
        let messages: Vec<(String, String, String)> = batch
            .drain(..)
            .map(|m| (m.message_id, m.channel_id, m.content))
            .collect();

        match search_service.index_messages_batch(&messages).await {
            Ok(count) => {
                if count > 0 {
                    tracing::debug!(count, "Indexed messages for semantic search");
                }
            }
            Err(e) => {
                tracing::warn!("Failed to index messages: {}", e);
                // Don't crash on indexing failure — it's best-effort
            }
        }
    }

    tracing::info!("Message indexer shut down");
}

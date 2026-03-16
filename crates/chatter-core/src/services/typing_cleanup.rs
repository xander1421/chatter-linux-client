//! Background task for cleaning up expired typing indicators.
//!
//! Prevents typing indicators from getting stuck by periodically
//! removing expired entries from the typing aggregator cache.

use std::sync::Arc;
use std::time::Duration;

use tokio::time;

use crate::network::streaming::typing_stream::TypingAggregator;

/// Interval between cleanup runs (5 seconds).
const CLEANUP_INTERVAL: Duration = Duration::from_secs(5);

/// Start background typing indicator cleanup task.
///
/// This task runs periodically to remove expired typing indicators
/// from the cache, preventing them from appearing stuck in the UI.
///
/// # Arguments
///
/// * `typing_aggregator` - The typing aggregator to clean up
///
/// # Returns
///
/// A JoinHandle for the spawned task. The task runs indefinitely
/// until cancelled.
///
/// # Example
///
/// ```ignore
/// let aggregator = Arc::new(TypingAggregator::new());
/// let cleanup_handle = start_typing_cleanup(aggregator.clone());
///
/// // Later, to stop the cleanup task:
/// cleanup_handle.abort();
/// ```
pub fn start_typing_cleanup(
    typing_aggregator: Arc<TypingAggregator>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = time::interval(CLEANUP_INTERVAL);
        interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);

        loop {
            interval.tick().await;
            
            // Clean up expired typing indicators
            let removed = typing_aggregator.cleanup_expired();
            
            if removed > 0 {
                tracing::trace!(
                    removed_count = removed,
                    "Cleaned up expired typing indicators"
                );
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::timeout;

    #[tokio::test]
    async fn test_cleanup_task_runs() {
        let aggregator = Arc::new(TypingAggregator::with_timeout(Duration::from_millis(100)));
        
        // Add a typing indicator that will expire quickly
        aggregator.on_typing_started("channel-1", "user-1");
        
        // Start cleanup task
        let handle = start_typing_cleanup(aggregator.clone());
        
        // Wait long enough for cleanup to run and indicator to expire
        tokio::time::sleep(Duration::from_millis(200)).await;
        
        // Check that the typing indicator was cleaned up
        assert!(!aggregator.is_anyone_typing("channel-1"));
        
        // Clean up
        handle.abort();
    }

    #[tokio::test]
    async fn test_cleanup_task_can_be_cancelled() {
        let aggregator = Arc::new(TypingAggregator::new());
        let handle = start_typing_cleanup(aggregator);
        
        // Cancel the task
        handle.abort();
        
        // Should complete quickly
        let result = timeout(Duration::from_millis(100), handle).await;
        assert!(result.is_ok());
    }
}
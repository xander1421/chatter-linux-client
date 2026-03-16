//! Member fanout service.
//!
//! Consolidates the common pattern of iterating over group members
//! and sending to each one with error handling.
//!
//! Used by:
//! - Server-managed groups (SKDM distribution, announcements)
//! - Anonymous groups (message fanout, member list broadcast)

use std::future::Future;
use std::time::Duration;

use rand::RngCore;
use tokio::time::sleep;

use crate::domain::UserId;

/// Result of a fanout operation.
#[derive(Debug)]
pub struct FanoutResult {
    /// Number of successful sends.
    pub success_count: usize,
    /// Number of failed sends.
    pub failure_count: usize,
    /// Member IDs that failed (for retry or logging).
    pub failed_members: Vec<UserId>,
}

impl FanoutResult {
    /// Create a new empty result.
    pub fn new() -> Self {
        Self {
            success_count: 0,
            failure_count: 0,
            failed_members: Vec::new(),
        }
    }

    /// Record a success.
    pub fn record_success(&mut self) {
        self.success_count += 1;
    }

    /// Record a failure.
    pub fn record_failure(&mut self, member_id: UserId) {
        self.failure_count += 1;
        self.failed_members.push(member_id);
    }

    /// Check if all sends succeeded.
    pub fn all_succeeded(&self) -> bool {
        self.failure_count == 0
    }
}

impl Default for FanoutResult {
    fn default() -> Self {
        Self::new()
    }
}

/// Options for fanout operations.
#[derive(Debug, Clone)]
pub struct FanoutOptions {
    /// Maximum random delay between sends (milliseconds).
    /// Set to 0 for no delay.
    pub max_delay_ms: u32,

    /// Whether to continue on individual send failures.
    pub continue_on_error: bool,
}

impl Default for FanoutOptions {
    fn default() -> Self {
        Self {
            max_delay_ms: 0,
            continue_on_error: true,
        }
    }
}

impl FanoutOptions {
    /// Create options with staggered delivery (for traffic correlation resistance).
    pub fn staggered(max_delay_ms: u32) -> Self {
        Self {
            max_delay_ms,
            continue_on_error: true,
        }
    }

    /// Create options for immediate delivery (no delay).
    pub fn immediate() -> Self {
        Self::default()
    }
}

/// Execute a fanout operation to multiple members.
///
/// # Arguments
/// * `members` - Iterator of (user_id, optional mailbox data)
/// * `current_user` - The current user ID (excluded from fanout)
/// * `options` - Fanout options (delay, error handling)
/// * `send_fn` - Async function to send to each member
///
/// # Example
/// ```ignore
/// let result = fanout(
///     members.iter().map(|m| (m.user_id, m.mailbox_pk)),
///     current_user,
///     FanoutOptions::staggered(5000),
///     |user_id, mailbox_pk| async move {
///         service.send_to_member(user_id, mailbox_pk, &content).await
///     },
/// ).await;
/// ```
pub async fn fanout<T, F, Fut>(
    members: impl Iterator<Item = (UserId, T)>,
    current_user: UserId,
    options: FanoutOptions,
    send_fn: F,
) -> FanoutResult
where
    T: Clone,
    F: Fn(UserId, T) -> Fut,
    Fut: Future<Output = Result<(), Box<dyn std::error::Error + Send + Sync>>>,
{
    let mut result = FanoutResult::new();

    for (member_id, data) in members {
        // Skip self
        if member_id == current_user {
            continue;
        }

        // Apply staggered delay if configured
        if options.max_delay_ms > 0 {
            let delay_ms = rand::rng().next_u32() % options.max_delay_ms;
            sleep(Duration::from_millis(delay_ms as u64)).await;
        }

        // Execute send
        match send_fn(member_id, data).await {
            Ok(()) => {
                result.record_success();
            }
            Err(e) => {
                tracing::warn!("Fanout to {} failed: {}", member_id, e);
                result.record_failure(member_id);

                if !options.continue_on_error {
                    break;
                }
            }
        }
    }

    result
}

/// Simpler fanout for cases where members only have user_id.
pub async fn fanout_simple<F, Fut>(
    member_ids: impl Iterator<Item = UserId>,
    current_user: UserId,
    options: FanoutOptions,
    send_fn: F,
) -> FanoutResult
where
    F: Fn(UserId) -> Fut,
    Fut: Future<Output = Result<(), Box<dyn std::error::Error + Send + Sync>>>,
{
    fanout(
        member_ids.map(|id| (id, ())),
        current_user,
        options,
        |id, _| send_fn(id),
    )
    .await
}

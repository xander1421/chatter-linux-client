//! Anonymous group event bus handlers.
//!
//! Spawns a background task to handle DomainEvents for anonymous groups:
//! - AnonGroupMemberJoined
//! - AnonGroupMemberLeft
//! - AnonGroupMemberKicked

use std::sync::Arc;

use slint::Weak;

use chatter_core::client::ChatterClient;
use chatter_core::inbox::DomainEvent;
use crate::MainWindow;

use super::anon_group_handlers::{
    handle_anon_group_member_joined,
    handle_anon_group_member_left,
    handle_anon_group_member_kicked,
};

/// Spawn anon group event handler to process member events from the event bus.
///
/// Subscribes to `DomainEvent::AnonGroupMemberJoined/Left/Kicked` and updates
/// the UI with system messages when members join or leave.
///
/// Returns a handle to the spawned task.
pub fn spawn_anon_group_event_handler(
    weak_window: Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
) -> tokio::task::JoinHandle<()> {
    let ui_ctx = ui_ctx.clone();
    let mut event_rx = ui_ctx.event_bus().subscribe_domain();

    tokio::spawn(async move {
        loop {
            let event = match event_rx.recv().await {
                Ok(event) => event,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(dropped_count = n, "Domain event receiver lagged, continuing");
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            };
            match event {
                DomainEvent::AnonGroupMemberJoined { group_id, user_id, .. } => {
                    handle_anon_group_member_joined(
                        &weak_window,
                        &ui_ctx,
                        group_id,
                        user_id,
                    );
                }
                DomainEvent::AnonGroupMemberLeft { group_id, user_id } => {
                    handle_anon_group_member_left(
                        &weak_window,
                        &ui_ctx,
                        group_id,
                        user_id,
                    );
                }
                DomainEvent::AnonGroupMemberKicked { group_id, user_id, kicked_by } => {
                    handle_anon_group_member_kicked(
                        &weak_window,
                        &ui_ctx,
                        group_id,
                        user_id,
                        kicked_by,
                    );
                }
                DomainEvent::AnonGroupMemberListReceived { .. } => {
                    // Profile sharing handled at join time (invite + JoinAck).
                    // No action needed here — owner's member list already includes profiles.
                }
                _ => {
                    // Ignore other events
                }
            }
        }
    })
}

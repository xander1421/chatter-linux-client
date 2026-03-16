//! Anonymous group event bus handlers.
//!
//! Spawns a background task to handle DomainEvents for anonymous groups:
//! - AnonGroupMemberJoined
//! - AnonGroupMemberLeft
//! - AnonGroupMemberKicked

use std::sync::Arc;

use slint::Weak;

use chatter_core::app::UIContext;
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
    ui_ctx: &Arc<UIContext>,
) -> tokio::task::JoinHandle<()> {
    let ui_ctx = ui_ctx.clone();
    let mut event_rx = ui_ctx.event_bus.subscribe_domain();

    tokio::spawn(async move {
        while let Ok(event) = event_rx.recv().await {
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
                DomainEvent::AnonGroupMemberListReceived { group_id, .. } => {
                    // Share our profile with group members we just learned about.
                    // This ensures all members learn our display name directly rather
                    // than relying on the group owner to relay it.
                    let anon_group_service = ui_ctx.anon_group_service.clone();
                    tokio::spawn(async move {
                        if let Err(e) = anon_group_service.share_profile_with_group(&group_id).await {
                            tracing::warn!(
                                group_id = %group_id,
                                error = %e,
                                "Failed to share profile with group members"
                            );
                        } else {
                            tracing::debug!(
                                group_id = %group_id,
                                "Shared profile with group members after member list update"
                            );
                        }
                    });
                }
                _ => {
                    // Ignore other events
                }
            }
        }
    })
}

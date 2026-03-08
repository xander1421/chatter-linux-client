//! Anonymous group event bus handlers.
//!
//! Spawns a background task to handle DomainEvents for anonymous groups:
//! - AnonGroupMemberJoined
//! - AnonGroupMemberLeft
//! - AnonGroupMemberKicked

use std::sync::Arc;

use slint::Weak;

use chatter_linux::app::UIContext;
use chatter_linux::inbox::DomainEvent;
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
                DomainEvent::AnonGroupMemberListReceived { group_id, members } => {
                    let anon_group_service = ui_ctx.anon_group_service.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        if let Err(e) = anon_group_service.handle_member_list(group_id, members) {
                            tracing::error!(
                                group_id = %group_id,
                                error = %e,
                                "Failed to handle incoming member list"
                            );
                        } else {
                            tracing::info!(
                                group_id = %group_id,
                                "Successfully synced member list from anon DM"
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

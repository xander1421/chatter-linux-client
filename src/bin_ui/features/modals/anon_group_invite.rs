//! Anonymous group invite event handler.
//!
//! Handles DomainEvent::AnonGroupInviteReceived and displays invite dialog.

use std::sync::Arc;

use slint::Weak;

use chatter_linux::app::UIContext;
use chatter_linux::inbox::DomainEvent;
use crate::MainWindow;

/// Spawn anon group invite handler to process incoming invite events.
///
/// Subscribes to `DomainEvent::AnonGroupInviteReceived` from the event bus
/// and auto-accepts the invite (TODO: show UI dialog for user choice).
///
/// Returns a handle to the spawned task.
pub fn spawn_anon_group_invite_handler(
    weak_window: Weak<MainWindow>,
    ui_ctx: &Arc<UIContext>,
) -> tokio::task::JoinHandle<()> {
    let anon_group_service = ui_ctx.anon_group_service.clone();
    let user_service = ui_ctx.user_service.clone();
    let mut event_rx = ui_ctx.event_bus.subscribe_domain();

    tokio::spawn(async move {
        while let Ok(event) = event_rx.recv().await {
            match event {
                DomainEvent::AnonGroupInviteReceived {
                    group_id,
                    group_name,
                    inviter_id,
                    inviter_mailbox_pk,
                    ..
                } => {
                    let inviter_name = user_service.get_display_name(&inviter_id);
                    tracing::info!(
                        group_id = %group_id,
                        group_name = %group_name,
                        inviter_id = %inviter_id,
                        inviter_name = %inviter_name,
                        "Processing anon group invite - auto-accepting for now"
                    );

                    // Capture values for UI update before spawning
                    let group_id_str = group_id.to_string();
                    let group_name_for_ui = group_name.clone();
                    let weak_for_ui = weak_window.clone();

                    // For now, auto-accept the invite
                    // TODO: Show a UI dialog for the user to accept/decline
                    let service = anon_group_service.clone();
                    tokio::spawn(async move {

                            match service.accept_invite(group_id, group_name.clone(), inviter_id, inviter_mailbox_pk).await {
                                Ok(_group) => {
                                    tracing::info!(
                                        group_id = %group_id,
                                        "Successfully accepted anon group invite"
                                    );

                                    // Add to sidebar
                                    let gid_str = group_id_str;
                                    let gname = group_name_for_ui;
                                    let weak = weak_for_ui;
                                    let _ = slint::invoke_from_event_loop(move || {
                                        if let Some(w) = weak.upgrade() {
                                            crate::bin_ui::features::sidebar::add_private_group_to_sidebar(
                                                &w,
                                                &gid_str,
                                                &gname,
                                                1, // Just us for now, member list will update later
                                            );
                                        }
                                    });
                                }
                                Err(e) => {
                                    tracing::error!(
                                        group_id = %group_id,
                                        error = %e,
                                        "Failed to accept anon group invite"
                                    );
                                }
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

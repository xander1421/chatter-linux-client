//! Simplified send message callback - delegates to services.
//!
//! This is a cleaner version that:
//! 1. Checks for Jarvis commands (delegates to JarvisService)
//! 2. Uploads attachments if needed (delegates to AttachmentService)
//! 3. Sends the message (delegates to MessageService polymorphically)
//! 4. Lets EventBus handle UI updates reactively

use std::sync::Arc;

use slint::ComponentHandle;

use chatter_linux::app::UIContext;
use chatter_linux::domain::jarvis::JarvisContext;
use chatter_linux::events::DomainEvent;

use crate::MainWindow;

pub fn wire_send_v2(window: &MainWindow, ui_ctx: &Arc<UIContext>) {
    let ui_ctx_clone = ui_ctx.clone();
    let weak_window = window.as_weak();
    
    window.on_send(move |text: slint::SharedString| {
        let text = text.to_string();
        if text.trim().is_empty() { return; }
        
        let ui_ctx = ui_ctx_clone.clone();
        let weak = weak_window.clone();
        
        tokio::spawn(async move {
            // Get active channel ID
            let channel_id = match ui_ctx.chat_state.get().active_channel_id {
                Some(id) => id,
                None => {
                    tracing::warn!("No active channel for message send");
                    return;
                }
            };
            
            // 1. Check if Jarvis should handle this input
            let jarvis_context = JarvisContext::channel(&channel_id);
            let author_id = ui_ctx.auth_state.get().current_user_id;
            
            match ui_ctx.jarvis_service.process_input(&text, &jarvis_context, author_id.as_ref()).await {
                Ok(true) => {
                    // Jarvis handled it - done
                    return;
                }
                Ok(false) => {
                    // Not a Jarvis command, continue with normal send
                }
                Err(e) => {
                    tracing::error!("Jarvis process_input error: {}", e);
                    // Continue with normal send
                }
            }
            
            // 2. Handle attachments if any
            let pending_attachments = ui_ctx.chat_state.get_pending_attachments();
            let attachment_pointers = if !pending_attachments.is_empty() {
                // Convert to paths
                let paths: Vec<std::path::PathBuf> = pending_attachments
                    .iter()
                    .filter(|a| a.pointer.is_none()) // Skip pre-uploaded
                    .map(|a| a.path.clone())
                    .collect();
                
                // Use batch upload
                match ui_ctx.attachment_service.upload_batch(&ui_ctx.grpc_pool, paths).await {
                    Ok(pointers) => {
                        // Clear pending attachments from UI state
                        ui_ctx.chat_state.clear_pending_attachments();
                        pointers
                    }
                    Err(e) => {
                        tracing::error!("Failed to upload attachments: {}", e);
                        // Emit error event for UI
                        ui_ctx.event_bus.emit_domain(DomainEvent::MessageSendFailed {
                            channel_id,
                            error: format!("Attachment upload failed: {}", e),
                        });
                        return;
                    }
                }
            } else {
                vec![]
            };
            
            // 3. Send message using polymorphic MessageService
            match ui_ctx.message_service.send_polymorphic(&channel_id, &text, attachment_pointers).await {
                Ok(message) => {
                    // Emit success event - UI will update reactively
                    ui_ctx.event_bus.emit_domain(DomainEvent::MessageSent {
                        message_id: message.id,
                        channel_id: message.channel_id,
                        sender_id: message.sender_id,
                        content: text,
                        sent_at: message.sent_at,
                    });
                    
                    // Update sidebar preview
                    if let Some(w) = weak.upgrade() {
                        let _ = slint::invoke_from_event_loop(move || {
                            // This should also be event-driven eventually
                            // For now, just clear the composer
                            w.set_composer_text("".into());
                        });
                    }
                }
                Err(e) => {
                    tracing::error!("Failed to send message: {}", e);
                    // Emit error event
                    ui_ctx.event_bus.emit_domain(DomainEvent::MessageSendFailed {
                        channel_id,
                        error: e.to_string(),
                    });
                }
            }
        });
    });
}
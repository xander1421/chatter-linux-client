//! The composer's send path, when the open conversation is a mesh one.
//!
//! Hooked into `features::chat::send_message` the same way the Jarvis channel
//! is: an early interception, before the message reaches anything that would
//! try to talk to a server. A mesh conversation makes no RPC at all, so it
//! must never enter the gateway pipeline.

use std::sync::Arc;

use slint::ComponentHandle;

use chatter_core::client::ChatterClient;

use crate::{MainWindow, MeshGlobal, run_ui};

use super::core_api;

/// Route a composer send into the mesh.
///
/// Returns `true` when the send was consumed — either dispatched or
/// deliberately dropped — so the caller must stop. Returns `false` when no
/// mesh conversation is open and the ordinary send path should run.
///
/// The sent message is not appended here: `MeshService` mirrors our own sends
/// back through the event stream with `is_local` set, so the local and remote
/// paths build the row exactly once, in exactly the same way.
#[must_use]
pub fn intercept_send(
    ui_ctx: &Arc<ChatterClient>,
    text: &str,
    weak: slint::Weak<MainWindow>,
) -> bool {
    let Some(conversation) = super::active() else {
        return false;
    };

    let text = text.trim().to_string();
    if text.is_empty() {
        // Empty bodies cost airtime and decode as nothing on the far side.
        // Core refuses them too; refusing here keeps the composer quiet.
        return true;
    }

    let ui_ctx = ui_ctx.clone();
    tokio::spawn(async move {
        match core_api::send(&ui_ctx, &conversation, &text).await {
            Ok(delivery) => {
                // Not a receipt: this is the branch the router took. The row's
                // own state arrives as a `MeshEvent::Delivery`.
                tracing::info!(?delivery, "mesh: message handed to the router");
                run_ui!(weak, |w| w.global::<MeshGlobal>().set_notice("".into()));
            }
            Err(error) => {
                // Nothing could take it at all — a peer no transport reaches, a
                // relay-less build, an invalid cell. Worth saying out loud,
                // because unlike "no peers nearby" this one means the message
                // did not leave.
                //
                // The composer cleared itself optimistically when the send was
                // dispatched, so the text is put back: a message that never left
                // the device must not also disappear from it. Only when the
                // composer is still empty, so a reply typed in the meantime is
                // never clobbered.
                tracing::warn!(%error, "mesh: message not sent");
                let notice = format!("Not sent: {error}");
                run_ui!(weak, |w| {
                    w.global::<MeshGlobal>().set_notice(notice.clone().into());
                    if w.global::<crate::ChatPane>().get_composer_text().is_empty() {
                        let pane = w.global::<crate::ChatPane>();
                        pane.set_composer_text(text.clone().into());
                        pane.set_composer_caret_end_seq(pane.get_composer_caret_end_seq() + 1);
                    }
                });
            }
        }
    });

    true
}

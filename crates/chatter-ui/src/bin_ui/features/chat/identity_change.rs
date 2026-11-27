//! The peer-identity-change prompt: display and acceptance.
//!
//! Detection of a changed identity key has worked for a while — the change is
//! parked in `pending_identity_changes` and every send path refuses while it is
//! there. What was missing was any way for a human to see or resolve it, so a
//! peer reinstalling their app left the conversation permanently dead with the
//! toast "Encryption failed — try again", which by that error's own definition
//! can never succeed.
//!
//! This module is the presentation half only. Every fact and every word comes
//! from `ChatterClient::identity_change_prompt`; acceptance goes straight to
//! `ChatterClient::accept_peer_identity_change`, which does the whole
//! pin/drop-sessions/rotate-sender-keys transaction in core.

use std::sync::Arc;

use slint::ComponentHandle;

use chatter_core::client::ChatterClient;
use chatter_core::domain::ids::UserId;
use chatter_core::state::ActiveConversation;

use crate::MainWindow;

pub(crate) fn active_peer(client: &ChatterClient) -> Option<UserId> {
    match client.active_conversation() {
        ActiveConversation::ServerDm { peer_id } => Some(peer_id),
        ActiveConversation::AnonDm(id) => client.get_anon_dm_conversation(&id).map(|dm| dm.peer_id),
        _ => None,
    }
}

/// Restore the durable account warning when a session opens or detects a change.
/// Every word comes from `ChatterClient::own_identity_warning`.
pub fn refresh_own(w: &MainWindow, client: &ChatterClient) {
    match client.own_identity_warning() {
        Ok(warning) => {
            w.set_own_identity_mismatch(warning.is_some());
            let warning = warning.unwrap_or_else(|| chatter_core::client::OwnIdentityWarning {
                title: String::new(),
                detail: String::new(),
                action: None,
            });
            w.set_own_identity_title(warning.title.into());
            w.set_own_identity_detail(warning.detail.into());
            w.set_own_identity_action(warning.action.unwrap_or_default().into());
        }
        Err(error) => tracing::warn!(%error, "Could not load own identity-change warning"),
    }
}

/// Show or hide the prompt for `peer`, reading the pending change from core.
///
/// Called on every DM open (so a change parked while the app was closed
/// is visible on the next look) and whenever the inbox reports a fresh one.
/// Always writes all four properties, including the cleared case — a prompt
/// left over from a previous conversation would offer to pin the wrong key.
pub fn refresh(w: &MainWindow, ui_ctx: &ChatterClient, peer: &UserId) {
    match ui_ctx.identity_change_prompt(peer) {
        Ok(Some(prompt)) => {
            tracing::warn!(
                peer = %peer,
                attributed = prompt.attributed,
                "[IDENTITY] showing the identity-change prompt — conversation is wedged"
            );
            w.global::<crate::ChatPane>()
                .set_identity_change_title(prompt.title.into());
            w.global::<crate::ChatPane>()
                .set_identity_change_detail(prompt.detail.into());
            w.global::<crate::ChatPane>()
                .set_identity_change_accept(prompt.accept.into());
            w.global::<crate::ChatPane>()
                .set_identity_change_attributed(prompt.attributed);
            w.global::<crate::ChatPane>()
                .set_identity_change_visible(true);
        }
        Ok(None) => clear(w),
        Err(e) => {
            // Failing closed here would be worse than failing open: the prompt
            // is the only route back, so a storage hiccup must not be allowed
            // to leave a stale one pinned to the wrong conversation.
            tracing::error!(peer = %peer, error = %e, "[IDENTITY] could not read the pending change");
            clear(w);
        }
    }
}

/// Hide the prompt after acceptance and while changing conversations.
pub fn clear(w: &MainWindow) {
    w.global::<crate::ChatPane>()
        .set_identity_change_visible(false);
    w.global::<crate::ChatPane>()
        .set_identity_change_title("".into());
    w.global::<crate::ChatPane>()
        .set_identity_change_detail("".into());
    w.global::<crate::ChatPane>()
        .set_identity_change_accept("".into());
    w.global::<crate::ChatPane>()
        .set_identity_change_attributed(false);
}

/// Wire the Accept button.
///
/// The peer comes from the ACTIVE conversation rather than from the callback,
/// so the key being pinned is always the one belonging to the conversation the
/// prompt is drawn in. `accept_peer_identity_change` takes no key at all: it
/// re-reads the parked one, so the UI cannot accept something the user was
/// never shown.
pub fn wire_accept(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let ui_ctx = ui_ctx.clone();
    let weak = window.as_weak();

    window
        .global::<crate::ChatPane>()
        .on_identity_change_accept_clicked(move || {
            let Some(w) = weak.upgrade() else { return };

            let Some(peer_id) = active_peer(&ui_ctx) else {
                tracing::warn!("[IDENTITY] accept fired without a peer conversation");
                return;
            };

            // Read before accepting: the toast confirms the decision the user
            // was shown, which accepting removes.
            let accepted = ui_ctx
                .identity_change_prompt(&peer_id)
                .ok()
                .flatten()
                .map(|prompt| prompt.accepted)
                .unwrap_or_default();
            match ui_ctx.accept_peer_identity_change(&peer_id) {
                Ok(sessions_dropped) => {
                    tracing::warn!(
                        peer = %peer_id,
                        sessions_dropped,
                        "[IDENTITY] user ACCEPTED a new peer identity key"
                    );
                    clear(&w);
                    // Say so explicitly. The visible consequence of accepting is a
                    // bar disappearing, which on its own does not distinguish
                    // "accepted" from "the button did nothing".
                    w.set_toast_message(accepted.into());
                    w.set_toast_variant("success".into());
                    w.set_toast_visible(true);
                }
                Err(e) => {
                    tracing::error!(peer = %peer_id, error = %e, "[IDENTITY] accept failed");
                    // The prompt deliberately stays up: the conversation is still
                    // wedged, so removing the only route back would be a lie.
                    w.set_toast_message(
                        chatter_i18n::tr(
                            "Could not accept the new safety number — the conversation is still \
                             blocked.",
                        )
                        .into(),
                    );
                    w.set_toast_variant("error".into());
                    w.set_toast_visible(true);
                }
            }
        });
}

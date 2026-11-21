//! Content another app shares into Chatter (the system share sheet).
//!
//! `MainActivity.dispatchShare` copies the shared streams into the cache dir
//! and calls [`nativeShareReceived`]. The payload is untrusted data and never
//! consent to send: it waits here until the account is ready, the destination
//! picker opens, and only the user's choice of chat stages it — text into that
//! chat's composer, files into its attachment tray — for the user's own Send.
//! A share started from one of our Direct Share chats (`share_targets`) names
//! its chat up front: when core resolves that key the picker is skipped, and the
//! share is staged the same way.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use chatter_core::client::ChatterClient;
use chatter_core::state::ActiveConversation;
use jni::objects::{JObject, JObjectArray, JString};
use slint::ComponentHandle;

struct IncomingShare {
    text: String,
    files: Vec<PathBuf>,
    /// The opaque key of a tapped Direct Share chat; empty for a plain share.
    /// Untrusted until core resolves it.
    shortcut: String,
}

impl IncomingShare {
    /// What the picker shows above the destination list.
    fn preview(&self) -> String {
        let mut out: String = self.text.trim().chars().take(500).collect();
        if !self.files.is_empty() {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&chatter_i18n::trn(
                "{n} file",
                "{n} files",
                self.files.len() as u64,
                &[],
            ));
        }
        out
    }

    /// Delete the cache copies (file + its per-selection dir, as the picker does).
    fn discard(self) {
        for file in self.files {
            let _ = std::fs::remove_file(&file);
            if let Some(parent) = file.parent() {
                let _ = std::fs::remove_dir(parent);
            }
        }
    }
}

/// Received, picker not yet shown (no signed-in main view to show it in).
static WAITING: Mutex<Option<IncomingShare>> = Mutex::new(None);
/// Picker open: held until the user picks a chat. `stage` claims it; if the
/// picker leaves the modal stack unclaimed (cancel, Back, reset) `discard_offered`
/// deletes the cache copies at once.
static OFFERED: Mutex<Option<IncomingShare>> = Mutex::new(None);

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_chatter_client_MainActivity_nativeShareReceived(
    mut env: jni::EnvUnowned<'_>,
    _this: JObject,
    text: JString,
    paths: JObjectArray,
    shortcut: JString,
) {
    env.with_env(|env| -> jni::errors::Result<_> {
        let text = text
            .try_to_string(&env)
            .map(String::from)
            .unwrap_or_default();
        let files = super::file_picker::read_string_array(env, &paths)
            .unwrap_or_default()
            .into_iter()
            .map(PathBuf::from)
            .collect();
        let shortcut = shortcut
            .try_to_string(&env)
            .map(String::from)
            .unwrap_or_default();
        // A newer share supersedes an unanswered one.
        for slot in [&WAITING, &OFFERED] {
            if let Some(old) = slot.lock().ok().and_then(|mut slot| slot.take()) {
                old.discard();
            }
        }
        if let Ok(mut waiting) = WAITING.lock() {
            *waiting = Some(IncomingShare {
                text,
                files,
                shortcut,
            });
        }
        super::notification_route::open_pending_soon();
        Ok(())
    })
    .resolve::<jni::errors::LogErrorAndDefault>()
}

/// Route a waiting share: straight into the chat of a tapped Direct Share
/// target when core resolves its key, otherwise through the destination
/// picker. Called on the UI thread once the account is signed in and on the
/// main view; a no-op otherwise.
pub fn open_pending(window: &crate::MainWindow, client: &Arc<ChatterClient>) {
    let Some(share) = WAITING.lock().ok().and_then(|mut slot| slot.take()) else {
        return;
    };
    if chatter_core::ui::is_opaque_notification_key(&share.shortcut) {
        if let Some(target) = client.conversation_for_share_target(&share.shortcut) {
            // Nothing else is on screen to dismiss but stray layers.
            crate::bin_ui::features::back::reset_to_chats(window);
            stage_into(window, client, target, share);
            return;
        }
    }
    let preview = share.preview();
    if let Ok(mut offered) = OFFERED.lock() {
        *offered = Some(share);
    }
    crate::bin_ui::features::chat::open_share_in_modal(window, client, preview);
}

/// The picker closed without a choice: delete the unclaimed cache copies.
/// Called by the modal stack whenever the forward picker leaves it.
pub fn discard_offered() {
    if let Some(share) = OFFERED.lock().ok().and_then(|mut slot| slot.take()) {
        share.discard();
    }
}

/// The user picked `target` in the picker: close it, open that chat and stage
/// the share into its composer. Nothing is sent.
pub fn stage(window: &crate::MainWindow, client: &Arc<ChatterClient>, target: ActiveConversation) {
    let Some(share) = OFFERED.lock().ok().and_then(|mut slot| slot.take()) else {
        return;
    };
    // Claimed above, so closing the picker no longer discards it.
    crate::bin_ui::features::back::close(window, "forward-message");
    stage_into(window, client, target, share);
}

/// Open `target` and stage `share` into its composer and tray.
fn stage_into(
    window: &crate::MainWindow,
    client: &Arc<ChatterClient>,
    target: ActiveConversation,
    share: IncomingShare,
) {
    crate::bin_ui::features::chat::open::select_conversation(window, target);
    // The open commits synchronously (active conversation and its draft), so
    // what follows lands in the chosen chat.
    if client.active_conversation() != target {
        tracing::warn!("share-in: chat did not open; dropping the share");
        share.discard();
        return;
    }
    if !share.text.is_empty() {
        let pane = window.global::<crate::ChatPane>();
        let draft = pane.get_composer_text();
        let text = if draft.is_empty() {
            share.text.clone()
        } else {
            format!("{draft}\n{}", share.text)
        };
        pane.set_composer_text(text.into());
        pane.set_composer_caret_end_seq(pane.get_composer_caret_end_seq() + 1);
    }
    if share.files.is_empty() {
        return;
    }
    let snapshot = client.chat_snapshot();
    let (revision, session) = (snapshot.attachment_revision, client.session_guard());
    let (client, weak) = (client.clone(), window.as_weak());
    tokio::spawn(async move {
        crate::bin_ui::features::composer::stage_owned_files(
            share.files,
            &client,
            target,
            revision,
            session,
            &weak,
        )
        .await;
    });
}

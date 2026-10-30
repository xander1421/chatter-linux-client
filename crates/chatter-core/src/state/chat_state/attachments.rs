use std::path::PathBuf;

use crate::domain::ids::{AttachmentId, ChannelId, MessageId};

use super::{ActiveConversation, ChatState, ChatStateData};

/// A pending file attachment (uploaded but not yet sent).
#[derive(Clone, Debug)]
pub struct PendingAttachment {
    /// The attachment ID from the server after upload.
    pub id: AttachmentId,
    /// Original filename.
    pub filename: String,
    /// Original file path (for display/reference).
    pub path: PathBuf,
    /// File size in bytes.
    pub size: u64,
    /// MIME type.
    pub mime_type: String,
    /// Pre-uploaded attachment pointer (for GIFs from picker).
    /// If set, the attachment is already uploaded and this pointer should be used directly.
    pub pointer: Option<crate::proto::content_v1::AttachmentPointer>,
}

impl PendingAttachment {
    /// Create a new pending attachment (will be uploaded when sent).
    #[must_use]
    pub fn new(
        id: AttachmentId,
        filename: String,
        path: PathBuf,
        size: u64,
        mime_type: String,
    ) -> Self {
        Self {
            id,
            filename,
            path,
            size,
            mime_type,
            pointer: None,
        }
    }

    /// Create a pending attachment with a pre-uploaded pointer.
    /// Used for GIFs from the picker that are already uploaded to CDN.
    #[must_use]
    pub fn with_pointer(
        id: AttachmentId,
        path: PathBuf,
        filename: String,
        size: u64,
        mime_type: String,
        pointer: crate::proto::content_v1::AttachmentPointer,
    ) -> Self {
        Self {
            id,
            filename,
            path,
            size,
            mime_type,
            pointer: Some(pointer),
        }
    }
}

impl ChatState {
    pub fn thread_pending_attachments(&self, root: &MessageId) -> Vec<PendingAttachment> {
        self.store.with(|s| s.thread_attachment_trays.get(root).cloned().unwrap_or_default())
    }

    pub fn add_thread_attachment_for(
        &self,
        root: MessageId,
        revision: u64,
        attachment: PendingAttachment,
    ) {
        self.store.update(|state| {
            if state.attachment_revision == revision {
                state.thread_attachment_trays.entry(root).or_default().push(attachment);
            }
        });
    }

    pub fn take_thread_attachments_for(
        &self,
        root: MessageId,
        revision: u64,
    ) -> Option<Vec<PendingAttachment>> {
        let mut taken = None;
        self.store.update(|state| {
            if state.attachment_revision == revision
                && state.open_threads.iter().any(|thread| thread.root_message_id == root)
            {
                taken = Some(state.thread_attachment_trays.remove(&root).unwrap_or_default());
            }
        });
        taken
    }

    pub fn remove_thread_attachment(&self, root: MessageId, id: AttachmentId) {
        self.store.update(|state| {
            if let Some(tray) = state.thread_attachment_trays.get_mut(&root) {
                tray.retain(|attachment| attachment.id != id);
                if tray.is_empty() { state.thread_attachment_trays.remove(&root); }
            }
        });
    }

    pub fn take_thread_attachment_trays_for(&self, roots: &[MessageId]) -> Vec<PendingAttachment> {
        let mut taken = Vec::new();
        self.store.update(|state| {
            for root in roots {
                if let Some(mut attachments) = state.thread_attachment_trays.remove(root) {
                    taken.append(&mut attachments);
                }
            }
        });
        taken
    }

    pub(super) fn switch_attachment_tray(state: &mut ChatStateData, entering: Option<ChannelId>) {
        if state.active_channel_id == entering {
            return;
        }
        let leaving = std::mem::take(&mut state.pending_attachments);
        if !leaving.is_empty() {
            state
                .attachment_trays
                .insert(state.active_channel_id, leaving);
        }
        state.pending_attachments = state.attachment_trays.remove(&entering).unwrap_or_default();
    }

    /// Clear every account-local tray during session reset.
    pub fn clear_attachment_trays(&self) {
        self.store.update(|state| {
            state.pending_attachments.clear();
            state.attachment_trays.clear();
            state.thread_attachment_trays.clear();
            state.attachment_revision = state.attachment_revision.wrapping_add(1);
        });
    }

    /// Remove a deleted conversation's tray, including the active tray.
    pub fn take_attachment_tray_for(
        &self,
        conversation: ActiveConversation,
    ) -> Vec<PendingAttachment> {
        let key = conversation.channel_id();
        let mut taken = Vec::new();
        self.store.update(|state| {
            if state.active_conversation == conversation && state.active_channel_id == key {
                taken = std::mem::take(&mut state.pending_attachments);
            } else if let Some(tray) = state.attachment_trays.remove(&key) {
                taken = tray;
            }
        });
        taken
    }

    /// Stage an asynchronous selection under the conversation that requested it.
    pub fn add_pending_attachment_for(
        &self,
        conversation: ActiveConversation,
        revision: u64,
        attachment: PendingAttachment,
    ) {
        self.store.update(|state| {
            if state.attachment_revision != revision {
                return;
            }
            let key = conversation.channel_id();
            if state.active_channel_id == key {
                state.pending_attachments.push(attachment);
            } else {
                state
                    .attachment_trays
                    .entry(key)
                    .or_default()
                    .push(attachment);
            }
        });
    }

    /// Add a pending attachment for the current message.
    pub fn add_pending_attachment(&self, attachment: PendingAttachment) {
        self.store.update(|state| {
            state.pending_attachments.push(attachment);
        });
    }

    /// Get all pending attachments.
    #[must_use]
    pub fn get_pending_attachments(&self) -> Vec<PendingAttachment> {
        self.store.with(|s| s.pending_attachments.clone())
    }

    /// Take the staged attachments for a send to `conversation`, in one
    /// update: `None`, with nothing taken, when another conversation has
    /// become active or the account-local attachment revision has changed.
    /// Each conversation owns its tray; a stale send cannot consume new-account files.
    pub fn take_pending_attachments_for(
        &self,
        conversation: ActiveConversation,
        revision: u64,
    ) -> Option<Vec<PendingAttachment>> {
        let mut taken = None;
        self.store.update(|state| {
            if state.active_conversation == conversation
                && state.active_channel_id == conversation.channel_id()
                && state.attachment_revision == revision
            {
                taken = Some(std::mem::take(&mut state.pending_attachments));
            }
        });
        taken
    }

    /// Clear all pending attachments (called after message is sent).
    pub fn clear_pending_attachments(&self) {
        self.store.update(|state| {
            state.pending_attachments.clear();
        });
    }

    /// Remove a pending attachment by its ID.
    pub fn remove_pending_attachment(&self, attachment_id: AttachmentId) {
        self.store.update(|state| {
            state.pending_attachments.retain(|a| a.id != attachment_id);
        });
    }
}

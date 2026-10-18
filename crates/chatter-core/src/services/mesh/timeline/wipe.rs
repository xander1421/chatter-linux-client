use super::*;

impl MeshTimeline {
    /// Panic wipe: destroy every mesh message and forget every conversation.
    ///
    /// Deletes by conversation *type*, not by walking the open directory.
    /// Leaving a geohash cell keeps its history by design, so a wipe driven from
    /// the sidebar would silently spare everything said in every cell the user
    /// ever left — and a panic wipe that leaves messages behind is worse than no
    /// panic wipe, because the user believes it worked.
    ///
    /// Both conversation types are cleared even if one fails, and the first
    /// error is returned afterwards: stopping at the first failure would leave
    /// the rest on disk, which is the worst outcome for the one operation whose
    /// entire purpose is leaving nothing behind.
    pub fn wipe(&self) -> Result<(), StorageError> {
        self.nearby_channels.lock().clear();
        self.open.lock().clear();

        // Partial chunked transfers, before anything else. They have no
        // attachment row, so nothing below would ever reach them — and unlike a
        // finished attachment they are *plaintext* on disk, which makes them the
        // most exposed thing the wipe has to destroy.
        super::super::file_stream::clear_transfers_dir();

        let mut first_error = None;
        for conversation_type in [ConversationType::Mesh, ConversationType::MeshDm] {
            // Attachments first, and *before* the messages that name them.
            //
            // A mesh file's bytes live outside the database — encrypted, under a
            // key held only in its `attachments` row — so deleting the messages
            // first would orphan both the row and the file with nothing left
            // pointing at them. Dropping the row alone would crypto-shred the
            // blob, which protects the contents, but a panic wipe is not only
            // about contents: a megabyte left on disk is proof that something was
            // received, and that is exactly what the user reached for this button
            // to destroy.
            match self.messages.list_by_conversation_type(&conversation_type) {
                Ok(messages) => {
                    for attachment in messages
                        .iter()
                        .flat_map(super::super::mapping::attachments_of)
                    {
                        // Best effort per attachment, and deliberately no early
                        // return: one file that refuses to unlink must not spare
                        // every file after it.
                        let path = crate::services::attachment::attachment_file_path(attachment);
                        if let Err(error) = std::fs::remove_file(&path)
                            && error.kind() != std::io::ErrorKind::NotFound
                        {
                            tracing::warn!(
                                target: "mesh",
                                %error,
                                path = %path.display(),
                                "a mesh attachment file could not be removed"
                            );
                        }
                        if let Err(error) = self.attachments.delete(attachment) {
                            tracing::warn!(
                                target: "mesh",
                                %error,
                                "a mesh attachment row could not be removed"
                            );
                        }
                    }
                }
                Err(error) => {
                    tracing::error!(
                        target: "mesh",
                        %error,
                        "mesh messages could not be listed; their attachments may survive the wipe"
                    );
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
            }

            if let Err(error) = self
                .messages
                .delete_by_conversation_type(&conversation_type)
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        if let Err(error) = self.directory.wipe()
            && first_error.is_none()
        {
            first_error = Some(error);
        }
        if let Err(error) = self.delivery.wipe()
            && first_error.is_none()
        {
            first_error = Some(error);
        }

        // The room is not something you join, so it comes back immediately —
        // an empty room, with nothing in it.
        self.register(&MeshConversation::Room, None);

        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

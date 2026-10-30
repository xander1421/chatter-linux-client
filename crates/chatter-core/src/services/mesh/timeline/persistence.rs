use super::*;

impl MeshTimeline {
    /// Persist one mesh message, and remember the conversation it belongs to.
    ///
    /// Returns whether the row was new. Idempotent on message ID, which is what
    /// makes the mesh's redundancy free: the same message legitimately arrives
    /// over the radio, from a courier, and from a relay drop, and
    /// [`wire_message_id`](super::super::mapping::wire_message_id) maps all three onto
    /// the same row. Writing it three times would turn the redundancy that
    /// makes delivery work into three bubbles in the timeline.
    ///
    /// # This is a deliberate divergence from bitchat
    ///
    /// bitchat's whitepaper states ephemerality as a design goal: no plaintext
    /// message content is ever written to disk, and everything its
    /// store-and-forward stack persists is either sealed ciphertext or
    /// already-public broadcast traffic. The line below writes mesh plaintext to
    /// disk, and does so knowingly:
    ///
    /// - **Selective ephemerality is a half-measure.** Mesh messages evaporating
    ///   while the user's Signal messages sit in the same database offers a
    ///   feeling of safety rather than safety, which is worse than either
    ///   consistent option.
    /// - **A chat that silently forgets its history is indistinguishable from a
    ///   bug** to everyone who has not read the whitepaper.
    /// - **The row lands in SQLCipher under the OS-keyring passphrase**, the
    ///   same protection every other message in this app gets. It is not a
    ///   weaker store; it is the same store.
    ///
    /// The consequence is not optional: a user who reaches for the mesh in the
    /// situations bitchat targets is relying on being able to destroy this, so
    /// panic wipe has to reach these rows. Reversing the decision means
    /// confining mesh messages to an in-memory timeline and losing history
    /// across restarts — not weakening what is written here.
    pub fn record(&self, event: &MeshMessage) -> Result<bool, StorageError> {
        let mut row = event.message.clone();
        // Our own message is read the moment it is composed. Leaving it unread
        // would put an unread badge on the conversation the user just typed in.
        row.is_read = event.is_local;
        // Deliberately left empty. `delivery_status` carries server semantics —
        // "sent" there means a server accepted it — and a mesh message handed to
        // a courier is genuinely sent while no server has ever seen it. Mesh
        // delivery has its own vocabulary in [`MeshDelivery`](super::MeshDelivery)
        // and rides the event stream, rather than being flattened into a field
        // that means something else.
        row.delivery_status.clear();

        // Plaintext, to disk, on purpose. See "This is a deliberate divergence
        // from bitchat" above before changing it.
        let stored = self.messages.insert_or_ignore(&row)?;

        // A message proves the conversation exists. Only a message we received
        // carries a nickname worth labelling a DM with.
        let nickname = (!event.is_local && matches!(&event.conversation, MeshConversation::Dm(_)))
            .then(|| event.nickname.as_str());
        self.register(&event.conversation, nickname);

        // And names its sender for as long as the row exists, not just while
        // the peer is in range — otherwise history reads "Unknown peer".
        if !event.is_local
            && let Some(sender) = event.message.sender_id.as_ref()
            && let Err(error) = self.directory.remember_sender(
                &sender.to_string(),
                event.sender.as_str(),
                &event.nickname,
            )
        {
            tracing::warn!(%error, "mesh: could not remember a sender's nickname");
        }

        Ok(stored)
    }

    /// The nickname a sender last used, as remembered by [`record`](Self::record).
    pub fn remembered_sender_name(&self, sender: &UserId) -> Option<String> {
        self.directory
            .sender_name(&sender.to_string())
            .inspect_err(|error| tracing::warn!(%error, "mesh: could not read a sender's nickname"))
            .ok()
            .flatten()
    }

    /// Whether a message with this wire ID is already on disk.
    ///
    /// The file path needs this and the text path does not. [`record`](Self::record)
    /// is idempotent on the message ID, so a text message that arrives twice
    /// collapses onto its row for free — but a file's bytes are stored *before*
    /// the row exists, and the gossip archive exists precisely to hand public
    /// packets back long after the dedup window that would have caught them on
    /// the radio. Storing first and letting the row be ignored would leave an
    /// encrypted copy on disk that nothing references, and emit an event naming
    /// an attachment the stored row does not have.
    pub fn already_stored(&self, wire_id: &str) -> Result<bool, StorageError> {
        Ok(self
            .messages
            .get(&super::super::mapping::wire_message_id(wire_id))?
            .is_some())
    }

    /// Persist one mesh file, and hand back the id a message can carry it by.
    ///
    /// Used by both directions: a file that arrived over the radio, and the
    /// sender's own copy of one it just put on the air. Neither has a server
    /// behind it, so `remote_url` stays `None` — a mesh attachment that claimed
    /// a CDN key would join the download queue and be retried forever against a
    /// CDN that never had it.
    ///
    /// A mesh file is stored exactly as a locally-composed attachment is:
    /// encrypted under a fresh per-file key, written to [`attachment_file_path`],
    /// and recorded with the key and digest that the existing read path decrypts
    /// with (`services::attachment::crypto` only opens `OnFilesystem` rows).
    ///
    /// The encryption is not theatre either. The bytes land outside SQLCipher,
    /// so this is what keeps a mesh file at parity with the messages beside it
    /// rather than lying in the open next to an encrypted database.
    pub fn store_file(
        &self,
        file_name: Option<&str>,
        mime_type: Option<&str>,
        bytes: &[u8],
    ) -> Result<AttachmentId, MeshError> {
        let id = AttachmentId::new();
        let encrypted = AttachmentCrypto::encrypt(bytes)?;

        ensure_attachments_dir().map_err(|source| MeshError::FileIo {
            path: attachments_dir().display().to_string(),
            source,
        })?;
        let path = attachment_file_path(&id);
        std::fs::write(&path, &encrypted.ciphertext).map_err(|source| MeshError::FileIo {
            path: path.display().to_string(),
            source,
        })?;

        let mut metadata = FileMetadata::new(
            display_name(file_name),
            mime_type.unwrap_or(DEFAULT_MIME).to_string(),
            bytes.len() as u64,
        );
        // The digest over the ciphertext, matching what the CDN upload path
        // stores here — the read path verifies against `encryption_iv`, and
        // leaving this empty would make a mesh attachment the only one whose
        // metadata cannot be checked against its bytes.
        metadata.sha256 = encrypted.digest.to_vec();

        let attachment = Attachment {
            id,
            metadata,
            status: AttachmentStatus::Ready,
            content_location: ContentLocation::OnFilesystem,
            remote_url: None,
            encryption_key: Some(encrypted.key.to_vec()),
            encryption_iv: Some(encrypted.digest.to_vec()),
            created_at: Utc::now(),
            secret_chat: false,
        };
        if let Err(error) = self.attachments.insert_with_file_path(&attachment, &path) {
            // The row holds the only copy of the key, so a ciphertext left
            // behind after it fails to insert is an orphan nothing can ever open
            // or name.
            let _ = std::fs::remove_file(&path);
            return Err(error.into());
        }
        Ok(id)
    }

    /// Persist one mesh file that is already on disk, without reading it into
    /// memory.
    ///
    /// The streaming counterpart of [`store_file`](Self::store_file), and the
    /// call the chunked receive path ends with. Same row, same encryption, same
    /// place on disk — the only difference is that the plaintext arrives as a
    /// path rather than a `&[u8]`, so `encrypt_file_to_file` can do the work in
    /// 256 KiB windows instead of the caller holding the whole file.
    ///
    /// This is the same function `services::attachment::upload` uses for the
    /// server flow. Using it here is the point of P9: the mesh path and the CDN
    /// path now have identical memory behaviour, so a 16 MiB mesh file costs what
    /// a 16 MiB upload costs.
    ///
    /// `plaintext` is **not** removed. The caller owns it — for a received
    /// transfer it is the spool's partial file, which is discarded together with
    /// the bitmap; for a send it is the user's own file, which must obviously
    /// survive.
    pub fn store_file_streamed(
        &self,
        file_name: Option<&str>,
        mime_type: Option<&str>,
        plaintext: &std::path::Path,
    ) -> Result<AttachmentId, MeshError> {
        let id = AttachmentId::new();

        ensure_attachments_dir().map_err(|source| MeshError::FileIo {
            path: attachments_dir().display().to_string(),
            source,
        })?;
        let path = attachment_file_path(&id);
        let encrypted =
            AttachmentCrypto::encrypt_file_to_file(plaintext, &path).map_err(|error| {
                // The output file is opened before the encryption can fail, so a
                // failure leaves a partial ciphertext with no row and no key: an
                // orphan nothing can open or name.
                let _ = std::fs::remove_file(&path);
                error
            })?;

        let mut metadata = FileMetadata::new(
            display_name(file_name),
            mime_type.unwrap_or(DEFAULT_MIME).to_string(),
            encrypted.plaintext_size,
        );
        // Over the ciphertext, matching `store_file` and the CDN upload path: the
        // read path verifies against `encryption_iv`.
        metadata.sha256 = encrypted.digest.to_vec();

        let attachment = Attachment {
            id,
            metadata,
            status: AttachmentStatus::Ready,
            content_location: ContentLocation::OnFilesystem,
            remote_url: None,
            encryption_key: Some(encrypted.key.to_vec()),
            encryption_iv: Some(encrypted.digest.to_vec()),
            created_at: Utc::now(),
            secret_chat: false,
        };
        if let Err(error) = self.attachments.insert_with_file_path(&attachment, &path) {
            let _ = std::fs::remove_file(&path);
            return Err(error.into());
        }
        Ok(id)
    }
}

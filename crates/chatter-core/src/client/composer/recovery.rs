//! Staged files and persisted failed-send recovery.

use super::*;

// A blocking job may outlive its awaiting future. Unclaimed staging output
// must be deleted when Tokio drops that job's result after cancellation.
struct PreparedOwnedFile {
    client: std::sync::Arc<ChatterClient>,
    path: Option<PathBuf>,
}
impl Drop for PreparedOwnedFile {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            self.client.release_staged_files(&[PendingAttachment::new(
                AttachmentId::new(),
                String::new(),
                path,
                0,
                String::new(),
            )]);
        }
    }
}

impl ChatterClient {
    // ── Recovery: staged files and parked sends ─────────────────────

    pub(super) fn composer_recovery_key(owner: UserId) -> String {
        format!("ui.composer_recovery.{}", owner.0)
    }

    pub(super) fn staging_root(&self) -> PathBuf {
        self.directories().data_dir().join("composer-staging")
    }

    fn staged_upload_temp_root() -> PathBuf {
        crate::services::attachment::attachments_dir().join("staged-upload-tmp")
    }

    /// Upload plaintext is only needed for one send. A process death skips
    /// TempDir's destructor, so remove any leftovers before a client can send.
    pub(in crate::client) fn cleanup_staged_upload_temps(&self) -> std::io::Result<()> {
        let root = Self::staged_upload_temp_root();
        if root.exists() {
            std::fs::remove_dir_all(&root)?;
        }
        Self::ensure_staged_upload_temp_root()?;
        Ok(())
    }

    fn ensure_staged_upload_temp_root() -> std::io::Result<PathBuf> {
        crate::services::attachment::ensure_attachments_dir()?;
        let root = Self::staged_upload_temp_root();
        std::fs::create_dir_all(&root)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(root)
    }

    fn staging_dir(&self, owner: UserId) -> PathBuf {
        self.staging_root().join(owner.0.to_string())
    }

    fn staging_files(&self, owner: UserId) -> Vec<PathBuf> {
        let Ok(dirs) = std::fs::read_dir(self.staging_dir(owner)) else {
            return Vec::new();
        };
        dirs.flatten()
            .flat_map(|dir| {
                std::fs::read_dir(dir.path())
                    .into_iter()
                    .flatten()
                    .flatten()
                    .map(|entry| entry.path())
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    pub(super) fn wipe_staging_dir(&self, owner: UserId) {
        for path in self.staging_files(owner) {
            if let Some(key) = self.staging_crypto_key(&path) {
                let _ = self.settings_repo().delete(&key);
            }
        }
        let _ = std::fs::remove_dir_all(self.staging_dir(owner));
    }

    /// A clean break from the former plaintext layout: only files whose
    /// encryption metadata is readable in SQLCipher may survive startup.
    pub(in crate::client) fn discard_legacy_staging_all(&self) {
        let Ok(accounts) = std::fs::read_dir(self.staging_root()) else {
            return;
        };
        for account in accounts.flatten() {
            let Ok(dirs) = std::fs::read_dir(account.path()) else {
                continue;
            };
            for dir in dirs.flatten() {
                let Ok(files) = std::fs::read_dir(dir.path()) else {
                    continue;
                };
                for file in files.flatten() {
                    let path = file.path();
                    let Some(key) = self.staging_crypto_key(&path) else {
                        continue;
                    };
                    if matches!(self.settings_repo().get::<StagingCrypto>(&key), Ok(None)) {
                        let _ = std::fs::remove_file(&path);
                        let _ = std::fs::remove_dir(dir.path());
                    }
                }
            }
        }
    }

    fn staging_crypto_key(&self, path: &std::path::Path) -> Option<String> {
        let relative = path.strip_prefix(self.staging_root()).ok()?;
        let mut parts = relative.components();
        let std::path::Component::Normal(owner) = parts.next()? else {
            return None;
        };
        let std::path::Component::Normal(token) = parts.next()? else {
            return None;
        };
        let std::path::Component::Normal(_) = parts.next()? else {
            return None;
        };
        if parts.next().is_some() {
            return None;
        }
        let owner = owner.to_str()?;
        let token = token.to_str()?;
        Some(format!("ui.composer_staging.{owner}.{token}"))
    }

    /// Return the real size for a staged ciphertext file, for the tray UI.
    pub fn staged_file_size(&self, path: &std::path::Path) -> Option<u64> {
        let key = self.staging_crypto_key(path)?;
        Some(
            self.settings_repo()
                .get::<StagingCrypto>(&key)
                .ok()??
                .plaintext_size,
        )
    }

    /// Prepare file display metadata off the caller's executor. Publication is
    /// still fenced by the selection's account-local attachment revision.
    pub async fn prepare_pending_file(
        self: &std::sync::Arc<Self>,
        path: PathBuf,
        staged_size: Option<u64>,
        conversation: ActiveConversation,
        revision: u64,
    ) -> Result<(), String> {
        let client = self.clone();
        let mut owned = PreparedOwnedFile {
            client: client.clone(),
            path: Some(path.clone()),
        };
        crate::services::attachment::run_attachment_work(move || {
            let metadata = std::fs::metadata(&path)
                .map_err(|e| format!("Failed to read attachment metadata: {e}"))?;
            let filename = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown")
                .to_string();
            let mime = mime_guess::from_path(&path)
                .first_or_octet_stream()
                .to_string();
            let size = staged_size
                .or_else(|| client.staged_file_size(&path))
                .unwrap_or(metadata.len());
            let attachment =
                PendingAttachment::new(AttachmentId::new(), filename, path, size, mime);
            if client.add_pending_attachment_for(conversation, revision, attachment) {
                owned.path.take();
            }
            Ok(())
        })
        .await
        .map_err(|e| format!("Attachment preparation failed: {e}"))?
    }

    /// Bound pixels retained by the tray model as well as decoder concurrency:
    /// at most 64 x 160 x 160 x RGBA = 6.25 MiB, regardless of batch size.
    /// Remaining selections still appear, validate and send, with file icons.
    pub fn pending_preview_files(
        attachments: &[PendingAttachment],
    ) -> impl Iterator<Item = (usize, &PendingAttachment)> {
        attachments
            .iter()
            .enumerate()
            .filter(|(_, attachment)| attachment.mime_type.starts_with("image/"))
            .take(64)
    }

    /// Read/decode a tray preview only on the bounded attachment worker. This
    /// skips the inline-thumbnail WebP encode/decode round trip and Lanczos pass.
    pub fn pending_image_pixels(&self, path: &std::path::Path) -> Option<image::RgbaImage> {
        let bytes = if self.staging_crypto_key(path).is_some() {
            self.staged_image_plaintext(path)?
        } else {
            if std::fs::metadata(path).ok()?.len() > 10 * 1024 * 1024 {
                return None;
            }
            use std::io::Read;
            let mut bytes = zeroize::Zeroizing::new(Vec::new());
            std::fs::File::open(path)
                .ok()?
                .take(10 * 1024 * 1024 + 1)
                .read_to_end(&mut bytes)
                .ok()?;
            if bytes.len() > 10 * 1024 * 1024 {
                return None;
            }
            bytes
        };
        crate::media::ImageProcessor::tray_preview_from_memory(&bytes).ok()
    }

    /// Stage app-owned files without running encryption on the caller's executor.
    pub async fn stage_owned_file_async(
        self: &std::sync::Arc<Self>,
        path: PathBuf,
    ) -> Result<PathBuf, String> {
        let client = self.clone();
        let session = self.session_guard();
        crate::services::attachment::run_attachment_work(move || {
            if !session.is_current() {
                return PreparedOwnedFile {
                    client,
                    path: Some(path),
                };
            }
            let staged = client.stage_owned_file(path.clone());
            if !session.is_current() && staged != path {
                client.release_staged_files(&[PendingAttachment::new(
                    AttachmentId::new(),
                    String::new(),
                    staged,
                    0,
                    String::new(),
                )]);
                return PreparedOwnedFile {
                    client,
                    path: Some(path),
                };
            }
            PreparedOwnedFile {
                client,
                path: Some(staged),
            }
        })
        .await
        .map(|mut staged| staged.path.take().unwrap())
        .map_err(|e| format!("Attachment staging failed: {e}"))
    }

    fn staged_image_plaintext(
        &self,
        path: &std::path::Path,
    ) -> Option<zeroize::Zeroizing<Vec<u8>>> {
        const MAX_PREVIEW_INPUT: u64 = 10 * 1024 * 1024;
        let key_name = self.staging_crypto_key(path)?;
        let crypto: StagingCrypto = self.settings_repo().get(&key_name).ok()??;
        if crypto.plaintext_size > MAX_PREVIEW_INPUT {
            return None;
        }
        let key: [u8; 64] = crypto.key.try_into().ok()?;
        let digest: [u8; 32] = crypto.digest.try_into().ok()?;
        let source = std::fs::File::open(path).ok()?;
        let size = source.metadata().ok()?.len();
        if size > crate::services::attachment::image_preview_ciphertext_limit() {
            return None;
        }
        let mut decryptor = crate::crypto::streaming_decrypt::StreamingDecryptor::new(
            std::io::BufReader::new(source),
            &key,
            &digest,
            crypto.plaintext_size.try_into().ok()?,
            size.try_into().ok()?,
        );
        Some(zeroize::Zeroizing::new(decryptor.decrypt_all().ok()?))
    }

    fn restorable_staged_file(&self, path: &std::path::Path) -> bool {
        let Some(key) = self.staging_crypto_key(path) else {
            return true;
        };
        !matches!(self.settings_repo().get::<StagingCrypto>(&key), Ok(None))
    }

    /// Verify and decrypt a staged file for the duration of one upload.
    /// The existing attachment uploader and media sanitizer require a file path;
    /// its private temporary plaintext file is removed when the send returns.
    pub(crate) async fn staged_upload_file(
        &self,
        path: &std::path::Path,
    ) -> Result<Option<(tempfile::TempDir, PathBuf)>, crate::services::attachment::AttachmentError>
    {
        use crate::services::attachment::AttachmentError;
        let Some(key_name) = self.staging_crypto_key(path) else {
            return Ok(None);
        };
        let crypto: StagingCrypto = self
            .settings_repo()
            .get(&key_name)
            .map_err(|e| AttachmentError::StorageError(e.to_string()))?
            .ok_or_else(|| {
                AttachmentError::DecryptionError("staged file has no encryption key".into())
            })?;
        let path = path.to_path_buf();
        crate::services::attachment::run_attachment_work(move || {
            let key: [u8; 64] = crypto
                .key
                .try_into()
                .map_err(|_| AttachmentError::DecryptionError("invalid staged file key".into()))?;
            let digest: [u8; 32] = crypto.digest.try_into().map_err(|_| {
                AttachmentError::DecryptionError("invalid staged file digest".into())
            })?;
            let source = std::fs::File::open(&path)
                .map_err(|e| AttachmentError::FileError(e.to_string()))?;
            let size = source
                .metadata()
                .map_err(|e| AttachmentError::FileError(e.to_string()))?
                .len() as usize;
            let mut decryptor = crate::crypto::streaming_decrypt::StreamingDecryptor::new(
                std::io::BufReader::new(source),
                &key,
                &digest,
                crypto.plaintext_size as usize,
                size,
            );
            decryptor
                .verify()
                .map_err(|e| AttachmentError::DecryptionError(e.to_string()))?;
            decryptor
                .init_decryption()
                .map_err(|e| AttachmentError::DecryptionError(e.to_string()))?;
            let root = Self::ensure_staged_upload_temp_root()
                .map_err(|e| AttachmentError::FileError(e.to_string()))?;
            let temp = tempfile::Builder::new()
                .prefix("upload-")
                .tempdir_in(root)
                .map_err(|e| AttachmentError::FileError(e.to_string()))?;
            let output_path = temp.path().join(
                path.file_name()
                    .ok_or_else(|| AttachmentError::FileError("staged filename missing".into()))?,
            );
            let mut output = std::fs::File::create(&output_path)
                .map_err(|e| AttachmentError::FileError(e.to_string()))?;
            let mut chunk = vec![0u8; 512 * 1024];
            loop {
                let read = decryptor
                    .read_decrypted(&mut chunk)
                    .map_err(|e| AttachmentError::DecryptionError(e.to_string()))?;
                if read == 0 {
                    break;
                }
                output
                    .write_all(&chunk[..read])
                    .map_err(|e| AttachmentError::FileError(e.to_string()))?;
            }
            drop(output);
            Ok(Some((temp, output_path)))
        })
        .await
        .map_err(|e| AttachmentError::FileError(format!("Preparation task failed: {e}")))?
    }

    /// The account whose recovery row may be written: the drafts' owner,
    /// only while the session that loaded it is the current one. `None`
    /// between a sign-out and the next composer use, so nothing of the old
    /// account lands under the new one.
    pub(super) fn recovery_owner(&self) -> Option<UserId> {
        let auth = self.auth_state().get();
        if *self.composer_draft_revision.lock() != auth.session_revision {
            return None;
        }
        if self
            .composer_recovery_unread
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            // The row could not be read this session: whatever it holds
            // must not be written over. See `restore_composer_recovery`.
            return None;
        }
        let owner = *self.composer_draft_owner.lock();
        owner.filter(|owner| Some(*owner) == auth.current_user_id)
    }

    /// Encrypt an app-owned picker or paste copy under the signed-in account.
    /// The random file key is stored in SQLCipher before ciphertext is published.
    /// A failed stage returns the original path; callers must reject that path.
    pub fn stage_owned_file(&self, path: PathBuf) -> PathBuf {
        let (_drafts, _) = self.composer_drafts_for_session();
        let Some(owner) = self.recovery_owner() else {
            return path;
        };
        let Some(name) = path.file_name() else {
            return path;
        };
        let dir = self
            .staging_dir(owner)
            .join(uuid::Uuid::new_v4().to_string());
        if std::fs::create_dir_all(&dir).is_err() {
            return path;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).is_err() {
                let _ = std::fs::remove_dir(&dir);
                return path;
            }
        }
        let dest = dir.join(name);
        let staging = match tempfile::NamedTempFile::new_in(&dir) {
            Ok(file) => file,
            Err(_) => {
                let _ = std::fs::remove_dir(&dir);
                return path;
            }
        };
        let encrypted =
            match crate::crypto::attachment_crypto::AttachmentCrypto::encrypt_file_to_file(
                &path,
                staging.path(),
            ) {
                Ok(result) => result,
                Err(error) => {
                    tracing::warn!(%error, "could not encrypt staged attachment");
                    drop(staging);
                    let _ = std::fs::remove_dir(&dir);
                    return path;
                }
            };
        let Some(key_name) = self.staging_crypto_key(&dest) else {
            return path;
        };
        let crypto = StagingCrypto {
            key: encrypted.key.to_vec(),
            digest: encrypted.digest.to_vec(),
            plaintext_size: encrypted.plaintext_size,
        };
        if self.settings_repo().set(&key_name, &crypto).is_err() {
            drop(staging);
            let _ = std::fs::remove_dir(&dir);
            return path;
        }
        if staging.persist(&dest).is_err() {
            let _ = self.settings_repo().delete(&key_name);
            let _ = std::fs::remove_dir_all(&dir);
            return path;
        }
        let _ = std::fs::remove_file(&path);
        // The Android picker copies each selection into a directory of its
        // own (`MainActivity.copyToCache`); it is empty now.
        if let Some(parent) = path.parent() {
            if parent
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("selection_"))
            {
                let _ = std::fs::remove_dir(parent);
            }
        }
        dest
    }

    /// Delete the staged copies behind `attachments`, once their send was
    /// taken by core or they left the tray. Only files under the staging
    /// root go; a user's original referenced by path is never touched.
    pub fn release_staged_files(&self, attachments: &[PendingAttachment]) {
        let root = self.staging_root();
        for attachment in attachments {
            let path = &attachment.path;
            if path.as_os_str().is_empty() || !path.starts_with(&root) {
                continue;
            }
            let _ = std::fs::remove_file(path);
            if let Some(key) = self.staging_crypto_key(path) {
                let _ = self.settings_repo().delete(&key);
            }
            if let Some(dir) = path.parent() {
                let _ = std::fs::remove_dir(dir);
            }
        }
    }

    /// A deleted conversation cannot leave recoverable staged copies behind.
    pub(crate) fn clear_composer_conversation(&self, conversation: ActiveConversation) {
        self.clear_composer_conversation_messages(conversation, &[]);
    }

    pub(crate) fn clear_composer_conversation_messages(
        &self,
        conversation: ActiveConversation,
        message_ids: &[MessageId],
    ) {
        let (mut drafts, _) = self.composer_drafts_for_session();
        if let Some(channel) = conversation.channel_id() {
            let key = channel.to_string();
            drafts.remove(&key);
            let mut owner = self.composer_owner.lock();
            if *owner == key {
                owner.clear();
            }
        }
        for id in message_ids {
            drafts.remove(&Self::thread_draft_key(id));
            drafts.remove(&Self::failed_thread_reply_key(id));
        }
        self.persist_composer_drafts(&drafts);
        drop(drafts);
        let tray = self.chat_state().take_attachment_tray_for(conversation);
        self.release_staged_files(&tray);
        let thread_trays = self
            .chat_state()
            .take_thread_attachment_trays_for(message_ids);
        self.release_staged_files(&thread_trays);
        let mut parked = self.parked_sends.lock();
        parked.retain(|send| {
            if send.conversation == conversation {
                self.release_staged_files(&send.attachments);
                false
            } else {
                true
            }
        });
        drop(parked);
        let _ = self.persist_composer_recovery();
    }

    /// Write the trays and the parked sends under the signed-in account.
    /// The UI's chat-state watcher calls this on every tray change; the
    /// parked-send paths call it themselves.
    pub fn persist_composer_recovery(&self) -> bool {
        let (_drafts, _) = self.composer_drafts_for_session();
        if self.auth_state().get().current_user_id.is_none() {
            return true;
        }
        self.composer_recovery_dirty.store(true, Ordering::SeqCst);
        let Some(owner) = self.recovery_owner() else {
            return false;
        };
        let parked = self.parked_sends.lock();
        self.persist_recovery_with(owner, &parked)
    }

    pub(super) fn composer_recovery_snapshot(&self, parked: &[ComposerSend]) -> ComposerRecovery {
        let chat = self.chat_state().get();
        let mut recovery = ComposerRecovery::default();
        let mut trays: Vec<(Option<ChannelId>, &[PendingAttachment])> =
            vec![(chat.active_channel_id, chat.pending_attachments.as_slice())];
        trays.extend(
            chat.attachment_trays
                .iter()
                .map(|(k, v)| (*k, v.as_slice())),
        );
        for (key, attachments) in trays {
            // A mesh scope keys its tray as `None`; it is no conversation
            // core can restore.
            let Some(id) = key else {
                continue;
            };
            if attachments.is_empty() {
                continue;
            }
            recovery.trays.insert(
                id.to_string(),
                attachments.iter().map(StagedAttachment::from).collect(),
            );
        }
        for (root, attachments) in &chat.thread_attachment_trays {
            if !attachments.is_empty() {
                recovery.thread_trays.insert(
                    root.to_string(),
                    attachments.iter().map(StagedAttachment::from).collect(),
                );
            }
        }
        recovery.parked = parked.iter().filter_map(ParkedSend::of).collect();
        recovery
    }

    pub(super) fn persist_recovery_with(&self, owner: UserId, parked: &[ComposerSend]) -> bool {
        self.composer_recovery_dirty.store(true, Ordering::SeqCst);
        if self.recovery_owner() != Some(owner) {
            return false;
        }
        let recovery = self.composer_recovery_snapshot(parked);
        let repo = self.settings_repo();
        let key = Self::composer_recovery_key(owner);
        let written = if recovery.is_empty() {
            repo.delete(&key).map(|_| ())
        } else {
            repo.set(&key, &recovery)
        };
        if written.is_err() {
            tracing::warn!("composer recovery state not saved");
            false
        } else {
            self.composer_recovery_dirty.store(false, Ordering::SeqCst);
            true
        }
    }

    /// Load `owner`'s row into the trays and the parked list, preserving
    /// files not named by a successfully acknowledged row. Runs once per
    /// session, under the drafts lock; nothing in here takes it again.
    ///
    /// Only a row that was read, or confirmed absent, is acted on. A read
    /// or decode failure is unknown state: nothing is restored, nothing is
    /// swept, and `recovery_owner` returns `None` for the rest of the
    /// session so no tray change writes over the row. The next session
    /// reads it again; sign-out deletes it as always.
    pub(super) fn restore_composer_recovery(&self, owner: UserId, session_revision: u64) {
        let key = Self::composer_recovery_key(owner);
        let recovery = match self.settings_repo().get::<ComposerRecovery>(&key) {
            Ok(row) => row.unwrap_or_default(),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "composer recovery row not read; row and staged files left as they are"
                );
                self.composer_recovery_unread
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                return;
            }
        };
        self.composer_recovery_unread
            .store(false, std::sync::atomic::Ordering::SeqCst);

        let chat = self.chat_state();
        let revision = chat.get().attachment_revision;
        for (id, attachments) in recovery.trays {
            let Ok(channel) = id.parse::<ChannelId>() else {
                tracing::warn!(tray = %id, "unparsable tray key; recovery row and files protected");
                self.composer_recovery_unread.store(true, Ordering::SeqCst);
                continue;
            };
            for attachment in attachments
                .into_iter()
                .filter(|attachment| self.restorable_staged_file(&attachment.path))
                .map(StagedAttachment::into_pending)
            {
                // Trays are keyed by channel id alone, so the variant does
                // not matter here: a DM's tray has the DM's derived id.
                chat.add_pending_attachment_for(
                    ActiveConversation::Channel(channel),
                    revision,
                    attachment,
                );
            }
        }
        for (id, attachments) in recovery.thread_trays {
            let Ok(root) = id.parse::<MessageId>() else {
                tracing::warn!(tray = %id, "unparsable thread tray key; recovery row protected");
                self.composer_recovery_unread.store(true, Ordering::SeqCst);
                continue;
            };
            for attachment in attachments
                .into_iter()
                .filter(|attachment| self.restorable_staged_file(&attachment.path))
                .map(StagedAttachment::into_pending)
            {
                chat.add_thread_attachment_for(root, revision, attachment);
            }
        }
        let mut parked = self.parked_sends.lock();
        for send in recovery.parked {
            let attachments: Vec<PendingAttachment> = send
                .attachments
                .into_iter()
                .filter(|attachment| self.restorable_staged_file(&attachment.path))
                .map(StagedAttachment::into_pending)
                .collect();
            if send.text.is_empty() && send.reply.is_none() && attachments.is_empty() {
                continue;
            }
            parked.push(ComposerSend {
                attempt_id: uuid::Uuid::new_v4(),
                session_revision,
                conversation: send.conversation.into_active(),
                text: send.text,
                reply: send.reply,
                attachments,
            });
        }
        drop(parked);
        // Unreferenced files can be unacknowledged staging writes. Keep them
        // until explicit removal/send/sign-out rather than guessing ownership.
    }
}

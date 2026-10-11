use super::{AnonDmError, AnonDmService};
use crate::domain::anon_dm::AnonDmStatus;

impl AnonDmService {
    /// Our profile key, when the visibility setting shares it with anon
    /// peers (`profile_visibility::anon_shareable_profile_key`).
    pub(crate) fn get_my_profile_key(&self) -> Option<[u8; 32]> {
        crate::services::profile_visibility::anon_shareable_profile_key(&self.settings_repo)
    }

    /// Get our display name from settings.
    pub fn get_my_display_name(&self) -> Option<String> {
        use crate::storage::repositories::settings_keys;
        self.settings_repo
            .get_string(settings_keys::DISPLAY_NAME)
            .ok()
            .flatten()
    }

    /// Get the current user's avatar bytes (if set).
    ///
    /// This is the raw avatar image data stored locally during registration.
    /// Used by UI to display the user's own avatar in the sidebar.
    pub fn get_user_avatar(&self) -> Option<Vec<u8>> {
        use crate::storage::repositories::settings_keys;
        self.settings_repo
            .get_bytes(settings_keys::AVATAR)
            .ok()
            .flatten()
    }

    /// The secret-chat attachment service: every transfer over Tor.
    fn secret_attachments(&self) -> crate::services::AttachmentService {
        crate::services::AttachmentService::new(
            self.attachment_repo.clone(),
            self.grpc_pool.clone(),
        )
        .for_secret_chat()
    }

    /// Our avatar as a pointer to an encrypted upload, sent only alongside a
    /// shared profile key (see [`crate::services::secret_avatar`]).
    pub(crate) async fn my_avatar_pointer(
        &self,
    ) -> Option<crate::proto::content_v1::AttachmentPointer> {
        let avatar = self.get_user_avatar()?;
        crate::services::secret_avatar::pointer_for(
            &self.settings_repo,
            &self.secret_attachments(),
            &avatar,
        )
        .await
    }

    /// Our avatar's pointer for a secret-chat request, which must not wait on
    /// an upload: a first upload over Tor can take a new hidden-service
    /// circuit and fail after ~10 s (RC108 O3). The cached pointer when this
    /// avatar was uploaded before; otherwise `None`, and the upload runs in
    /// the background so the profile update sent once the peer accepts
    /// (`send_profile_update`) finds it cached.
    pub(crate) fn my_avatar_pointer_without_upload(
        &self,
    ) -> Option<crate::proto::content_v1::AttachmentPointer> {
        let avatar = self.get_user_avatar()?;
        let cached = crate::services::secret_avatar::cached_pointer(&self.settings_repo, &avatar);
        if cached.is_none() && tokio::runtime::Handle::try_current().is_ok() {
            let (settings, attachments) = (self.settings_repo.clone(), self.secret_attachments());
            tokio::spawn(async move {
                crate::services::secret_avatar::pointer_for(&settings, &attachments, &avatar).await;
            });
        }
        cached
    }

    /// Our avatar's pointer for a secret-chat acceptance, without uploading:
    /// the cached pointer of an earlier upload, and whether an avatar is
    /// owed (we have one that was never uploaded), so the profile update
    /// after acceptance must carry it (RC108 O3, responder side).
    pub(crate) fn my_uploaded_avatar_pointer(
        &self,
    ) -> (Option<crate::proto::content_v1::AttachmentPointer>, bool) {
        let Some(avatar) = self.get_user_avatar() else {
            return (None, false);
        };
        let cached = crate::services::secret_avatar::cached_pointer(&self.settings_repo, &avatar);
        let owed = cached.is_none();
        (cached, owed)
    }

    /// The avatar a peer's pointer names; `current` is what we hold for them.
    /// `Err` when the fetch failed (worth retrying, see
    /// [`Self::refresh_peer_avatar`]).
    pub(crate) async fn resolve_peer_avatar(
        &self,
        pointer: Option<&crate::proto::content_v1::AttachmentPointer>,
        current: Option<Vec<u8>>,
    ) -> Result<Option<Vec<u8>>, crate::services::attachment::AttachmentError> {
        crate::services::secret_avatar::resolve(&self.secret_attachments(), pointer, current).await
    }

    /// Fetch the avatar a peer's pointer names, after each of `delays` until
    /// a fetch goes through, and store it when it differs from the one we
    /// hold. Returns the new avatar, or `None` when nothing changed (no
    /// conversation with this peer, same avatar, every fetch failed). The
    /// pointer stays pending until a fetch goes through, so opening the chat
    /// retries it ([`Self::retry_pending_peer_avatar`]).
    pub(crate) async fn refresh_peer_avatar(
        &self,
        conversation_id: &crate::domain::anon_dm::AnonDmId,
        peer_id: &crate::domain::UserId,
        pointer: &crate::proto::content_v1::AttachmentPointer,
        delays: &[std::time::Duration],
    ) -> Result<Option<Vec<u8>>, AnonDmError> {
        let held = match self.anon_dm_repo.get_conversation(conversation_id)? {
            Some(dm) if dm.peer_id == *peer_id && dm.status != AnonDmStatus::Blocked => {
                dm.peer_avatar
            }
            _ => return Ok(None),
        };
        let key = conversation_id.to_string();
        crate::services::secret_avatar::set_pending_peer(&self.settings_repo, &key, pointer);
        let avatar = crate::services::secret_avatar::retrying(delays, || {
            self.resolve_peer_avatar(Some(pointer), held.clone())
        })
        .await;
        let Ok(avatar) = avatar else {
            return Ok(None);
        };
        crate::services::secret_avatar::clear_pending_peer(
            &self.settings_repo,
            &key,
            Some(pointer),
        );
        match avatar {
            Some(avatar) => self.store_peer_avatar(conversation_id, peer_id, avatar),
            None => Ok(None),
        }
    }

    /// One more fetch of a peer avatar that has not come through yet (the
    /// chat was opened). The new avatar, or `None` when nothing was pending
    /// or this fetch failed too (it stays pending).
    pub(crate) async fn retry_pending_peer_avatar(
        &self,
        conversation_id: &crate::domain::anon_dm::AnonDmId,
    ) -> Result<Option<Vec<u8>>, AnonDmError> {
        let Some(pointer) = crate::services::secret_avatar::pending_peer(
            &self.settings_repo,
            &conversation_id.to_string(),
        ) else {
            return Ok(None);
        };
        let Some(dm) = self.anon_dm_repo.get_conversation(conversation_id)? else {
            return Ok(None);
        };
        self.refresh_peer_avatar(
            conversation_id,
            &dm.peer_id,
            &pointer,
            &[std::time::Duration::ZERO],
        )
        .await
    }

    /// Store `avatar` as the peer's when it is new. Re-reads the row (a fetch
    /// takes a while) and writes the avatar column alone, so no other writer's
    /// snapshot is replaced. Returns the avatar when it changed.
    pub(crate) fn store_peer_avatar(
        &self,
        conversation_id: &crate::domain::anon_dm::AnonDmId,
        peer_id: &crate::domain::UserId,
        avatar: Vec<u8>,
    ) -> Result<Option<Vec<u8>>, AnonDmError> {
        match self.anon_dm_repo.get_conversation(conversation_id)? {
            Some(dm) if dm.peer_id == *peer_id && dm.peer_avatar.as_ref() != Some(&avatar) => {
                self.anon_dm_repo
                    .set_peer_avatar(conversation_id, &avatar)?;
                Ok(Some(avatar))
            }
            _ => Ok(None),
        }
    }

    /// Get our Signal Protocol identity key.
    pub(crate) fn get_my_identity_key(&self) -> Result<[u8; 33], AnonDmError> {
        use crate::storage::repositories::settings_keys;

        // Get identity key from settings
        let key_bytes = self
            .settings_repo
            .get_bytes(settings_keys::IDENTITY_KEY_PUBLIC)
            .map_err(|e| AnonDmError::CryptoError(e.to_string()))?
            .ok_or(AnonDmError::NotLoggedIn)?;

        if key_bytes.len() != 33 {
            return Err(AnonDmError::CryptoError(format!(
                "Invalid identity key length: {}",
                key_bytes.len()
            )));
        }

        let mut key = [0u8; 33];
        key.copy_from_slice(&key_bytes);
        Ok(key)
    }

    /// Generate random padding to reach a target size.
    ///
    /// Delegates to the centralized NDSS '21 padding in `gateway::builders`.
    pub(crate) fn generate_padding(boundary: usize) -> Vec<u8> {
        crate::services::gateway::builders::generate_padding(boundary)
    }
}

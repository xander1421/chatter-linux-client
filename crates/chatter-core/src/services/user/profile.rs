use super::*;

impl UserService {
    // ========================================================================
    // Profile Operations
    // ========================================================================

    /// Get a user's profile by ID.
    ///
    /// Fetches from server and upserts to local storage.
    /// If we have the user's profile key (from friendship), decrypts display_name.
    pub async fn get_user_profile(&self, user_id: &UserId) -> Result<Profile, UserError> {
        let current_user = self.settings_repo.current_user_id()?;

        let mut user_client = self.user_client();
        let proto_profile = user_client.get_user_profile(&user_id.to_string()).await?;

        let mut profile = proto_to_domain_profile(&proto_profile)?;

        if *user_id == current_user {
            restore_own_profile(
                &self.settings_repo,
                &self.user_repo,
                &mut profile,
                &proto_profile,
            )?;
            self.user_repo.ensure_user_exists(user_id)?;
            if self.user_repo.upsert_profile(&profile)? {
                self.event_bus
                    .emit_domain(super::profile_updated_event(&profile));
            }
            self.profile_fetched_at
                .lock()
                .insert(*user_id, Instant::now());
            return Ok(profile);
        }

        // Decrypt with their profile key. The server value is always
        // ciphertext, so we never fall back to it. No key yet keeps a known
        // local name; a held key that fails authentication explicitly resolves
        // to an unavailable label rather than presenting a stale name as current.
        let stored = self.user_repo.get_profile(user_id).ok().flatten();
        profile.display_name = decrypt_friend_display_name(
            &self.user_repo,
            &current_user,
            user_id,
            &proto_profile.display_name,
        )
        .or_else(|| {
            stored
                .as_ref()
                .map(|p| p.display_name.clone())
                .filter(|n| !n.is_empty())
        })
        .unwrap_or_else(|| friend_display_name_placeholder(user_id));

        // Decrypt status the same way; proto now carries ciphertext.
        profile.status_message = decrypt_friend_status(
            &self.user_repo,
            &current_user,
            user_id,
            &proto_profile.status_message,
        )
        .map(StatusMessage::new)
        .or_else(|| {
            // Only a decrypt miss keeps the old one; an empty value is a clear.
            (!proto_profile.status_message.is_empty())
                .then(|| stored.as_ref().and_then(|p| p.status_message.clone()))
                .flatten()
        });

        // Decrypt the avatar with the friend's profile key (if we have it).
        profile.avatar_bytes = self
            .try_decrypt_avatar(&current_user, user_id, &proto_profile.encrypted_avatar)
            .or_else(|| {
                (!proto_profile.encrypted_avatar.is_empty())
                    .then(|| stored.and_then(|p| p.avatar_bytes))
                    .flatten()
            });

        self.user_repo.ensure_user_exists(user_id)?;
        if self.user_repo.upsert_profile(&profile)? {
            self.event_bus
                .emit_domain(super::profile_updated_event(&profile));
        }
        self.profile_fetched_at
            .lock()
            .insert(*user_id, Instant::now());

        Ok(profile)
    }

    /// Refetch a user's profile if this device has not fetched it within
    /// [`PROFILE_REFRESH_INTERVAL`]; `Ok(None)` when it is still fresh.
    ///
    /// A peer's name/bio/avatar edit does not change their profile key, so
    /// nothing pushes it to us. Signal-Desktop covers this by calling
    /// `throttledGetProfiles` (a 5-minute throttle) whenever a conversation is
    /// opened (`ts/state/ducks/conversations.preload.ts`); this is the same
    /// decision. Call it on DM open and profile open.
    pub async fn refresh_profile_if_stale(
        &self,
        user_id: &UserId,
    ) -> Result<Option<Profile>, UserError> {
        let last = self.profile_fetched_at.lock().get(user_id).copied();
        if !profile_fetch_due(last, Instant::now()) {
            return Ok(None);
        }
        self.get_user_profile(user_id).await.map(Some)
    }

    /// Try to decrypt a friend's avatar using their profile key.
    ///
    /// Returns Some(avatar_bytes) if we have their profile key and decryption
    /// succeeds; None if no key, empty blob, or decryption fails. Mirrors
    /// [`decrypt_friend_display_name`] but the avatar travels as raw bytes
    /// (no base64) in the proto's `encrypted_avatar` field.
    pub(super) fn try_decrypt_avatar(
        &self,
        current_user_id: &UserId,
        friend_id: &UserId,
        encrypted_avatar: &[u8],
    ) -> Option<Vec<u8>> {
        if encrypted_avatar.is_empty() {
            return None;
        }

        let profile_key = self
            .user_repo
            .get_friend_profile_key(current_user_id, friend_id)
            .ok()
            .flatten()?;

        crate::crypto::profile_crypto::decrypt_avatar(&profile_key, encrypted_avatar).ok()
    }

    /// Update the current user's profile.
    ///
    /// # Security Note
    ///
    /// If display_name is provided, it is encrypted with the profile_key before sending.
    /// Server stores only ciphertext - it cannot read profile data.
    /// Update the current user's profile.
    ///
    /// Owned `Option<String>` (rather than `Option<&str>`) is intentional —
    /// `Option<&str>` plays badly with the async-Rust borrow checker because
    /// the future captures references that must outlive every `.await`. The
    /// owned variant lets callers pass `name.clone()` once and forget.
    pub async fn update_user_profile(
        &self,
        display_name: Option<String>,
        status_message: Option<StatusMessage>,
    ) -> Result<Profile, UserError> {
        use base64::Engine;

        let current_user = self.settings_repo.current_user_id()?;

        let existing = self
            .user_repo
            .get_profile(&current_user)?
            .ok_or_else(|| UserError::UserNotFound(current_user))?;

        // Load our profile_key once — needed to encrypt both display_name and
        // avatar. Server never sees plaintext profile data, only opaque blobs.
        let profile_key: Option<[u8; 32]> = match self.settings_repo.get_bytes(keys::PROFILE_KEY)? {
            Some(bytes) => bytes.try_into().ok(),
            None => None,
        };

        // SECURITY: Encrypt display_name with profile_key if being updated
        let encrypted_display_name = if let Some(ref name) = display_name {
            if name.len() > crate::crypto::profile_crypto::MAX_NAME_LENGTH {
                return Err(UserError::InvalidInput("that name is too long".into()));
            }
            let profile_key = profile_key.ok_or(UserError::NotLoggedIn)?;
            let encrypted =
                crate::crypto::profile_crypto::encrypt_display_name(&profile_key, name)?;
            Some(base64::engine::general_purpose::STANDARD.encode(encrypted))
        } else {
            None
        };

        // SECURITY: Encrypt status_message with profile_key if being updated —
        // same E2E treatment as display_name (was previously sent plaintext).
        let encrypted_status = if let Some(ref s) = status_message {
            let profile_key = profile_key.ok_or(UserError::NotLoggedIn)?;
            let encrypted = crate::crypto::profile_crypto::encrypt_status(&profile_key, &s.text)?;
            Some(base64::engine::general_purpose::STANDARD.encode(encrypted))
        } else {
            None
        };

        // Encrypt our avatar (from the local AVATAR settings key) for E2E
        // delivery to friends — same profile-key crypto the anon path uses, but
        // carried in the server UserProfile's `encrypted_avatar` blob. Normalize
        // (downscale/re-encode to WebP via the canonical avatar normalizer) when
        // it exceeds the ~64 KB target, then encrypt. If the encrypted blob would
        // still exceed the wire cap, skip it so the rest of the update succeeds.
        let (encrypted_avatar, own_avatar_bytes): (Vec<u8>, Option<Vec<u8>>) = match (
            profile_key,
            self.settings_repo.get_bytes(keys::AVATAR)?,
        ) {
            (Some(pk), Some(avatar)) if !avatar.is_empty() => {
                const AVATAR_TARGET_BYTES: usize = 64 * 1024;
                // Match the anon-DM avatar ceiling; the profile RPC rides a
                // normal gRPC stream (MB-scale limit), so the old 96 KB was
                // needlessly tight — a 512² WebP-lossless photo (~120-200 KB)
                // was being dropped. WebP-lossless of a 512² image never
                // approaches 1 MB, so this is a safety net, not a real gate.
                const ENCRYPTED_AVATAR_CAP: usize = crate::domain::anon_dm_init::MAX_AVATAR_SIZE;
                let normalized = if avatar.len() > AVATAR_TARGET_BYTES {
                    crate::media::ImageProcessor::process_avatar_from_memory(&avatar)
                        .unwrap_or(avatar)
                } else {
                    avatar
                };
                match crate::crypto::profile_crypto::encrypt_avatar(&pk, &normalized) {
                    Ok(enc) if enc.len() <= ENCRYPTED_AVATAR_CAP => (enc, Some(normalized)),
                    Ok(enc) => {
                        tracing::warn!(
                            encrypted_len = enc.len(),
                            cap = ENCRYPTED_AVATAR_CAP,
                            "Encrypted avatar exceeds wire cap; omitting from profile update"
                        );
                        (Vec::new(), Some(normalized))
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "Failed to encrypt avatar; omitting from profile update");
                        (Vec::new(), None)
                    }
                }
            }
            _ => (Vec::new(), None),
        };

        let proto_profile = UserProfile {
            user_id: current_user.to_string(),
            display_name: encrypted_display_name.unwrap_or_else(|| existing.display_name.clone()),
            // Encrypted when we're updating status; otherwise a throwaway value
            // (status_message is only in the update mask when actually changed).
            status_message: encrypted_status
                .or_else(|| existing.status_message.as_ref().map(|s| s.text.clone()))
                .unwrap_or_default(),
            presence: domain_presence_to_proto(existing.presence),
            encrypted_name: Vec::new(),
            encrypted_about: Vec::new(),
            encrypted_avatar: encrypted_avatar.clone(),
        };

        let mut paths = Vec::new();
        if display_name.is_some() {
            paths.push("display_name".to_string());
        }
        if status_message.is_some() {
            paths.push("status_message".to_string());
        }
        if !encrypted_avatar.is_empty() {
            paths.push("encrypted_avatar".to_string());
        }

        // Only masked fields go up. The server reads an absent or empty mask
        // as "every field", which would upload the stored plaintext name and
        // status (refused since server 8feb3585) and clear the avatar — so
        // with nothing to change, nothing is sent.
        if paths.is_empty() {
            return Ok(existing);
        }
        let update_mask = prost_types::FieldMask { paths };

        let mut user_client = self.user_client();
        let mut stream = user_client
            .update_user_profile(proto_profile, update_mask)
            .await?;

        let mut profile_opt: Option<Profile> = None;
        while let Some(event_result) = stream.next().await {
            let event = event_result?;
            if let Some(user_event::Event::ProfileUpdated(proto_profile)) = event.event {
                profile_opt = Some(proto_to_domain_profile(&proto_profile)?);
                break;
            }
        }

        let mut profile = profile_opt.ok_or_else(|| {
            UserError::InvalidInput("no profile updated event received".to_string())
        })?;

        // Store plaintext locally. When we ARE changing the name, use the
        // plaintext we just sent. When we're NOT (status/avatar-only update),
        // the ProfileUpdated event echoes the ENCRYPTED display_name, so never
        // persist proto_profile.display_name — keep our own plaintext name
        // (settings holds it decrypted since login/registration).
        profile.display_name = match display_name {
            Some(plaintext_name) => plaintext_name,
            None => self
                .settings_repo
                .get_string(keys::DISPLAY_NAME)
                .ok()
                .flatten()
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| friend_display_name_placeholder(&profile.user_id)),
        };

        // Same for status: the echo carries the ENCRYPTED value. Keep the
        // plaintext we just set, else the existing local plaintext.
        profile.status_message = status_message.or(existing.status_message);

        // Store our own (normalized) avatar bytes locally — the returned event
        // only carries the encrypted blob, which we can't decrypt for ourselves
        // via the friend-key path.
        if own_avatar_bytes.is_some() {
            profile.avatar_bytes = own_avatar_bytes;
        }

        self.user_repo.upsert_profile(&profile)?;

        Ok(profile)
    }
}

/// Fill our OWN fetched profile with plaintext and restore the local copies
/// the UI reads (`keys::DISPLAY_NAME`, `keys::AVATAR`).
///
/// The server holds our name, status and avatar encrypted under OUR profile
/// key (`update_user_profile` encrypts with `keys::PROFILE_KEY`), not under
/// a friend key — so the friend path would find no key and render a
/// placeholder. A new device has none of these locally (it gets the profile
/// key from the SVR restore), which is why its own profile showed initials
/// while friends saw the photo. A field that is empty or fails to decrypt
/// keeps the local value rather than being cleared.
///
/// Shared by the fetch path (`get_user_profile`) and the live push of our own
/// profile on the user-events stream, which a linked device receives when
/// another device of this account edits the profile.
pub(crate) fn restore_own_profile(
    settings_repo: &SettingsRepository,
    user_repo: &UserRepository,
    profile: &mut Profile,
    proto: &UserProfile,
) -> Result<(), UserError> {
    use crate::crypto::profile_crypto;
    use base64::Engine;

    let key: Option<[u8; 32]> = settings_repo
        .get_bytes(keys::PROFILE_KEY)?
        .and_then(|b| b.try_into().ok());
    let b64 = |s: &str| {
        base64::engine::general_purpose::STANDARD
            .decode(s)
            .ok()
            .filter(|b| !b.is_empty())
    };

    let name = key.as_ref().and_then(|k| {
        let bytes = b64(&proto.display_name)?;
        profile_crypto::decrypt_display_name(k, &bytes)
            .ok()
            .filter(|n| !n.is_empty())
    });
    let status = key.as_ref().and_then(|k| {
        let bytes = b64(&proto.status_message)?;
        profile_crypto::decrypt_status(k, &bytes)
            .ok()
            .filter(|s| !s.is_empty())
    });
    let avatar = key.as_ref().and_then(|k| {
        if proto.encrypted_avatar.is_empty() {
            return None;
        }
        profile_crypto::decrypt_avatar(k, &proto.encrypted_avatar).ok()
    });

    if let Some(ref n) = name {
        settings_repo.set_string(keys::DISPLAY_NAME, n)?;
    }
    if let Some(ref a) = avatar {
        settings_repo.set_bytes(keys::AVATAR, a)?;
    }

    let existing = user_repo.get_profile(&profile.user_id)?;
    profile.display_name = name
        .or_else(|| {
            settings_repo
                .get_string(keys::DISPLAY_NAME)
                .ok()
                .flatten()
                .filter(|n| !n.is_empty())
        })
        .unwrap_or_else(|| friend_display_name_placeholder(&profile.user_id));
    profile.status_message = status
        .map(StatusMessage::new)
        .or_else(|| existing.as_ref().and_then(|p| p.status_message.clone()));
    profile.avatar_bytes = avatar.or_else(|| settings_repo.get_bytes(keys::AVATAR).ok().flatten());
    Ok(())
}

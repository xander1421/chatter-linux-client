use super::*;
use crate::crypto::device_credentials::DeviceToken;
use chatter_proto::chat_v1::AuthResponse;
use zeroize::Zeroizing;

impl AuthService {
    /// Login with username, password and PIN.
    pub async fn login(
        &self,
        auth_client: &mut AuthClient,
        username: &str,
        password: &str,
        device_info: &str,
        pin: Option<&str>,
        on_verifying: &(dyn Fn() + Send + Sync),
    ) -> Result<LoginResult, AuthError> {
        let identifier = login_identifier(username);
        if self.is_logged_in() {
            let pending = self
                .pending_key_republish()?
                .ok_or(AuthError::AlreadyLoggedIn)?;
            // LoginFinish can replace a device. A live session can retry its
            // own unfinished publication, and must sign out before switching.
            if pending.login_identifier != identifier {
                return Err(wrong_republish_account());
            }
        }

        // Reuse one PIN derivation for the login's SVR key and local fingerprint.
        let svr_secret = if let Some(p) = pin {
            // Shape-only: this is the EXISTING account PIN being used to log in
            // and drive the post-login SVR restore, not a PIN being chosen.
            // Full strength validation here would lock a user out of their own
            // account for a PIN that was valid when set — the exact
            // ratchet-forward violation the validate_pin/validate_pin_shape
            // split exists to prevent (§10). A weak legacy PIN's cost is
            // already paid; refusing the login only denies the owner.
            crate::crypto::svr::validate_pin_shape(p)
                .map_err(|e| AuthError::LoginFailed(format!("invalid PIN: {e}")))?;
            Zeroizing::new(crate::crypto::svr::derive_svr_secret_async(p, username).await?)
        } else {
            return Err(AuthError::LoginFailed(
                "PIN is required for account recovery".to_string(),
            ));
        };
        let svr_public_key =
            crate::crypto::svr::SvrKeypair::from_secret(&svr_secret).public_key_bytes();

        // LoginFinish only needs a new auth token. The server keeps the
        // account's UAK, and local Signal/profile keys are read after ownership
        // has been checked against the authenticated response.
        let auth_token = DeviceToken::generate();
        let stored_device_id = self
            .settings_repo
            .get_string(keys::SVR_SALT_USERNAME)?
            .filter(|nickname| login_identifier(nickname) == identifier)
            .map(|_| self.settings_repo.get(keys::CURRENT_DEVICE_ID))
            .transpose()?
            .flatten();

        // OPAQUE login: KE1 -> KE2, then KE3 -> session. A wrong password
        // (or an unknown nickname, answered from a fake record) fails while
        // opening KE2, before anything else is sent. KE2 may first cost a
        // proof of work when the account has recent failed attempts.
        let opaque_start = opaque::login_start(password)?;
        let grant = {
            let mut g = self.recovery_login_grant.lock();
            match g.take() {
                Some((id, grant)) if id == identifier => Some(grant),
                other => {
                    *g = other;
                    None
                }
            }
        };
        // This device's trusted-device token for the account, if it has
        // logged in (or registered) here before. Unreadable = none: the login
        // then goes through the shared ceiling, which is what it would do
        // without a token anyway.
        let token_key = keys::trusted_device_token_key(&identifier);
        let stored_token = self.settings_repo.get_bytes(&token_key).ok().flatten();
        let mut device_token = stored_token.clone();
        let started = login_start_paying_pow(
            auth_client,
            &identifier,
            &opaque_start.ke1,
            grant.as_deref(),
            &mut device_token,
            on_verifying,
        )
        .await;
        if stored_token.is_some() && device_token.is_none() {
            // Revoked by the server (password changed, recovered, expired).
            let _ = self.settings_repo.delete(&token_key);
        }
        let started = started?;
        let opaque_done = opaque::login_finish_async(opaque_start, password, started.ke2).await?;
        let response = auth_client
            .login_finish(
                &started.login_id,
                &opaque_done.ke3,
                device_info,
                &svr_public_key, // Derived from PIN
                auth_token.as_bytes(),
                stored_device_id,
            )
            .await
            .map_err(|e| match &e {
                NetworkError::Unauthenticated => AuthError::InvalidCredentials,
                _ => AuthError::Network(e),
            })?;

        self.finish_login(
            username,
            stored_device_id,
            &auth_token,
            &svr_secret,
            response,
        )
    }

    fn finish_login(
        &self,
        username: &str,
        stored_device_id: Option<u32>,
        auth_token: &DeviceToken,
        svr_secret: &[u8; 32],
        response: AuthResponse,
    ) -> Result<LoginResult, AuthError> {
        let session = response
            .session
            .ok_or_else(|| AuthError::LoginFailed("no session returned".to_string()))?;
        let user_profile = response
            .user_profile
            .ok_or_else(|| AuthError::LoginFailed("no user_profile returned".to_string()))?;
        let user_id: UserId = user_profile
            .user_id
            .parse()
            .map_err(|_| AuthError::LoginFailed("invalid user_id format".to_string()))?;
        let device_id = session.device_id;

        // A server-unlinked install may already have lost its keys. A normal
        // logout keeps history, whose SavedMessages channel records its owner.
        let mut local_owner = self.local_account_owner()?;
        if local_owner.is_none()
            && let Some((owner, _)) = self.current_user()
        {
            // Backfill the channel for an older, still-configured device whose
            // first channel sync was interrupted.
            self.cache_saved_messages_channel(owner)?;
            local_owner = self.local_account_owner()?;
        }
        let switched_account = local_owner.is_some_and(|owner| owner != user_id);
        if switched_account && !self.is_logged_in() {
            super::wipe_unlinked_install(self.settings_repo.database())?;
            self.republish_completed
                .store(false, std::sync::atomic::Ordering::Release);
            self.recovery_login_grant.lock().take();
        } else {
            let pending = self.pending_key_republish()?;
            if switched_account
                || pending
                    .as_ref()
                    .is_some_and(|pending| pending.owner != user_id)
            {
                return Err(wrong_republish_account());
            }
            if local_owner.is_none()
                && pending.is_none()
                && (self.identity_manager.has_identity()? || self.user_repo.count()? > 0)
            {
                // A legacy install could sign out before Saved Messages was
                // synced. Preserve its data when ownership can't be established.
                return Err(AuthError::LoginFailed(chatter_i18n::tr(
                    "Could not verify which account owns the local data on this device.",
                )));
            }
        }
        self.settings_repo.delete(keys::UNLINKED_ACCOUNT_ID)?;
        self.cache_saved_messages_channel(user_id)?;

        // Read these only after the wipe. Missing keys let ChatterClient run
        // SVR recovery; only its explicit recovery flow can mint a new identity.
        let identity_public_key = self
            .identity_manager
            .get_identity()?
            .map(|(_, kp)| kp.identity_key().serialize());
        let stored_profile_key = self
            .settings_repo
            .get_bytes(keys::PROFILE_KEY)
            .ok()
            .flatten()
            .and_then(|bytes| <[u8; 32]>::try_from(bytes.as_slice()).ok())
            .map(Zeroizing::new);

        // Save the fresh trusted-device token for the account's next login.
        if !response.trusted_device_token.is_empty() {
            self.settings_repo.set_bytes(
                &keys::trusted_device_token_key(&login_identifier(username)),
                &response.trusted_device_token,
            )?;
        }

        // PRIVACY: decrypt display_name with the locally-known profile_key.
        // Fresh-device case has no profile_key yet; the post-login SVR
        // restore plus a follow-up profile fetch will populate display_name.
        let display_name = match (&stored_profile_key, user_profile.display_name.is_empty()) {
            (_, true) => String::new(),
            // Login does not need the name, so an undecryptable one must not
            // fail it: keep whatever name this device already had.
            (Some(pk), false) => base64::engine::general_purpose::STANDARD
                .decode(&user_profile.display_name)
                .map_err(|e| e.to_string())
                .and_then(|bytes| {
                    crate::crypto::profile_crypto::decrypt_display_name(pk, &bytes)
                        .map_err(|e| e.to_string())
                })
                .unwrap_or_else(|e| {
                    tracing::warn!("login: display_name undecryptable, keeping local name: {e}");
                    self.settings_repo
                        .get_string(keys::DISPLAY_NAME)
                        .ok()
                        .flatten()
                        .unwrap_or_default()
                }),
            (None, false) => String::new(),
        };

        self.settings_repo
            .set_string(keys::DISPLAY_NAME, &display_name)?;
        self.settings_repo
            .set_string(keys::SVR_SALT_USERNAME, username)?;
        self.settings_repo
            .set_string(keys::PUSH_TOPIC, &response.push_topic)?;
        self.settings_repo
            .set_string(keys::DEVICE_WAKE_TOPIC, &response.device_wake_topic)?;

        let fp = Zeroizing::new(crate::crypto::svr::pin_fingerprint_from_secret(svr_secret));
        self.settings_repo
            .set_bytes(keys::PIN_FINGERPRINT, fp.as_slice())?;

        // Private friend codes become available once the local identity exists.
        if let Some(pk) = &identity_public_key {
            self.settings_repo
                .set_bytes(keys::IDENTITY_KEY_PUBLIC, pk.as_ref())?;
        }

        // Store user locally if not exists
        if self.user_repo.get(&user_id)?.is_none() {
            let user = User {
                id: user_id,
                username: username.to_string(),
                svr_public_key: Vec::new(), // SVR key will be set during PIN setup
                last_active: chrono::Utc::now(),
                created_at: chrono::Utc::now(),
            };
            self.user_repo.insert(&user)?;
        }

        // Create/update profile
        let profile = Profile {
            user_id,
            display_name: display_name.clone(),
            avatar_bytes: None,
            status_message: None,
            presence: PresenceStatus::Online,
        };
        self.user_repo.upsert_profile(&profile)?;

        // Commit the local credentials together before opening authenticated
        // RPCs. Always use the device id the server actually assigned.
        {
            let mut conn = self.settings_repo.database().connection()?;
            let tx = conn.transaction().map_err(StorageError::from)?;
            SettingsRepository::set_with_connection(
                &tx,
                keys::CURRENT_USER_ID,
                &user_id.to_string(),
            )?;
            SettingsRepository::set_with_connection(&tx, keys::CURRENT_DEVICE_ID, &device_id)?;
            SettingsRepository::set_with_connection(
                &tx,
                keys::DEVICE_AUTH_TOKEN,
                &auth_token.to_base64(),
            )?;
            tx.commit().map_err(StorageError::from)?;
        }
        self.auth_interceptor
            .set_basic(format!("{user_id}.{device_id}"), auth_token.to_base64());

        Ok(LoginResult {
            user_id,
            device_id,
            display_name,
            needs_prekey_upload: switched_account || stored_device_id != Some(device_id),
            keys_restored: false, // Set by caller after the SVR restore attempt
            // No local identity yet. The caller must resolve this before the
            // login can be called successful — see `ChatterClient::login`.
            key_recovery: identity_public_key
                .is_none()
                .then_some(KeyRecoveryNeed::NoPinProvided),
            recovery_codes: None, // Login never returns recovery codes
        })
    }
}

fn wrong_republish_account() -> AuthError {
    AuthError::LoginFailed(chatter_i18n::tr(
        "Sign in to the same account to finish publishing your encryption keys.",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::ChatterClient;
    use crate::domain::message::{ConversationType, Message};
    use crate::storage::MessageRepository;
    use chatter_proto::chat_v1::{Session, UserProfile};
    use rand::{SeedableRng, rngs::StdRng};

    fn test_client(data_dir: &std::path::Path) -> Arc<ChatterClient> {
        ChatterClient::builder()
            .server_url("https://localhost:1")
            .data_dir(data_dir.to_str().unwrap())
            .build()
            .unwrap()
    }

    #[tokio::test]
    async fn login_refuses_to_reuse_unattributed_legacy_keys() {
        let dir = tempfile::tempdir().unwrap();
        let client = test_client(dir.path());
        let auth = client.auth();
        auth.identity_manager
            .generate_identity(&mut StdRng::seed_from_u64(20))
            .unwrap();

        let reply = AuthResponse {
            session: Some(Session {
                device_id: 9,
                ..Default::default()
            }),
            user_profile: Some(UserProfile {
                user_id: UserId::new().to_string(),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(matches!(
            auth.finish_login("owner", None, &DeviceToken::generate(), &[4; 32], reply),
            Err(AuthError::LoginFailed(_))
        ));
        assert!(auth.identity_manager.has_identity().unwrap());
        assert!(!auth.is_logged_in());
    }

    async fn check_login_after_logout(switch_accounts: bool) {
        let dir = tempfile::tempdir().unwrap();
        let client = test_client(dir.path());
        let auth = client.auth();
        let settings = &auth.settings_repo;
        let db = settings.database();
        let messages = MessageRepository::new(db.clone());
        let old_owner = UserId::new();
        let new_owner = if switch_accounts {
            UserId::new()
        } else {
            old_owner
        };
        let new_username = if switch_accounts { "other" } else { "owner" };

        settings
            .set_string(keys::CURRENT_USER_ID, &old_owner.to_string())
            .unwrap();
        settings.set(keys::CURRENT_DEVICE_ID, &1u32).unwrap();
        settings
            .set_bytes(keys::DEVICE_AUTH_TOKEN, &[1; 32])
            .unwrap();
        settings.set_bytes(keys::PROFILE_KEY, &[2; 32]).unwrap();
        settings
            .set_string(keys::SVR_SALT_USERNAME, "owner")
            .unwrap();
        settings
            .set_string(keys::DISPLAY_NAME, "Original name")
            .unwrap();
        settings.set("kt.libsignal.head", &123u64).unwrap();
        settings.set("kt.libsignal.distinguished", &100u64).unwrap();
        auth.begin_key_republish().unwrap();
        let (_, identity) = auth
            .identity_manager
            .generate_identity(&mut StdRng::seed_from_u64(19))
            .unwrap();
        let public_key = identity.identity_key().serialize();
        settings
            .set_bytes(keys::IDENTITY_KEY_PUBLIC, &public_key)
            .unwrap();
        let key_store = &client.signal().key_store;
        let peer_address = format!("{}:1", UserId::new());
        key_store.store_session(&peer_address, &[3]).unwrap();

        let saved_message = Message::new_text(
            old_owner.into(),
            ConversationType::SavedMessages,
            Some(old_owner),
            "Saved history".into(),
            1,
        );
        messages.insert(&saved_message).unwrap();
        let files = db.path().with_file_name("attachments");
        std::fs::create_dir_all(&files).unwrap();
        let attachment = files.join("saved.enc");
        std::fs::write(&attachment, b"test attachment").unwrap();

        // The client had not synced channels when it logged out. Sign-out
        // should still keep the real Saved Messages channel and its history.
        assert_eq!(auth.local_account_owner().unwrap(), None);
        if switch_accounts {
            let channel =
                tonic::transport::Endpoint::from_static("http://127.0.0.1:1").connect_lazy();
            let mut rpc = AuthClient::new(channel.into(), auth.auth_interceptor.clone());
            auth.logout(&mut rpc).await.unwrap();
        } else {
            auth.do_logout().await.unwrap();
        }
        assert!(!auth.is_logged_in());
        assert!(auth.auth_interceptor.get_auth_info().is_none());
        assert!(
            settings
                .get_string(keys::UNLINKED_ACCOUNT_ID)
                .unwrap()
                .is_none()
        );
        assert_eq!(auth.local_account_owner().unwrap(), Some(old_owner));

        // Enter the same completion path as a successful LoginFinish.
        let token = DeviceToken::generate();
        let result = auth
            .finish_login(
                new_username,
                None,
                &token,
                &[4; 32],
                AuthResponse {
                    session: Some(Session {
                        device_id: 9,
                        ..Default::default()
                    }),
                    user_profile: Some(UserProfile {
                        user_id: new_owner.to_string(),
                        display_name: "not encrypted for this device".into(),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            )
            .unwrap();

        assert_eq!(result.user_id, new_owner);
        assert_eq!(result.device_id, 9);
        assert!(result.needs_prekey_upload);
        assert_eq!(auth.current_user(), Some((new_owner, 9)));
        assert_eq!(auth.local_account_owner().unwrap(), Some(new_owner));
        assert_eq!(
            settings
                .get_bytes(keys::DEVICE_AUTH_TOKEN)
                .unwrap()
                .as_deref(),
            Some(token.as_bytes().as_slice())
        );
        let session = auth.auth_interceptor.get_auth_info().unwrap();
        assert_eq!(session.user_id, new_owner.to_string());
        assert_eq!(session.device_id, 9);
        assert_eq!(session.token, token.to_base64());

        if switch_accounts {
            assert_eq!(result.key_recovery, Some(KeyRecoveryNeed::NoPinProvided));
            assert!(result.display_name.is_empty());
            assert!(auth.identity_manager.get_identity().unwrap().is_none());
            assert!(
                settings
                    .get_bytes(keys::IDENTITY_KEY_PUBLIC)
                    .unwrap()
                    .is_none()
            );
            assert!(settings.get_bytes(keys::PROFILE_KEY).unwrap().is_none());
            assert!(settings.get::<u64>("kt.libsignal.head").unwrap().is_none());
            assert!(
                settings
                    .get::<u64>("kt.libsignal.distinguished")
                    .unwrap()
                    .is_none()
            );
            assert!(!auth.key_republish_pending().unwrap());
            assert!(key_store.load_session(&peer_address).unwrap().is_none());
            assert!(messages.get(&saved_message.id).unwrap().is_none());
            assert!(!attachment.exists());
        } else {
            assert!(result.key_recovery.is_none());
            assert_eq!(result.display_name, "Original name");
            assert!(auth.identity_manager.has_identity().unwrap());
            assert_eq!(
                settings
                    .get_bytes(keys::IDENTITY_KEY_PUBLIC)
                    .unwrap()
                    .as_deref(),
                Some(public_key.as_ref())
            );
            assert_eq!(settings.get::<u64>("kt.libsignal.head").unwrap(), Some(123));
            assert!(auth.key_republish_pending().unwrap());
            assert!(key_store.load_session(&peer_address).unwrap().is_some());
            assert!(messages.get(&saved_message.id).unwrap().is_some());
            assert!(attachment.exists());
        }
    }

    #[tokio::test]
    async fn same_account_login_after_logout_reuses_local_identity_and_history() {
        check_login_after_logout(false).await;
    }

    #[tokio::test]
    async fn different_account_login_after_logout_wipes_before_loading_identity() {
        check_login_after_logout(true).await;
    }
}

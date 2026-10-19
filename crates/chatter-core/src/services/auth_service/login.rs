use super::*;

impl AuthService {
    /// Login with username and password.
    ///
    /// Orchestrates:
    /// 1. Password hashing
    /// 2. Identity key retrieval/generation
    /// 3. Device credential generation
    /// 4. Server authentication
    /// 5. Local state persistence
    /// 6. Auth interceptor configuration
    pub async fn login(
        &self,
        auth_client: &mut AuthClient,
        username: &str,
        password: &str,
        device_info: &str,
        pin: Option<&str>,
        on_verifying: &(dyn Fn() + Send + Sync),
    ) -> Result<LoginResult, AuthError> {
        // Check not already logged in
        if self.is_logged_in() {
            return Err(AuthError::AlreadyLoggedIn);
        }

        // PRIVACY: the server sees only the nickname-derived identifier, and
        // the password only through OPAQUE (`crypto::opaque`).
        let identifier = login_identifier(username);

        // Profile key: existing-device login loads it from local settings; a
        // fresh device has none here and recovers it from SVR backup AFTER
        // successful login (see `Client::attempt_svr_restore`). This avoids
        // the chicken-and-egg of needing profile_key to authenticate before
        // the authenticated SVR fetch can return profile_key.
        let stored_profile_key: Option<[u8; 32]> = self
            .settings_repo
            .get_bytes(keys::PROFILE_KEY)
            .ok()
            .flatten()
            .and_then(|bytes| <[u8; 32]>::try_from(bytes.as_slice()).ok());

        // Derive SVR public key - required for registration
        //
        // PERF/CORRECTNESS: one Argon2id for both the keypair here and the PIN
        // fingerprint refreshed further down — same (pin, username), same
        // secret — and run off the async worker. See `derive_svr_secret_async`.
        let mut svr_secret: [u8; 32];
        let svr_public_key = if let Some(p) = pin {
            // Shape-only: this is the EXISTING account PIN being used to log in
            // and drive the post-login SVR restore, not a PIN being chosen.
            // Full strength validation here would lock a user out of their own
            // account for a PIN that was valid when set — the exact
            // ratchet-forward violation the validate_pin/validate_pin_shape
            // split exists to prevent (§10). A weak legacy PIN's cost is
            // already paid; refusing the login only denies the owner.
            crate::crypto::svr::validate_pin_shape(p)
                .map_err(|e| AuthError::LoginFailed(format!("invalid PIN: {e}")))?;
            let secret = crate::crypto::svr::derive_svr_secret_async(p, username).await?;
            let keypair = crate::crypto::svr::SvrKeypair::from_secret(&secret);
            svr_secret = secret;
            keypair.public_key_bytes().to_vec()
        } else {
            return Err(AuthError::LoginFailed(
                "PIN is required for account recovery".to_string(),
            ));
        };

        // Load the local identity key pair — and DO NOT create one if it is
        // absent.
        //
        // This used to mint a fresh identity here. That single line was the
        // root of the worst defect in the product: on a reinstall it produced
        // a local identity BEFORE the post-login SVR restore ran, so the
        // restore saw `has_local_identity() == true`, concluded this was an
        // existing device, logged "SVR restore skipped: local identity keys
        // already exist" and returned without ever fetching the PIN backup.
        // The user's real identity key was destroyed by the act of logging in
        // to recover it, and nothing said so.
        //
        // A missing identity is now carried out of here as a fact for the
        // caller to resolve (`ChatterClient::login` runs the SVR restore, and
        // only mints on an explicit user decision once recovery is ruled out).
        // Login itself needs no identity key: the request carries only a
        // device auth token.
        let identity_key_pair = self.identity_manager.get_identity()?.map(|(_, kp)| kp);

        // For login, device_id might come from settings (re-auth) or be assigned by server
        let stored_device_id: Option<u32> = self
            .settings_repo
            .get(keys::CURRENT_DEVICE_ID)
            .ok()
            .flatten();
        let device_id_for_creds = stored_device_id.unwrap_or(1);

        // Generate device credentials. Login never sends a UAK: the server's
        // copy is account-level and moves only at registration and via
        // SetUnidentifiedAccessKey, so a fresh device (no local profile_key
        // yet, so a throwaway key here) or one that missed a ProfileKeySync
        // cannot overwrite it.
        let credentials = match &stored_profile_key {
            Some(pk) => DeviceCredentials::generate(device_id_for_creds, pk),
            None => {
                let mut tmp_pk = generate_profile_key();
                let creds = DeviceCredentials::generate(device_id_for_creds, &tmp_pk);
                tmp_pk.zeroize();
                creds
            }
        };

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
                credentials.auth_token().as_bytes(),
                stored_device_id,
            )
            .await
            .map_err(|e| match &e {
                NetworkError::Unauthenticated => AuthError::InvalidCredentials,
                _ => AuthError::Network(e),
            })?;

        let identity_public_key = identity_key_pair
            .as_ref()
            .map(|kp| kp.identity_key().serialize());

        // Extract session and user profile
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

        // Signing in on an install another device unlinked. The same account
        // keeps this install's history, as Signal relinking to the same
        // account deletes configuration only; a different account must not
        // inherit it, so everything goes first (Signal-Desktop
        // AccountManager.preload.ts createAccount: isRelinkingToSameAccount ->
        // removeAllConfiguration, else removeAllData). Only this answer from
        // the server can say which account it is.
        let stored_profile_key = match self.unlinked_account() {
            Some(former) if former == user_id => {
                self.settings_repo.delete(keys::UNLINKED_ACCOUNT_ID)?;
                stored_profile_key
            }
            Some(_) => {
                super::wipe_unlinked_install(self.settings_repo.database())?;
                None
            }
            None => stored_profile_key,
        };

        // The fresh trusted-device token for this device's next login. In
        // the encrypted settings database, like the device auth token.
        if !response.trusted_device_token.is_empty() {
            self.settings_repo
                .set_bytes(&token_key, &response.trusted_device_token)?;
        }

        // LATENT BUG FIX (surfaced by validated.rs V51's fresh-install flow):
        // on a fresh device we generate credentials with a guessed device_id
        // (1), but the server binds the auth token to the device id it
        // actually ASSIGNS (e.g. 2 when the primary registration still holds
        // device 1). Basic auth is "{user_id}.{device_id}:{token}" — with the
        // stale id every authenticated RPC after login fails device auth.
        // Rebind the same token/UAK to the server-assigned id before
        // persisting or configuring the interceptor.
        let credentials = if credentials.device_id() != device_id {
            DeviceCredentials::from_stored(
                device_id,
                *credentials.auth_token().as_bytes(),
                *credentials.unidentified_access_key(),
            )
        } else {
            credentials
        };

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

        // Persist auth state in settings
        self.settings_repo
            .set_string(keys::CURRENT_USER_ID, &user_id.to_string())?;
        self.settings_repo
            .set(keys::CURRENT_DEVICE_ID, &device_id)?;
        self.settings_repo
            .set_string(keys::DISPLAY_NAME, &display_name)?;
        // Salt for later PIN-derived flows (server message-backup from settings).
        self.settings_repo
            .set_string(keys::SVR_SALT_USERNAME, username)?;
        // Server-issued opaque push topic (empty when push is disabled).
        // configure_push subscribes to this instead of deriving it locally.
        self.settings_repo
            .set_string(keys::PUSH_TOPIC, &response.push_topic)?;
        self.settings_repo
            .set_string(keys::DEVICE_WAKE_TOPIC, &response.device_wake_topic)?;

        // SECURITY: store auth token. profile_key is NOT touched on login -
        // it is set at registration (random) and on a fresh device is loaded
        // by SVR restore. Overwriting from login would either be a no-op or
        // corrupt state.
        self.settings_repo
            .set_bytes(keys::DEVICE_AUTH_TOKEN, credentials.auth_token().as_bytes())?;

        // Refresh local PIN fingerprint (re-auth gate for linked-device approval).
        // Reuses the SVR secret already derived above.
        if pin.is_some() {
            let mut fp = crate::crypto::svr::pin_fingerprint_from_secret(&svr_secret);
            self.settings_repo.set_bytes(keys::PIN_FINGERPRINT, &fp)?;
            fp.zeroize();
        }

        // SECURITY: done with the PIN-derived secret — drop it now.
        svr_secret.zeroize();

        // Store identity public key for NDSS'21 private friend codes.
        // This allows generating friend codes without accessing the SignalStore.
        // Absent on a reinstall until the SVR restore (or an explicit identity
        // reset) supplies one — writing a placeholder would hand out friend
        // codes for a key that does not exist.
        if let Some(pk) = identity_public_key.as_ref() {
            self.settings_repo
                .set_bytes(keys::IDENTITY_KEY_PUBLIC, pk.as_ref())?;
        }

        // Configure auth interceptor for Basic auth
        self.auth_interceptor.set_basic(
            credentials.auth_username(&user_id.to_string()),
            credentials.auth_token_base64(),
        );

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

        Ok(LoginResult {
            user_id,
            device_id,
            display_name,
            needs_prekey_upload: stored_device_id != Some(device_id), // Server may replace a retired device
            keys_restored: false, // Set by caller after the SVR restore attempt
            // No local identity yet. The caller must resolve this before the
            // login can be called successful — see `ChatterClient::login`.
            key_recovery: identity_key_pair
                .is_none()
                .then_some(KeyRecoveryNeed::NoPinProvided),
            recovery_codes: None, // Login never returns recovery codes
        })
    }
}

//! Authentication high-level API (login, registration, recovery, PINs).

use std::sync::Arc;

use super::{ChatterClient, ClientError, RecoveryResult};
use crate::network::grpc::client_pool::TransportMode;
use crate::services::auth_service::{prove_password, AuthError, LoginResult, PendingRecoveryCodes};
use crate::services::key_recovery::{KeyRecoveryNeed, classify_restore_failure};
use crate::services::signal::kt_anchor::{self, OwnIdentityAtSignIn, OwnIdentityCheck};
use crate::services::svr_backup_service::{RestoreResult, SvrBackupError, SvrBackupService};
use crate::storage::repositories::settings_repo::keys;

impl ClientError {
    /// Text for the login/register screen, without the `auth error:` /
    /// `login failed:` chain `Display` produces for logs.
    #[must_use]
    pub fn user_message(&self) -> String {
        match self {
            Self::Auth(e) => e.user_message(),
            Self::CallEnded => chatter_i18n::tr("Call ended"),
            Self::Validation(_) | Self::IdentityRecoveryRequired(_) | Self::DeviceReplaced => {
                self.to_string()
            }
            Self::Connection(crate::error::NetworkError::Unauthenticated) => {
                chatter_i18n::tr("Wrong password.")
            }
            Self::Connection(_) | Self::NotConnected => {
                chatter_i18n::tr("Couldn't reach the server. Try again.")
            }
            _ => chatter_i18n::tr("Something went wrong. Try again."),
        }
    }
}

impl ChatterClient {
    /// Codes issued to this device that the user has not confirmed saving.
    /// The settings database is encrypted at rest; callers must never log them.
    pub fn unacknowledged_recovery_codes(
        &self,
    ) -> Result<Option<PendingRecoveryCodes>, ClientError> {
        let Some((user_id, _)) = self.current_user() else {
            return Ok(None);
        };
        self.auth()
            .settings_repo
            .get(&keys::unacknowledged_recovery_codes_key(user_id))
            .map_err(|e| ClientError::Auth(e.into()))
    }

    /// Forget the locally retained codes after the user confirms saving them.
    pub fn acknowledge_recovery_codes(&self) -> Result<(), ClientError> {
        let (user_id, _) = self.current_user().ok_or(ClientError::Auth(
            crate::services::auth_service::AuthError::NotLoggedIn,
        ))?;
        self.auth()
            .settings_repo
            .delete(&keys::unacknowledged_recovery_codes_key(user_id))
            .map(|_| ())
            .map_err(|e| ClientError::Auth(e.into()))
    }

    pub(super) fn install_session_recovery(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        self.auth()
            .auth_interceptor
            .on_session_ended(Arc::new(move |generation| {
                let Some(client) = weak.upgrade() else { return };
                // This task is deliberately outside session tasks: teardown aborts
                // those tasks and must not abort itself before clearing credentials.
                tokio::spawn(async move {
                    let _lifecycle = client.auth_lifecycle_lock.lock().await;
                    if client.auth().auth_interceptor.generation() != generation {
                        return;
                    }
                    // Flush existing drafts/recovery before advancing the revision;
                    // the explicit logout path intentionally deletes these instead.
                    {
                        let (drafts, _) = client.composer_drafts_for_session();
                        client.persist_composer_drafts(&drafts);
                    }
                    client.persist_composer_recovery();
                    client.auth_state().invalidate_session();
                    let teardown_revision = client.auth_state().get().session_revision;
                    client.retire_presence().await;
                    if let Err(error) = client.auth().end_session() {
                        tracing::error!(%error, "could not persist ended session");
                    }
                    client.shutdown_session(teardown_revision, true).await;
                    client
                        .auth_state()
                        .set_session_ended_if_current(teardown_revision);
                });
            }));

        // The server's dedicated "device unlinked" answer, and nothing else
        // (network/grpc/interceptors.rs DEVICE_UNLINKED_MESSAGE). Signal-Desktop
        // `unlinkAndDisconnect` (background.preload.ts): stop, delete this
        // device's credentials, keys and sessions at once, keep the history
        // until the device is linked again (storage/unlink.rs).
        let weak = Arc::downgrade(self);
        self.auth()
            .auth_interceptor
            .on_device_unlinked(Arc::new(move |generation| {
                let Some(client) = weak.upgrade() else { return };
                tokio::spawn(async move {
                    let _lifecycle = client.auth_lifecycle_lock.lock().await;
                    if client.auth().auth_interceptor.generation() != generation {
                        return;
                    }
                    let Some((former_owner, _)) = client.auth().current_user() else {
                        tracing::error!("device unlinked, but no account is recorded on it");
                        return;
                    };
                    tracing::warn!("this device was unlinked from the account; deleting its keys");
                    // Drafts are history, which an unlink keeps.
                    {
                        let (drafts, _) = client.composer_drafts_for_session();
                        client.persist_composer_drafts(&drafts);
                    }
                    client.persist_composer_recovery();
                    client.auth_state().invalidate_session();
                    let teardown_revision = client.auth_state().get().session_revision;
                    // Stop authenticating before teardown, as the ended-session
                    // path does; there is nothing left to tell the server.
                    client.retire_presence().await;
                    client.auth().auth_interceptor.clear();
                    client.shutdown_session(teardown_revision, false).await;
                    // Keys go after every session task has stopped, so none
                    // can write a prekey or session back.
                    if let Err(error) = client.auth().forget_unlinked_device(former_owner) {
                        tracing::error!(%error, "could not delete the unlinked device's keys");
                    }
                    #[cfg(feature = "mesh")]
                    if let Err(error) = client.mesh().remove_account_bindings(former_owner) {
                        tracing::error!(%error, "could not remove the unlinked account's mesh bindings");
                    }
                    client
                        .auth_state()
                        .set_device_unlinked_if_current(teardown_revision);
                });
            }));
    }

    /// Text for the sign-in screen when this install was unlinked by another
    /// device of its account and has not been linked or signed in since.
    pub fn device_unlinked_notice(&self) -> Option<String> {
        self.auth()
            .unlinked_account()
            .map(|_| crate::state::auth_state::DEVICE_UNLINKED_NOTICE.to_string())
    }

    /// Ensure the gRPC pool is connected before running an auth RPC.
    ///
    /// Called from [`Self::login`] and [`Self::register`]. The startup
    /// `connect()` may have run before the user supplied a server URL
    /// (e.g. first-run, where the URL is entered in the register form
    /// itself), leaving the connection state at `Disconnected`. Without
    /// this self-heal, every auth RPC bounces with "Not connected to
    /// server" until the user manually retries `connect()`.
    async fn ensure_connected_for_auth(&self) -> Result<(), ClientError> {
        if self.is_connected() {
            return Ok(());
        }
        self.connect().await
    }

    // ── Auth (high-level) ───────────────────────────────────────────

    /// Log in and start all background services.
    ///
    /// This is the one-call entry point: authenticates, sets auth state,
    /// and starts inbox polling + event handlers.
    ///
    /// On a device with no local identity keys (a reinstall), the SVR restore
    /// runs BEFORE any identity key is minted, so the user recovers their real
    /// key instead of silently being given a fresh, unreachable one. If the
    /// restore cannot establish an identity — no PIN, wrong PIN, unreachable
    /// server, or no backup at all — this returns
    /// [`ClientError::IdentityRecoveryRequired`] carrying the reason, and the
    /// user must either retry or explicitly call
    /// [`Self::accept_new_identity`].
    pub async fn login(
        self: &Arc<Self>,
        username: &str,
        password: &str,
        device_name: &str,
        pin: Option<&str>,
    ) -> Result<LoginResult, ClientError> {
        self.login_with_replacement_notice(username, password, device_name, pin, async {})
            .await
    }

    /// Run the UI notice before clearing a replaced installation.
    pub async fn login_with_replacement_notice(
        self: &Arc<Self>,
        username: &str,
        password: &str,
        device_name: &str,
        pin: Option<&str>,
        replacement_notice: impl std::future::Future<Output = ()> + Send,
    ) -> Result<LoginResult, ClientError> {
        let _lifecycle = self.auth_lifecycle_lock.lock().await;
        if self.started.load(std::sync::atomic::Ordering::SeqCst)
            || (self.auth_state().is_logged_in()
                && *self.authed_notif_session.lock()
                    == Some(self.auth_state().get().session_revision))
        {
            return Err(ClientError::AlreadyStarted);
        }
        // If startup `connect()` failed (or hasn't run yet), the gRPC
        // pool will reject auth RPCs with "Not connected to server".
        // Self-heal here so callers don't have to remember to retry —
        // common UX hazard when the user fixes the server URL after
        // initial offline boot.
        self.ensure_connected_for_auth().await?;
        // Every derivation below (login lookup hash, SVR salt, the stored
        // SVR_SALT_USERNAME) must see the nickname registration used, not
        // the `@nick.1234` handle the app displays.
        let username = crate::crypto::auth_crypto::login_nickname(username);
        // A priced login (server proof of work) takes seconds on a phone;
        // the login screen shows `AuthStateData::login_verifying` meanwhile.
        let auth_state = self.auth_state().clone();
        let login = self
            .auth()
            .do_login(username, password, device_name, pin, &move || {
                auth_state.set_login_verifying(true)
            })
            .await;
        self.auth_state().set_login_verifying(false);
        let mut result = login?;
        self.messages().reload_session_state().await;
        self.signal().clear_account_state();
        self.anon_groups().held_posts.clear();

        // ORDER IS THE FIX. `do_login` no longer mints an identity key, so a
        // reinstall arrives here with none and the SVR restore below actually
        // runs. It used to be the other way round: the mint happened first,
        // this restore saw a local identity, skipped, and the user's real key
        // was destroyed by the act of logging in to recover it.
        if result.key_recovery.is_some() {
            match pin {
                Some(p) => {
                    if let Err(error) = self.attempt_svr_restore(p, username, &mut result).await {
                        self.cleanup_failed_sign_in(result.device_id, &error, replacement_notice)
                            .await?;
                        return Err(error);
                    }
                }
                // Nothing to attempt. Left as NoPinProvided so the caller is
                // told to ask for the PIN rather than being handed a working
                // -looking session with no keys.
                None => {
                    tracing::warn!("no local identity and no PIN supplied — cannot restore keys")
                }
            }
        }

        // Refuse to report success while this device has no identity key.
        //
        // The previous design returned Ok with `keys_missing = true`, and every
        // caller in the tree — including the only UI login handler — discarded
        // the result. That is what made total, permanent key loss look like a
        // successful login. An Err cannot be discarded.
        if let Some(need) = result.key_recovery {
            if need.is_retryable() {
                // Keep the server-assigned device id so the next attempt can
                // reclaim it, but require a fresh password login for a new PIN.
                self.auth().end_session()?;
                self.auth_state().set_logged_out();
            }
            return Err(ClientError::IdentityRecoveryRequired(need));
        }

        let publish = async {
            let mut auth_client = self
                .auth()
                .try_auth_client()
                .ok_or(ClientError::NotConnected)?;
            let expected = self
                .signal()
                .get_identity_public_key()
                .await
                .map_err(read_identity_error)?;
            let owner = result.user_id.to_string();
            if !result.keys_restored
                && let OwnIdentityAtSignIn::Republish(published) = self
                    .own_identity_at_sign_in(
                        &mut auth_client,
                        &owner,
                        OwnIdentityCheck::Local(&expected),
                    )
                    .await?
            {
                let pin = pin.ok_or(AuthError::NotLoggedIn)?;
                if self
                    .prepare_interrupted_reset(pin, username, published, &expected)
                    .await?
                {
                    let proof = prove_password(&mut auth_client, password).await?;
                    self.republish_identity_keys(&mut auth_client, proof)
                        .await?;
                    self.verify_republished_identity(&mut auth_client, &owner, &expected)
                        .await?;
                } else {
                    result.keys_restored = true;
                }
            }
            self.signal()
                .ensure_device_bundle(&mut auth_client, result.device_id)
                .await
                .map_err(publish_key_material_error)?;
            self.auth().commit_login(&result)?;
            Ok::<(), ClientError>(())
        }.await;
        if let Err(error) = publish {
            if matches!(
                error,
                ClientError::IdentityRecoveryRequired(need) if !need.is_retryable()
            ) {
                return Err(error);
            }
            self.cleanup_failed_sign_in(result.device_id, &error, replacement_notice)
                .await?;
            return Err(error);
        }
        self.auth_state().set_logged_in(
            result.user_id,
            result.device_id,
            result.display_name.clone(),
        );

        // Our name, bio and avatar live on the server encrypted with our
        // profile key. A new device has none of them locally — the avatar
        // showed as initials while friends saw the photo — so restore them now
        // that the profile key is in place. Best-effort: a failed fetch must
        // not fail a login that otherwise worked.
        match self.users().get_user_profile(&result.user_id).await {
            Ok(profile) => {
                result.display_name = profile.display_name;
                self.auth_state().set_logged_in(
                    result.user_id,
                    result.device_id,
                    result.display_name.clone(),
                );
            }
            Err(e) => tracing::warn!(error = %e, "login: own profile restore failed"),
        }

        self.name_this_device(device_name).await;
        self.start().await?;
        Ok(result)
    }

    async fn cleanup_failed_sign_in(
        &self,
        device_id: u32,
        error: &ClientError,
        replacement_notice: impl std::future::Future<Output = ()>,
    ) -> Result<(), ClientError> {
        if matches!(error, ClientError::DeviceReplaced) {
            replacement_notice.await;
            return self.wipe_replaced_device(device_id).await;
        }
        self.auth().end_session()?;
        self.auth_state().set_logged_out();
        Ok(())
    }

    async fn wipe_replaced_device(&self, device_id: u32) -> Result<(), ClientError> {
        // Password auth may have created a provisional device even though
        // this installation has stale keys. Retire that device if reachable.
        if let Some(mut auth) = self.auth().try_auth_client() {
            match tokio::time::timeout(
                std::time::Duration::from_secs(5),
                auth.logout(device_id, None),
            )
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(error)) => tracing::warn!(%error, "Could not retire the provisional device"),
                Err(_) => tracing::warn!("Timed out retiring the provisional device"),
            }
        }
        self.clear_composer_session();
        self.auth_state().set_logged_out();
        self.auth()
            .wipe_local_account(|_| Ok(()))
            .await
            .map_err(AuthError::Storage)?;
        self.messages().reload_session_state().await;
        self.signal().clear_account_state();
        self.anon_groups().held_posts.clear();
        Ok(())
    }

    /// Give this device its name on the server, encrypted to the account
    /// identity key (Signal's encryptDeviceName). The server creates the
    /// device row unnamed — it may not read the name, so it cannot write it.
    /// Best-effort: an unnamed device is labelled by its platform string.
    async fn name_this_device(&self, device_name: &str) {
        let Some(mut auth_client) = self.auth().try_auth_client() else {
            tracing::warn!("no auth client; this device stays unnamed");
            return;
        };
        if let Err(e) = self
            .devices()
            .name_this_device(&mut auth_client, device_name)
            .await
        {
            tracing::warn!(error = %e, "could not set this device's encrypted name");
        }
    }

    /// Accept a NEW Signal identity key after key recovery has been ruled out.
    ///
    /// **Destructive and irreversible.** It abandons every existing session,
    /// makes past messages undecryptable, and changes the safety number every
    /// contact has verified. Only ever reach this from an explicit user
    /// decision, never from a retry loop or a background task — which is why
    /// `login` returns an error rather than doing this for the user.
    ///
    /// Refused while [`KeyRecoveryNeed::is_retryable`] holds: a wrong PIN, an
    /// unreachable SVR server or a PIN that was never entered all still lead
    /// back to the real key, and minting over it is exactly the silent
    /// destruction this whole change exists to stop.
    ///
    /// Upload the backup under the PIN before publishing the new identity.
    /// A stale backup requires a new PIN. The server publishes the identity
    /// and its signed prekeys together after the backup succeeds.
    pub async fn accept_new_identity(
        self: &Arc<Self>,
        need: KeyRecoveryNeed,
        password: &str,
        pin: &str,
        new_pin: Option<&str>,
    ) -> Result<(), ClientError> {
        let _lifecycle = self.auth_lifecycle_lock.lock().await;
        if need.is_retryable() {
            return Err(ClientError::Validation(
                chatter_i18n::trf(
                    "refusing to mint a new identity while the original may still be recoverable: {}",
                    &[&need.user_message()],
                )
                .into(),
            ));
        }

        if self.auth().is_logged_in() {
            return Err(AuthError::AlreadyLoggedIn.into());
        }
        if self.link_device().verify_pin(pin)
            != crate::services::link_device_service::PinVerification::Match
        {
            self.auth().end_session()?;
            return Err(ClientError::IdentityRecoveryRequired(
                KeyRecoveryNeed::WrongPin {
                    remaining_attempts: None,
                },
            ));
        }
        let change_to = if need == KeyRecoveryNeed::StaleBackup {
            let chosen = new_pin.ok_or_else(|| ClientError::Validation(need.user_message()))?;
            crate::crypto::svr::validate_pin(chosen)
                .map_err(|error| ClientError::Validation(error.to_string()))?;
            if chosen == pin {
                return Err(ClientError::Validation(
                    chatter_i18n::tr("New PIN must be different from current PIN").into(),
                ));
            }
            Some(chosen)
        } else {
            None
        };

        let mut auth_client = self
            .auth()
            .try_auth_client()
            .ok_or(ClientError::NotConnected)?;
        let (owner, _) = self.current_user().ok_or(AuthError::NotLoggedIn)?;
        let owner = owner.to_string();
        let has_identity = self
            .signal()
            .key_store
            .has_local_identity()
            .map_err(AuthError::Storage)?;
        if has_identity {
            let expected = self
                .signal()
                .get_identity_public_key()
                .await
                .map_err(read_identity_error)?;
            let status = self
                .own_identity_at_sign_in(
                    &mut auth_client,
                    &owner,
                    OwnIdentityCheck::Local(&expected),
                )
                .await;
            match status {
                Ok(OwnIdentityAtSignIn::Republish(_)) => {}
                Err(error) if !matches!(error, ClientError::DeviceReplaced) => return Err(error),
                _ => {
                    return Err(ClientError::Validation(
                        chatter_i18n::tr("Sign in again to finish publishing your encryption keys.").into(),
                    ));
                }
            }
        } else {
            self.own_identity_at_sign_in(&mut auth_client, &owner, OwnIdentityCheck::BeforeReset)
                .await?;
        }
        let proof = prove_password(&mut auth_client, password).await?;
        auth_client
            .verify_password(proof)
            .await
            .map_err(AuthError::Network)?;
        if !has_identity {
            self.auth().generate_identity_for_recovery()?;
        }

        let publication =
            publish_after_reset_backup(self.upload_identity_reset_backup(pin, change_to), async {
                let proof = prove_password(&mut auth_client, password).await?;
                self.republish_identity_keys(&mut auth_client, proof).await
            })
            .await;
        if matches!(
            publication,
            Err(ClientError::IdentityRecoveryRequired(
                KeyRecoveryNeed::WrongPin { .. }
            ))
        ) {
            self.auth().end_session()?;
            self.auth_state().set_logged_out();
        }
        publication?;
        let expected = self
            .signal()
            .get_identity_public_key()
            .await
            .map_err(read_identity_error)?;
        self.verify_republished_identity(&mut auth_client, &owner, &expected)
            .await?;
        self.auth().end_session()?;
        self.auth_state().set_logged_out();

        tracing::warn!(
            "minted and published a NEW identity key — every contact's safety number has changed"
        );
        Ok(())
    }

    async fn upload_identity_reset_backup(
        &self,
        pin: &str,
        new_pin: Option<&str>,
    ) -> Result<(), ClientError> {
        use zeroize::Zeroizing;
        let settings = &self.auth().settings_repo;
        let profile_key = Zeroizing::new(
            match settings
                .get_bytes(keys::PROFILE_KEY)
                .map_err(AuthError::Storage)?
            {
                Some(key) => <[u8; 32]>::try_from(key.as_slice()).map_err(|_| {
                    ClientError::Validation(
                        chatter_i18n::tr("Something went wrong. Try again.").into(),
                    )
                })?,
                None => {
                    let key = crate::crypto::auth_crypto::generate_profile_key();
                    settings
                        .set_bytes(keys::PROFILE_KEY, &key)
                        .map_err(AuthError::Storage)?;
                    key
                }
            },
        );
        let username = self.svr_salt_username()?;
        if let Some(next_pin) = new_pin {
            let handle = self
                .svr_backup()
                .identity_reset_username(&username)
                .map_err(publish_svr_backup_error)?;
            self.change_pin_with_handle(pin, next_pin, &username, &handle, false)
                .await?;
        } else {
            self.svr_backup()
                .backup_for_identity_reset(pin, &username, &profile_key)
                .await
                .map_err(publish_svr_backup_error)?;
        }
        self.auth()
            .set_unidentified_access_key(&profile_key)
            .await?;
        Ok(())
    }

    /// Resolve an interrupted reset from the signed predecessor and the current backup.
    /// Returns whether the staged identity still needs publication.
    async fn prepare_interrupted_reset(
        &self,
        pin: &str,
        username: &str,
        published: libsignal_protocol::IdentityKey,
        staged: &[u8; 33],
    ) -> Result<bool, ClientError> {
        let backup = if self
            .svr_backup()
            .has_key_backup()
            .await
            .map_err(publish_svr_backup_error)?
        {
            match self.fetch_svr_backup(pin, username).await {
                Ok(backup) => Some(backup),
                // The backup could disappear after the existence check.
                Err(SvrBackupError::BackupNotFound) => None,
                Err(error) => return Err(publish_svr_backup_error(error)),
            }
        } else {
            None
        };
        let Some(backup) = backup else {
            SvrBackupService::clear_local_backup_version(&self.auth().settings_repo)
                .map_err(publish_svr_backup_error)?;
            self.upload_identity_reset_backup(pin, None).await?;
            return Ok(true);
        };

        match reset_backup_identity(&backup, published, staged) {
            ResetBackupIdentity::Staged => {
                // A PIN change or backup upload already committed. The fetch
                // authenticated the supplied PIN, so no other guess is needed.
                verified_backup_private_key(&backup, &staged[1..])?;
                self.preserve_svr_profile(&backup)?;
                self.auth()
                    .set_unidentified_access_key(&backup.profile_key)
                    .await?;
                Ok(true)
            }
            ResetBackupIdentity::Published => {
                // A usable backup appeared since the reset was initiated.
                // Discard only the never-published identity and restore the
                // key that still owns the account.
                self.install_verified_svr_backup(published, &backup, true)?;
                Ok(false)
            }
            ResetBackupIdentity::Stale => {
                self.preserve_svr_profile(&backup)?;
                Err(ClientError::IdentityRecoveryRequired(
                    KeyRecoveryNeed::StaleBackup,
                ))
            }
        }
    }

    async fn fetch_svr_backup(
        &self,
        pin: &str,
        username: &str,
    ) -> Result<RestoreResult, SvrBackupError> {
        let secret = zeroize::Zeroizing::new(
            crate::crypto::svr::derive_svr_secret_async(pin, username).await?,
        );
        self.svr_backup().fetch_and_restore_backup(&secret).await
    }

    async fn own_identity_at_sign_in(
        &self,
        auth_client: &mut crate::network::grpc::AuthClient,
        owner: &str,
        check: OwnIdentityCheck<'_>,
    ) -> Result<OwnIdentityAtSignIn, ClientError> {
        let deadline = match self.services.grpc_pool.transport_mode() {
            TransportMode::Direct => kt_anchor::PIN_RPC_DEADLINE,
            TransportMode::Onion => kt_anchor::TOR_RPC_DEADLINE,
        };
        let status = kt_anchor::verify_own_identity_at_sign_in(
            auth_client.key_transparency_client(),
            &self.signal().settings_repo,
            owner,
            check,
            deadline,
        )
        .await
        .map_err(|error| {
            tracing::warn!(%error, "Could not verify the account's identity at sign-in");
            ClientError::Validation(
                chatter_i18n::tr("Could not verify the Key Transparency log history.").into(),
            )
        })?;
        match status {
            OwnIdentityAtSignIn::Replaced => Err(ClientError::DeviceReplaced),
            OwnIdentityAtSignIn::UnexpectedKey => Err(unexpected_own_identity()),
            status => Ok(status),
        }
    }

    async fn verify_republished_identity(
        &self,
        auth_client: &mut crate::network::grpc::AuthClient,
        owner: &str,
        expected: &[u8],
    ) -> Result<(), ClientError> {
        if matches!(
            self.own_identity_at_sign_in(auth_client, owner, OwnIdentityCheck::Local(expected))
                .await?,
            OwnIdentityAtSignIn::Republish(_)
        ) {
            return Err(ClientError::Validation(
                chatter_i18n::tr("Sign in again to finish publishing your encryption keys.").into(),
            ));
        }
        Ok(())
    }

    /// Replace the account's server-side key material with this device's.
    ///
    /// One RPC, applied atomically by the server: identity key, signed prekey,
    /// Kyber prekey and both one-time pools land together, and every other
    /// device's stale bundle is dropped. Publishing them separately is what
    /// produced the original corruption — an identity key from one generation
    /// served next to a signed prekey from another.
    async fn republish_identity_keys(
        self: &Arc<Self>,
        auth_client: &mut crate::network::grpc::AuthClient,
        proof: chatter_proto::chat_v1::PasswordProof,
    ) -> Result<(), ClientError> {
        let outcome = self
            .signal()
            .republish_identity_bundle(auth_client, proof)
            .await
            .map_err(publish_key_material_error)?;

        tracing::info!(
            identity_key_changed = outcome.identity_key_changed,
            devices_reset = outcome.devices_reset,
            "published this device's key material"
        );
        Ok(())
    }

    /// Register a new account and start all background services.
    ///
    /// PIN must satisfy [`crypto::svr::validate_pin`] if provided
    /// (8-64 printable ASCII chars).
    pub async fn register(
        self: &Arc<Self>,
        username: &str,
        password: &str,
        display_name: &str,
        device_name: &str,
        avatar: Option<&[u8]>,
        pin: Option<&str>,
    ) -> Result<LoginResult, ClientError> {
        let _lifecycle = self.auth_lifecycle_lock.lock().await;
        if self.started.load(std::sync::atomic::Ordering::SeqCst)
            || (self.auth_state().is_logged_in()
                && *self.authed_notif_session.lock()
                    == Some(self.auth_state().get().session_revision))
        {
            return Err(ClientError::AlreadyStarted);
        }
        if let Some(p) = pin {
            crate::crypto::svr::validate_pin(p)
                .map_err(|e| ClientError::Validation(e.to_string()))?;
        }
        // Self-heal a stale Disconnected state (see `login` for context).
        self.ensure_connected_for_auth().await?;
        let result = self
            .auth()
            .do_register(username, password, display_name, device_name, avatar, pin)
            .await?;
        self.auth_state().set_logged_in(
            result.user_id,
            result.device_id,
            result.display_name.clone(),
        );
        self.name_this_device(device_name).await;

        // Create SVR backup after successful registration (non-blocking on failure)
        if let Some(p) = pin {
            self.perform_svr_backup(p, username).await;
        }

        self.start().await?;

        // Push the encrypted avatar to the server profile so friends can fetch
        // + decrypt it via the profile-key handshake (same path display_name
        // already uses). `do_register` stored the raw bytes in `keys::AVATAR`;
        // `update_user_profile` reads them, encrypts with the profile key, and
        // uploads as `encrypted_avatar`. Best-effort — a failure here must not
        // fail registration.
        if avatar.is_some_and(|a| !a.is_empty()) {
            if let Err(e) = self.users().update_user_profile(None, None).await {
                tracing::warn!(error = %e, "Registration: encrypted-avatar upload failed");
            }
        }

        Ok(result)
    }

    /// Use a one-time recovery code to reset password and SVR credentials.
    ///
    /// This is called when the user has lost their password but has a recovery code
    /// from registration. It resets the password and SVR key material on the server.
    ///
    /// **The user's SVR key backup does not survive this.** The backup is
    /// encrypted with, and indexed by, a secret derived from the PIN alone
    /// (`Argon2id(PIN, username)`), so setting a new PIN leaves the stored row
    /// unreadable by everyone. The server drops it (auth `UseRecoveryCode`
    /// step 9b) rather than orphaning it, and this call clears the matching
    /// local version counter so the next backup can be written at version 1.
    ///
    /// If this account's identity keys existed only in that backup, they are
    /// gone: [`RecoveryResult::key_backup_invalidated`] says so, and the caller
    /// must tell the user rather than discarding it.
    ///
    /// (This used to claim "the SVR backup will be created automatically
    /// during that login flow". It is not: `login` only ever calls
    /// `attempt_svr_restore`, and the sole caller of `perform_svr_backup` is
    /// `register`.)
    ///
    /// # Arguments
    ///
    /// * `username` - The plaintext username (will be hashed before sending)
    /// * `recovery_code` - 12-character alphanumeric recovery code (A-Z, 2-9)
    /// * `new_password` - The new password to set
    /// * `new_pin` - The new PIN (8+ chars) for SVR key derivation
    pub async fn use_recovery_code(
        &self,
        username: &str,
        recovery_code: &str,
        new_password: &str,
        new_pin: &str,
    ) -> Result<RecoveryResult, ClientError> {
        // A signed-out start no longer connects at boot (`startup_connect`).
        self.ensure_connected_for_auth().await?;
        let mut auth_client = self
            .auth()
            .try_auth_client()
            .ok_or_else(|| ClientError::NotConnected)?;
        // Same keying as `login`: the bare nickname, whatever form was typed.
        let username = crate::crypto::auth_crypto::login_nickname(username);
        let remaining_codes = self
            .auth()
            .use_recovery_code(
                &mut auth_client,
                username,
                recovery_code,
                new_password,
                new_pin,
            )
            .await
            .map_err(ClientError::Auth)?;

        // The server just deleted the backup that was keyed to the OLD PIN, so
        // its stored version is back to 0 and `StoreBackup` will only accept 1.
        // A stale local counter would post N+1 and be rejected forever — the
        // deterministic ErrVersionConflict that this whole path used to end in.
        let settings = std::sync::Arc::new(
            crate::storage::repositories::settings_repo::SettingsRepository::new(
                self.services.database.clone(),
            ),
        );
        if let Err(e) =
            crate::services::svr_backup_service::SvrBackupService::clear_local_backup_version(
                &settings,
            )
        {
            // Not fatal to the recovery itself — the password IS reset — but it
            // must not be silent: every later backup on this device fails.
            tracing::error!(
                error = %e,
                "Recovery succeeded but the local SVR version counter could not be \
                 cleared; backups from this device will be rejected until it is"
            );
        }

        Ok(RecoveryResult {
            remaining_codes,
            key_backup_invalidated: true,
        })
    }

    /// Regenerate recovery codes for the authenticated user.
    ///
    /// Deletes all existing codes and generates 8 new ones.
    /// Requires password confirmation for security.
    ///
    /// # Arguments
    ///
    /// * `password` - The user's current password (will be hashed before sending)
    pub async fn regenerate_recovery_codes(
        &self,
        password: &str,
    ) -> Result<Vec<String>, ClientError> {
        let mut auth_client = self
            .auth()
            .try_auth_client()
            .ok_or_else(|| ClientError::NotConnected)?;
        let codes = self
            .auth()
            .regenerate_recovery_codes(&mut auth_client, password)
            .await
            .map_err(ClientError::Auth)?;
        let (user_id, _) = self.current_user().ok_or(ClientError::Auth(
            crate::services::auth_service::AuthError::NotLoggedIn,
        ))?;
        self.auth()
            .settings_repo
            .set(
                &keys::unacknowledged_recovery_codes_key(user_id),
                &PendingRecoveryCodes {
                    codes: codes.clone(),
                    regenerated: true,
                },
            )
            .map_err(|e| ClientError::Auth(e.into()))?;
        Ok(codes)
    }

    /// Create an SVR encrypted backup after registration.
    ///
    /// Orchestration lives in `SvrBackupService::backup_from_pin`; this is a
    /// non-failing wrapper that just logs outcome — registration success must
    /// not block on backup upload.
    async fn perform_svr_backup(&self, pin: &str, username: &str) {
        match self.svr_backup().backup_from_pin(pin, username).await {
            Ok(version) => tracing::info!(
                version,
                "SVR backup created successfully after registration"
            ),
            Err(e) => tracing::warn!("SVR backup failed (non-fatal): {}", e),
        }
    }

    /// The username every PIN derivation is salted with: exactly what was
    /// typed at register/login (`keys::SVR_SALT_USERNAME`). Not the display
    /// name and not the `.discriminator` handle — a different salt derives a
    /// different secret, and a backup written under it cannot be restored.
    pub(super) fn svr_salt_username(&self) -> Result<String, ClientError> {
        self.signal()
            .settings_repo
            .get_string(keys::SVR_SALT_USERNAME)
            .map_err(|e| ClientError::Validation(format!("failed to read login username: {e}")))?
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                ClientError::Validation(
                    "no stored login username — log out and log in once, then retry".into(),
                )
            })
    }

    /// Whether the server holds a key backup for this account.
    pub async fn has_key_backup(&self) -> Result<bool, ClientError> {
        self.svr_backup()
            .has_key_backup()
            .await
            .map_err(|e| ClientError::Validation(format!("backup status check failed: {e}")))
    }

    /// Create the SVR key backup if registration failed to upload it.
    ///
    /// Gated on the local PIN fingerprint, and refuses when a backup already
    /// exists: the upload is an unconditional replace, so without both checks
    /// anyone holding the unlocked device could re-key the backup to a PIN of
    /// their choosing.
    pub async fn retry_svr_backup(&self, pin: &str) -> Result<(), ClientError> {
        let verification = self.link_device().verify_pin(pin);
        if verification != crate::services::link_device_service::PinVerification::Match {
            return Err(ClientError::Validation(verification.message().into()));
        }
        if self.has_key_backup().await? {
            return Ok(());
        }
        let username = self.svr_salt_username()?;
        self.svr_backup()
            .retry_from_credentials(pin, &username)
            .await
            .map_err(|e| ClientError::Validation(format!("SVR backup upload failed: {}", e)))?;
        tracing::info!("SVR backup retry succeeded");
        Ok(())
    }

    /// Change the user's SVR PIN.
    ///
    /// Re-encrypts the identity key backup with a new PIN-derived secret and
    /// atomically updates the server-side backup, registration lock, and SVR
    /// public key. Both PINs must satisfy [`crypto::svr::validate_pin`].
    ///
    /// All sensitive intermediates (secrets, keys) are zeroized on every path.
    ///
    /// # Arguments
    ///
    /// * `old_pin` - Current PIN.
    /// * `new_pin` - New PIN (must differ from `old_pin`).
    /// * `username` - Plaintext username (salt for Argon2id derivation).
    pub async fn change_pin(&self, old_pin: &str, new_pin: &str) -> Result<(), ClientError> {
        let username = self.svr_salt_username()?;
        let handle = self
            .svr_backup()
            .local_full_username()
            .map_err(|e| ClientError::Validation(e.to_string()))?;
        self.change_pin_with_handle(old_pin, new_pin, &username, &handle, true)
            .await
    }

    async fn change_pin_with_handle(
        &self,
        old_pin: &str,
        new_pin: &str,
        username: &str,
        full_username: &str,
        create_if_missing: bool,
    ) -> Result<(), ClientError> {
        use crate::crypto::svr::{
            SvrKeypair, derive_registration_lock, derive_svr_secret, validate_pin,
            validate_pin_shape,
        };
        use zeroize::Zeroize;

        // Old PIN is the EXISTING one being verified — shape-only, or a user
        // whose current PIN is now blocklisted could never change it (§10).
        // The NEW PIN is being chosen, so it gets full strength validation.
        validate_pin_shape(old_pin)
            .map_err(|e| ClientError::Validation(format!("old PIN: {e}")))?;
        validate_pin(new_pin).map_err(|e| ClientError::Validation(format!("new PIN: {e}")))?;
        if old_pin == new_pin {
            return Err(ClientError::Validation(
                "New PIN must be different from current PIN".into(),
            ));
        }

        // 2. Get identity keypair from signal store (private 32 bytes + public 33 bytes)
        let mut private_bytes = self
            .signal()
            .actor
            .get_identity_private_key()
            .await
            .map_err(|e| {
                ClientError::Validation(format!("failed to get identity private key: {}", e))
            })?;
        let public_bytes_33 = self
            .signal()
            .actor
            .get_identity_public_key()
            .await
            .map_err(|e| {
                private_bytes.zeroize();
                ClientError::Validation(format!("failed to get identity public key: {}", e))
            })?;
        // Strip the 0x05 type prefix to get raw 32-byte Curve25519 public key
        let mut identity_keypair = [0u8; 64];
        identity_keypair[..32].copy_from_slice(&private_bytes);
        identity_keypair[32..].copy_from_slice(&public_bytes_33[1..]);
        private_bytes.zeroize();

        // 3. Get profile key from settings
        let mut profile_key = match self.signal().settings_repo.get_bytes(keys::PROFILE_KEY) {
            Ok(Some(bytes)) if bytes.len() == 32 => {
                let mut pk = [0u8; 32];
                pk.copy_from_slice(&bytes);
                pk
            }
            Ok(_) => {
                identity_keypair.zeroize();
                return Err(ClientError::Validation(
                    "profile key not found or invalid length".into(),
                ));
            }
            Err(e) => {
                identity_keypair.zeroize();
                return Err(ClientError::Validation(format!(
                    "failed to read profile key: {}",
                    e
                )));
            }
        };

        // 4. Derive old and new SVR secrets
        let mut old_svr_secret = derive_svr_secret(old_pin, username).map_err(|e| {
            identity_keypair.zeroize();
            profile_key.zeroize();
            ClientError::Validation(format!("failed to derive old svr_secret: {}", e))
        })?;
        let mut new_svr_secret = derive_svr_secret(new_pin, username).map_err(|e| {
            old_svr_secret.zeroize();
            identity_keypair.zeroize();
            profile_key.zeroize();
            ClientError::Validation(format!("failed to derive new svr_secret: {}", e))
        })?;

        // 5. Derive new registration lock
        let mut new_reg_lock = derive_registration_lock(new_pin, username).map_err(|e| {
            old_svr_secret.zeroize();
            new_svr_secret.zeroize();
            identity_keypair.zeroize();
            profile_key.zeroize();
            ClientError::Validation(format!("failed to derive registration lock: {}", e))
        })?;

        // 6. Derive new SVR public key
        let svr_keypair = SvrKeypair::new_from_pin(new_pin, username).map_err(|e| {
            old_svr_secret.zeroize();
            new_svr_secret.zeroize();
            new_reg_lock.zeroize();
            identity_keypair.zeroize();
            profile_key.zeroize();
            ClientError::Validation(format!("failed to derive SVR keypair: {}", e))
        })?;
        let new_svr_public_key = svr_keypair.public_key_bytes();
        // svr_keypair is dropped here (zeroizes secret via Drop impl)

        // 7. Call service to re-encrypt and upload
        //    If no backup exists yet, fall back to creating a new one with the new PIN.
        let result = self
            .svr_backup()
            .change_pin_backup(
                &old_svr_secret,
                &new_svr_secret,
                &identity_keypair,
                &profile_key,
                full_username,
                &new_reg_lock,
                &new_svr_public_key,
            )
            .await;

        let final_result = match &result {
            // NOT_FOUND means the account has no backup at all, so there is
            // nothing to re-encrypt: create one under the new PIN. A wrong
            // old PIN is the distinct WrongPin (§114) and is refused below.
            Err(SvrBackupError::BackupNotFound) if create_if_missing => {
                tracing::info!("No existing SVR backup found, creating new backup with new PIN");
                self.svr_backup()
                    .create_and_upload_backup(
                        &new_svr_secret,
                        &identity_keypair,
                        &profile_key,
                        full_username,
                    )
                    .await
                    .map(|_| ())
            }
            _ => result.map(|_| ()),
        };

        // 8. Zeroize ALL sensitive intermediates (both success and error paths)
        old_svr_secret.zeroize();
        new_svr_secret.zeroize();
        identity_keypair.zeroize();
        profile_key.zeroize();
        new_reg_lock.zeroize();

        final_result.map_err(|error| {
            if !create_if_missing || matches!(error, SvrBackupError::WrongPin { .. }) {
                publish_svr_backup_error(error)
            } else {
                ClientError::Validation(format!("PIN change failed: {error}"))
            }
        })?;

        // Refresh local PIN fingerprint so re-auth gates (linked-device approve)
        // accept the new PIN immediately. Best-effort: a stored fingerprint
        // mismatch only blocks the linked-device flow, not message send.
        match crate::crypto::svr::derive_pin_fingerprint(new_pin, username) {
            Ok(mut fp) => {
                if let Err(e) = self
                    .signal()
                    .settings_repo
                    .set_bytes(keys::PIN_FINGERPRINT, &fp)
                {
                    tracing::warn!(error = %e, "Failed to persist new PIN fingerprint");
                }
                fp.zeroize();
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to derive new PIN fingerprint");
            }
        }

        tracing::info!("PIN changed successfully");
        Ok(())
    }

    /// Attempt to restore identity keys from SVR backup on a new device.
    ///
    /// Checks whether local identity keys exist. If they do, this is an
    /// existing device and no restore is needed. If they are missing, derives
    /// the SVR secret from PIN + username and fetches the encrypted backup.
    ///
    /// On success, hydrates the local identity key store and profile key.
    /// On failure, sets `result.key_recovery` to the classified reason. That
    /// is not advisory: `login` turns it into an error, because a device with
    /// no identity key cannot send or receive anything and reporting success
    /// is what made the loss invisible.
    async fn attempt_svr_restore(
        &self,
        pin: &str,
        username: &str,
        result: &mut LoginResult,
    ) -> Result<(), ClientError> {
        // A backup fetch spends a PIN attempt, even when the PIN is correct.
        // Only the signed KT result can authorize reading the backup.
        let mut auth_client = self
            .auth()
            .try_auth_client()
            .ok_or(ClientError::NotConnected)?;
        let owner = result.user_id.to_string();
        let (published, backup) = fetch_after_own_key_verification(
            async {
                match self
                    .own_identity_at_sign_in(
                        &mut auth_client,
                        &owner,
                        OwnIdentityCheck::BeforeRestore,
                    )
                    .await?
                {
                    OwnIdentityAtSignIn::RestoreKey(key) => Ok(key),
                    _ => Err(unexpected_own_identity()),
                }
            },
            self.fetch_svr_backup(pin, username),
        )
        .await?;
        let backup = match backup {
            Ok(backup) => backup,
            Err(error) => {
                let need = classify_restore_failure(&error);
                if matches!(
                    need,
                    KeyRecoveryNeed::NoBackup | KeyRecoveryNeed::BackupDestroyed
                ) {
                    SvrBackupService::clear_local_backup_version(&self.auth().settings_repo)
                        .map_err(publish_svr_backup_error)?;
                }
                tracing::warn!(?need, "SVR restore failed");
                result.key_recovery = Some(need);
                return Ok(());
            }
        };

        if !backup_matches_identity(&backup, published.public_key().public_key_bytes()) {
            self.preserve_svr_profile(&backup)?;
            result.key_recovery = Some(KeyRecoveryNeed::StaleBackup);
            return Ok(());
        }

        self.install_verified_svr_backup(published, &backup, false)?;
        result.keys_restored = true;
        result.key_recovery = None;
        Ok(())
    }

    fn install_verified_svr_backup(
        &self,
        published: libsignal_protocol::IdentityKey,
        backup: &RestoreResult,
        replace_staged: bool,
    ) -> Result<(), ClientError> {
        let private_key =
            verified_backup_private_key(backup, published.public_key().public_key_bytes())?;
        let key_pair = libsignal_protocol::IdentityKeyPair::new(published, private_key);
        let registration_id = (rand::random::<u32>() % 0x3FFF) + 1;
        let key_store = &self.signal().key_store;
        if replace_staged {
            key_store
                .clear_local_identity()
                .map_err(AuthError::Storage)?;
        }
        key_store
            .set_local_identity(registration_id, &key_pair.serialize())
            .map_err(AuthError::Storage)?;
        let settings = &self.signal().settings_repo;
        settings
            .set_bytes(keys::IDENTITY_KEY_PUBLIC, published.serialize().as_ref())
            .map_err(AuthError::Storage)?;

        if let Err(error) = settings.set_bytes(keys::PROFILE_KEY, &backup.profile_key) {
            tracing::warn!(%error, "Could not save the restored profile key");
        }
        if backup.full_username.contains('.')
            && let Err(error) = settings.set_string(keys::FULL_USERNAME, &backup.full_username)
        {
            tracing::warn!(%error, "Could not save the restored account handle");
        }
        tracing::info!(
            remaining_attempts = backup.remaining_attempts,
            "SVR key restore completed"
        );
        Ok(())
    }

    fn preserve_svr_profile(&self, backup: &RestoreResult) -> Result<(), AuthError> {
        use crate::storage::repositories::SettingsRepository;
        use base64::Engine;
        let mut conn = self.auth().settings_repo.database().connection()?;
        let tx = conn
            .transaction()
            .map_err(crate::error::StorageError::from)?;
        // The outdated payload can still recover the profile and full handle.
        if SettingsRepository::get_with_connection::<String>(&tx, keys::PROFILE_KEY)?.is_none() {
            let encoded = zeroize::Zeroizing::new(
                base64::engine::general_purpose::STANDARD.encode(backup.profile_key),
            );
            SettingsRepository::set_with_connection(&tx, keys::PROFILE_KEY, &*encoded)?;
        }
        if backup.full_username.contains('.')
            && SettingsRepository::get_with_connection::<String>(&tx, keys::FULL_USERNAME)?
                .is_none()
        {
            SettingsRepository::set_with_connection(
                &tx,
                keys::FULL_USERNAME,
                &backup.full_username,
            )?;
        }
        tx.commit().map_err(crate::error::StorageError::from)?;
        Ok(())
    }

    /// Retire the device on the server, then shut down the local session.
    pub async fn logout(&self) -> Result<(), ClientError> {
        self.logout_with_retirement(self.auth().do_logout()).await
    }

    #[cfg(test)]
    pub(crate) async fn test_logout_after_retirement(&self) -> Result<(), ClientError> {
        self.logout_with_retirement(std::future::ready(Ok(()))).await
    }

    pub(super) async fn logout_with_retirement(
        &self,
        retire_device: impl std::future::Future<Output = Result<(), AuthError>>,
    ) -> Result<(), ClientError> {
        let expected = self.auth_state().get().session_revision;
        let _lifecycle = self.auth_lifecycle_lock.lock().await;
        if self.auth_state().get().session_revision != expected {
            return Ok(());
        }
        let presence = self.suspend_presence(expected).await;
        let previous = self.current_presence_for_publication().await;
        self.send_presence_updates(crate::domain::user::PresenceStatus::Offline)
            .await;
        if let Err(error) = retire_device.await {
            if self.auth().auth_interceptor.generation() == presence.generation
                && previous != crate::domain::user::PresenceStatus::Offline
            {
                self.send_presence_updates(previous).await;
            }
            return Err(error.into());
        }
        presence.commit();
        crate::system::messaging_background::set_signed_in(false);
        // Invalidate queued UI work immediately, then retire the old call owner
        // using its captured revision, before new credentials can be installed.
        self.clear_composer_session();
        let teardown_revision = self.auth_state().get().session_revision;
        self.retire_presence().await;
        let handles = std::mem::take(&mut *self.tasks.lock());
        for handle in &handles {
            handle.abort();
        }
        for handle in handles {
            let _ = handle.await;
        }
        let retired = self.authed_notif_session.lock().unwrap_or(expected);
        self.stop_authed_notification_streams(retired).await;
        #[cfg(feature = "media")]
        if let Some(worker) = self.call_service().get() {
            let actor_revision = worker
                .incoming_presentation_state()
                .session_revision()
                .unwrap_or(expected);
            if let Err(error) = worker.reset_presentation_session(actor_revision).await {
                tracing::error!(%error, "call session reset failed during logout");
            }
            if self.auth_state().get().session_revision != teardown_revision {
                return Ok(());
            }
            if let Some(signaling) = self.call_signaling.get() {
                signaling.clear_call_session();
            }
        }
        if self.auth_state().get().session_revision != teardown_revision {
            return Ok(());
        }
        if self.auth_state().get().session_revision == teardown_revision {
            self.shutdown_session(teardown_revision, false).await;
            self.auth_state()
                .set_logged_out_if_current(teardown_revision);
        }
        Ok(())
    }
}

async fn publish_after_reset_backup(
    backup: impl std::future::Future<Output = Result<(), ClientError>>,
    publish: impl std::future::Future<Output = Result<(), ClientError>>,
) -> Result<(), ClientError> {
    backup.await?;
    publish.await
}

fn publish_key_material_error(error: crate::error::ServiceError) -> ClientError {
    tracing::warn!(%error, "Could not publish the device's encryption keys");
    ClientError::Validation(
        chatter_i18n::tr("Sign in again to finish publishing your encryption keys.").into(),
    )
}

fn publish_svr_backup_error(error: SvrBackupError) -> ClientError {
    if matches!(
        error,
        SvrBackupError::WrongPin { .. }
            | SvrBackupError::RateLimited
            | SvrBackupError::BackupDestroyed
    ) {
        return ClientError::IdentityRecoveryRequired(classify_restore_failure(&error));
    }
    tracing::warn!(%error, "Could not prepare the replacement encryption backup");
    ClientError::Validation(
        chatter_i18n::tr("Sign in again to finish publishing your encryption keys.").into(),
    )
}

#[derive(Debug, PartialEq, Eq)]
enum ResetBackupIdentity {
    Published,
    Staged,
    Stale,
}

fn backup_matches_identity(backup: &RestoreResult, public_key: &[u8]) -> bool {
    &backup.identity_keypair[32..] == public_key
}

fn verified_backup_private_key(
    backup: &RestoreResult,
    expected: &[u8],
) -> Result<libsignal_protocol::PrivateKey, ClientError> {
    let unavailable = || ClientError::IdentityRecoveryRequired(KeyRecoveryNeed::Unavailable);
    let key = libsignal_protocol::PrivateKey::deserialize(&backup.identity_keypair[..32])
        .map_err(|_| unavailable())?;
    let public = key.public_key().map_err(|_| unavailable())?;
    if public.public_key_bytes() != expected {
        return Err(unavailable());
    }
    Ok(key)
}

fn reset_backup_identity(
    backup: &RestoreResult,
    published: libsignal_protocol::IdentityKey,
    staged: &[u8; 33],
) -> ResetBackupIdentity {
    if backup_matches_identity(backup, published.public_key().public_key_bytes()) {
        ResetBackupIdentity::Published
    } else if backup_matches_identity(backup, &staged[1..]) {
        ResetBackupIdentity::Staged
    } else {
        ResetBackupIdentity::Stale
    }
}

fn read_identity_error(error: crate::error::ServiceError) -> ClientError {
    tracing::warn!(%error, "Could not read the local encryption identity");
    ClientError::Validation(chatter_i18n::tr("Something went wrong. Try again.").into())
}

fn unexpected_own_identity() -> ClientError {
    ClientError::Validation(
        chatter_i18n::tr("The server published an unexpected encryption key for your account.")
            .into(),
    )
}

async fn fetch_after_own_key_verification<T>(
    verify: impl std::future::Future<Output = Result<libsignal_protocol::IdentityKey, ClientError>>,
    fetch: impl std::future::Future<Output = T>,
) -> Result<(libsignal_protocol::IdentityKey, T), ClientError> {
    let key = verify.await?;
    Ok((key, fetch.await))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::device_credentials::DeviceToken;
    use crate::domain::ids::UserId;

    #[tokio::test]
    async fn failed_kt_verification_never_polls_the_pin_backup_fetch() {
        for error in [unexpected_own_identity(), ClientError::NotConnected] {
            let outcome = fetch_after_own_key_verification::<()>(async { Err(error) }, async {
                panic!("an unverified sign-in must never spend a backup attempt");
            })
            .await;
            assert!(outcome.is_err());
        }
    }

    #[tokio::test]
    async fn identity_reset_waits_for_the_backup_upload_before_publishing() {
        assert!(
            publish_after_reset_backup(async { Err(ClientError::NotConnected) }, async {
                panic!("failed backup upload must leave the published identity alone")
            },)
            .await
            .is_err()
        );

        let uploaded = std::cell::Cell::new(false);
        publish_after_reset_backup(
            async {
                uploaded.set(true);
                Ok(())
            },
            async {
                assert!(uploaded.get());
                Ok(())
            },
        )
        .await
        .unwrap();
    }

    #[test]
    fn reset_recovery_uses_the_current_backup_identity() {
        use rand::TryRngCore;
        let mut rng = rand::rngs::OsRng.unwrap_err();
        let published = libsignal_protocol::IdentityKeyPair::generate(&mut rng);
        let staged = libsignal_protocol::IdentityKeyPair::generate(&mut rng);
        let obsolete = libsignal_protocol::IdentityKeyPair::generate(&mut rng);
        let staged_public = staged
            .identity_key()
            .serialize()
            .as_ref()
            .try_into()
            .unwrap();
        let mut backup = RestoreResult {
            identity_keypair: [0; 64],
            profile_key: [0; 32],
            full_username: String::new(),
            remaining_attempts: 3,
        };
        for (current, expected) in [
            (obsolete, ResetBackupIdentity::Stale),
            (staged, ResetBackupIdentity::Staged),
            (published, ResetBackupIdentity::Published),
        ] {
            backup.identity_keypair[32..].copy_from_slice(current.public_key().public_key_bytes());
            assert_eq!(
                reset_backup_identity(&backup, *published.identity_key(), &staged_public),
                expected,
            );
        }
        assert!(!KeyRecoveryNeed::StaleBackup.is_retryable());
    }

    #[tokio::test]
    async fn wrong_pin_for_a_reset_reports_attempts_and_never_publishes() {
        let error = publish_svr_backup_error(SvrBackupError::WrongPin {
            remaining_attempts: Some(2),
        });
        let result = publish_after_reset_backup(async { Err(error) }, async {
            panic!("an unverified PIN must leave the published identity unchanged");
        })
        .await;
        assert!(matches!(
            result,
            Err(ClientError::IdentityRecoveryRequired(
                KeyRecoveryNeed::WrongPin {
                    remaining_attempts: Some(2),
                }
            ))
        ));
    }

    #[tokio::test]
    async fn only_identity_reset_can_back_up_a_missing_full_handle() {
        let dir = tempfile::tempdir().unwrap();
        let client = ChatterClient::builder()
            .server_url("https://localhost:1")
            .data_dir(dir.path().to_str().unwrap())
            .build()
            .unwrap();
        let backup = client.svr_backup();
        assert!(backup.local_full_username().is_err());
        assert_eq!(backup.identity_reset_username("owner").unwrap(), "owner");
        client
            .auth()
            .settings_repo
            .set_string(keys::FULL_USERNAME, "owner.123")
            .unwrap();
        assert_eq!(backup.local_full_username().unwrap(), "owner.123");
        assert_eq!(
            backup.identity_reset_username("owner").unwrap(),
            "owner.123"
        );
    }

    #[tokio::test]
    async fn a_replaced_device_signs_out_and_wipes_its_local_install() {
        let dir = tempfile::tempdir().unwrap();
        let client = ChatterClient::builder()
            .server_url("https://localhost:1")
            .data_dir(dir.path().to_str().unwrap())
            .build()
            .unwrap();
        let owner = UserId::new();
        let settings = &client.auth().settings_repo;
        crate::services::auth_service::cache_saved_messages_channel(settings, owner).unwrap();
        settings
            .set_string(keys::CURRENT_USER_ID, &owner.to_string())
            .unwrap();
        settings.set(keys::CURRENT_DEVICE_ID, &3u32).unwrap();
        client
            .auth()
            .auth_interceptor
            .set_basic(format!("{owner}.3"), DeviceToken::generate().to_base64());
        client.auth().generate_identity_for_recovery().unwrap();
        let files = settings.database().path().with_file_name("attachments");
        std::fs::create_dir_all(&files).unwrap();
        let attachment = files.join("old-account.enc");
        std::fs::write(&attachment, b"fixture").unwrap();

        client
            .cleanup_failed_sign_in(3, &unexpected_own_identity(), async {})
            .await
            .unwrap();
        assert!(client.signal().key_store.has_local_identity().unwrap());
        assert!(attachment.exists());

        client
            .cleanup_failed_sign_in(3, &ClientError::DeviceReplaced, async {
                assert!(attachment.exists(), "notice must precede the wipe");
            })
            .await
            .unwrap();

        assert_eq!(client.current_user(), None);
        assert!(client.auth().auth_interceptor.get_auth_info().is_none());
        assert!(!client.signal().key_store.has_local_identity().unwrap());
        assert_eq!(
            crate::storage::unlink::local_account_owner(settings.database()).unwrap(),
            None
        );
        assert!(!attachment.exists());
    }
}

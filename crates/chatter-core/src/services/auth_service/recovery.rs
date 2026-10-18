use super::*;

impl AuthService {
    /// Mint a Signal identity key for a device that has none, after key
    /// recovery has been ruled out.
    ///
    /// Separate from `register`'s mint on purpose: this runs against an
    /// account that already exists on the server, so the new key must be
    /// PUBLISHED (see `SignalService::republish_identity_bundle`) or the
    /// account is left serving a key nobody holds — the exact corruption this
    /// change exists to eliminate. Callers must publish immediately after.
    ///
    /// `generate_identity` refuses to overwrite an existing identity, so this
    /// cannot silently destroy a working device.
    pub fn generate_identity_for_recovery(&self) -> Result<(), AuthError> {
        let (_, identity_key_pair) = self
            .identity_manager
            .generate_identity(&mut rand::rngs::OsRng.unwrap_err())?;

        // Friend-code generation reads this without touching the SignalStore;
        // leaving the previous device's key here would mint codes for a key
        // this device cannot prove it holds.
        self.settings_repo.set_bytes(
            keys::IDENTITY_KEY_PUBLIC,
            identity_key_pair.identity_key().serialize().as_ref(),
        )?;

        Ok(())
    }
    /// Change the user's password.
    ///
    /// One OPAQUE exchange proving the current password and one OPAQUE
    /// registration for the new one, in two round trips. The login identifier
    /// does not change (it is nickname-derived). The server revokes every
    /// other device session on success.
    pub async fn change_password(
        &self,
        current_password: &str,
        new_password: &str,
    ) -> Result<(), AuthError> {
        if !self.is_logged_in() {
            return Err(AuthError::NotLoggedIn);
        }
        let mut auth_client = self.try_auth_client().ok_or_else(|| {
            AuthError::Network(NetworkError::ConnectionFailed(
                "Not connected to server".to_string(),
            ))
        })?;
        change_password_with(&mut auth_client, current_password, new_password).await
    }

    /// Redeem a recovery code to reset password + PIN.
    ///
    /// Validates inputs, derives the new SVR material from `(new_pin,
    /// username)` (`username` is the bare nickname, still needed to salt it),
    /// and runs the two-step redemption: the code plus a fresh OPAQUE
    /// registration for `new_password`. Returns the number of recovery codes
    /// that remain on the account. All sensitive intermediates are zeroized
    /// before return.
    pub async fn use_recovery_code(
        &self,
        auth_client: &mut AuthClient,
        username: &str,
        recovery_code: &str,
        new_password: &str,
        new_pin: &str,
    ) -> Result<i32, AuthError> {
        use zeroize::Zeroize;

        crate::crypto::svr::validate_pin(new_pin)
            .map_err(|e| AuthError::InvalidInput(e.to_string()))?;
        let recovery_code = normalize_recovery_code(recovery_code).ok_or_else(|| {
            AuthError::InvalidInput(format!(
                "Recovery code must be {RECOVERY_CODE_LEN} characters (letters and digits 2-9)"
            ))
        })?;

        let keypair = crate::crypto::svr::SvrKeypair::new_from_pin(new_pin, username)?;
        let svr_public_key = keypair.public_key_bytes();

        let mut registration_lock =
            crate::crypto::svr::derive_registration_lock(new_pin, username)?;

        // The server finds the account by the code itself. The new password is
        // a fresh OPAQUE registration; the login identifier stays as it was.
        let opaque_reg = opaque::registration_start(new_password)?;
        let result = async {
            let started = auth_client
                .use_recovery_code_start(&recovery_code, &opaque_reg.request)
                .await?;
            let record = opaque::registration_finish_async(
                opaque_reg,
                new_password,
                started.registration_response,
            )
            .await?;
            auth_client
                .use_recovery_code_finish(
                    &started.recovery_id,
                    &record,
                    &svr_public_key,
                    &registration_lock,
                )
                .await
                .map_err(AuthError::Network)
        }
        .await;

        registration_lock.zeroize();

        // The grant lets the login with the new password through even while
        // someone keeps the account's login guess ceiling full.
        let finished = result?;
        if !finished.login_grant.is_empty() {
            *self.recovery_login_grant.lock() =
                Some((login_identifier(username), finished.login_grant));
        }
        Ok(finished.remaining_codes)
    }

    /// Regenerate the caller's recovery codes.
    ///
    /// Proves the password (OPAQUE, [`prove_password`]) and spends the proof
    /// on the server RPC. Returns the newly issued codes (caller must display
    /// them exactly once — codes are never recoverable from the server).
    pub async fn regenerate_recovery_codes(
        &self,
        auth_client: &mut AuthClient,
        password: &str,
    ) -> Result<Vec<String>, AuthError> {
        let proof = prove_password(auth_client, password).await?;
        auth_client
            .regenerate_recovery_codes(proof)
            .await
            .map_err(AuthError::Network)
    }
}

/// Password change against `auth_client` (authenticated): KE1 for the current
/// password plus the new password's RegistrationRequest, then KE3 plus the new
/// RegistrationRecord. A wrong current password fails client-side, before the
/// second round trip, with [`AuthError::InvalidCredentials`].
async fn change_password_with(
    auth_client: &mut AuthClient,
    current_password: &str,
    new_password: &str,
) -> Result<(), AuthError> {
    let proof = opaque::login_start(current_password)?;
    let reg = opaque::registration_start(new_password)?;
    let started = auth_client
        .change_password_start(&proof.ke1, &reg.request)
        .await?;
    let done = opaque::login_finish_async(proof, current_password, started.ke2).await?;
    let record =
        opaque::registration_finish_async(reg, new_password, started.registration_response).await?;
    auth_client
        .change_password_finish(&started.flow_id, &done.ke3, &record)
        .await
        .map_err(|e| match &e {
            NetworkError::Unauthenticated => AuthError::InvalidCredentials,
            _ => AuthError::Network(e),
        })
}

/// Length of a recovery code (server `recoveryCodeLength`): 26 symbols from a
/// 31-symbol alphabet, ~128.8 bits. The code alone identifies the account at
/// recovery, so it has to be a full-strength secret.
pub const RECOVERY_CODE_LEN: usize = 26;
const RECOVERY_CODE_ALPHABET: &str = "ABCDEFGHJKMNPQRSTUVWXYZ23456789";

/// Reduce a typed recovery code to the canonical form the server stores a
/// digest of: separators (`-`, spaces) dropped, letters uppercased. `None`
/// when the result is not exactly [`RECOVERY_CODE_LEN`] alphabet symbols.
pub fn normalize_recovery_code(input: &str) -> Option<String> {
    let code: String = input
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .map(|c| c.to_ascii_uppercase())
        .collect();
    (code.len() == RECOVERY_CODE_LEN && code.chars().all(|c| RECOVERY_CODE_ALPHABET.contains(c)))
        .then_some(code)
}

#[cfg(test)]
mod recovery_code_tests {
    use super::*;

    #[test]
    fn normalize_accepts_grouped_lowercase_and_spaced_input() {
        let canonical = "ABCDEFGHJKMNPQRSTUVWXYZ234";
        assert_eq!(
            normalize_recovery_code(canonical).as_deref(),
            Some(canonical)
        );
        assert_eq!(
            normalize_recovery_code(" abcde-fghjk-mnpqr-stuvw-xyz234 ").as_deref(),
            Some(canonical)
        );
        assert_eq!(
            normalize_recovery_code("ABCDE FGHJK MNPQR STUVW XYZ234").as_deref(),
            Some(canonical)
        );
    }

    #[test]
    fn normalize_rejects_wrong_length_and_ambiguous_symbols() {
        // The old 12-character format is no longer a code.
        assert!(normalize_recovery_code("ABCDEFGHJKMN").is_none());
        assert!(normalize_recovery_code("ABCDEFGHJKMNPQRSTUVWXYZ2345").is_none());
        // 0/O/1/I/L are excluded from the alphabet.
        assert!(normalize_recovery_code("0BCDEFGHJKMNPQRSTUVWXYZ234").is_none());
        assert!(normalize_recovery_code("IBCDEFGHJKMNPQRSTUVWXYZ234").is_none());
        assert!(normalize_recovery_code("").is_none());
    }
}

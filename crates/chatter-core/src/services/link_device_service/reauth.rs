use super::*;

impl LinkDeviceService {
    /// Local constant-time PIN check. An absent fingerprint permits the
    /// caller to use the metered SVR oracle; corrupt state does not.
    pub fn verify_pin(&self, pin: &str) -> PinVerification {
        use PinVerification::{Error, Match, Mismatch, NotSetUpOnThisDevice};
        use PinVerificationError::*;
        use subtle::ConstantTimeEq;
        if crate::crypto::svr::validate_pin_shape(pin).is_err() {
            return Error(InvalidInput);
        }
        let salt = match self.settings_repo.get_string(keys::SVR_SALT_USERNAME) {
            Ok(Some(s)) if !s.trim().is_empty() => s,
            Ok(_) => return NotSetUpOnThisDevice,
            Err(_) => return Error(Storage),
        };
        let stored = match self.settings_repo.get_bytes(keys::PIN_FINGERPRINT) {
            Ok(Some(fp)) => fp,
            Ok(None) => return NotSetUpOnThisDevice,
            Err(_) => return Error(Storage),
        };
        if stored.len() != 32 {
            return Error(InvalidFingerprint);
        }
        let derived = match crate::crypto::svr::derive_pin_fingerprint(pin, &salt) {
            Ok(fp) => Zeroizing::new(fp),
            Err(_) => return Error(Crypto),
        };
        if derived.ct_eq(stored.as_slice()).unwrap_u8() == 1 {
            Match
        } else {
            Mismatch
        }
    }

    /// Convenience: verify EITHER a password OR a PIN. Tries the local PIN
    /// check first when the input looks like a PIN; otherwise falls through to
    /// the password check (a server OPAQUE proof). Lets the UI keep a single
    /// "confirm with password or PIN" input.
    pub async fn verify_password_or_pin(&self, secret: &str) -> Result<bool, LinkDeviceError> {
        // Try PIN first (cheap local HMAC compare against the stored
        // fingerprint). PIN and password share format constraints now (both
        // are 8+ printable ASCII), so we can't tell them apart by shape -
        // we attempt PIN, and fall through to password if PIN validation
        // rejects the input or the fingerprint doesn't match.
        if crate::crypto::svr::validate_pin_shape(secret).is_ok() {
            if self.verify_pin(secret) == PinVerification::Match {
                return Ok(true);
            }
        }
        self.verify_password(secret).await
    }

    /// Re-auth gate for the password: an OPAQUE login against the account's
    /// own record (PasswordProofStart), spent on VerifyPassword.
    ///
    /// The device holds nothing that could check a password offline: OPAQUE
    /// needs the server's OPRF evaluation, and keeping a local
    /// password-derived fingerprint would put an offline-crackable value on
    /// the device. Each attempt is one guess on this device's own server-side
    /// password-proof budget (not the account's login guess ceiling, so
    /// approving a link keeps working while logins are capped; server
    /// OPEN-ITEMS §133). `Ok(false)` for a wrong password; network
    /// failures are errors, never a silent "no".
    pub async fn verify_password(&self, password: &str) -> Result<bool, LinkDeviceError> {
        let channel = self
            .grpc_pool
            .try_channel()
            .ok_or(LinkDeviceError::Network(NetworkError::ConnectionFailed(
                "Not connected to server".into(),
            )))?;
        let mut auth = AuthClient::new(channel, self.grpc_pool.auth_clone());
        match prove_password(&mut auth, password).await {
            Ok(proof) => match auth.verify_password(proof).await {
                Ok(()) => Ok(true),
                Err(NetworkError::Unauthenticated) => Ok(false),
                Err(e) => Err(e.into()),
            },
            Err(AuthError::InvalidCredentials) => Ok(false),
            Err(AuthError::Network(e)) => Err(e.into()),
            Err(AuthError::Crypto(e)) => Err(e.into()),
            Err(e) => Err(LinkDeviceError::Invalid(e.to_string())),
        }
    }
}

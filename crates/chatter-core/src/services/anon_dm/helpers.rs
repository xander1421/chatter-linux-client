use super::{AnonDmError, AnonDmService};

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

    /// Get encrypted avatar for sharing during bootstrap.
    ///
    /// Encrypts avatar with profile key for privacy-preserving sharing.
    pub(crate) fn get_encrypted_avatar(&self, profile_key: &[u8; 32]) -> Option<Vec<u8>> {
        use crate::storage::repositories::settings_keys;
        let avatar = self
            .settings_repo
            .get_bytes(settings_keys::AVATAR)
            .ok()
            .flatten()?;
        crate::crypto::profile_crypto::encrypt_avatar(profile_key, &avatar).ok()
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

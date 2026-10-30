use super::*;

impl UserService {
    // ========================================================================
    // Friend Code Operations (Privacy-first discovery)
    // ========================================================================

    /// Generate a friend code URL for the current user.
    ///
    /// Format: `chatter://connect/{user_id}`
    ///
    /// This code can be shared or encoded as a QR code. It only contains
    /// the user ID - no username is exposed. Profile key is exchanged
    /// via E2E message after connection is established.
    pub fn current_friend_code(&self) -> Result<String, UserError> {
        let current_user = self.settings_repo.current_user_id()?;
        Ok(format!("chatter://connect/{}", current_user))
    }

    pub fn generate_friend_code(&self) -> Result<String, UserError> {
        self.current_friend_code()
    }

    /// Generate a QR code image (PNG) containing the friend code URL.
    ///
    /// Returns PNG image bytes that can be displayed in the UI.
    /// The QR code encodes `chatter://connect/{user_id}`.
    ///
    /// # Arguments
    ///
    /// * `size` - Size of the QR code in pixels (width = height)
    pub fn generate_friend_qr(&self, size: u32) -> Result<Vec<u8>, UserError> {
        use image::{ImageEncoder, Luma};
        use qrcode::QrCode;

        let code_url = self.generate_friend_code()?;

        // Generate QR code
        let qr = QrCode::new(code_url.as_bytes())
            .map_err(|e| UserError::InvalidInput(format!("Failed to generate QR code: {}", e)))?;

        // Render to image with quiet zone
        let qr_image = qr.render::<Luma<u8>>().quiet_zone(true).build();

        // Scale to requested size
        let scaled =
            image::imageops::resize(&qr_image, size, size, image::imageops::FilterType::Nearest);

        // Encode as PNG
        let mut png_bytes = Vec::new();
        let encoder = image::codecs::png::PngEncoder::new(&mut png_bytes);
        encoder
            .write_image(scaled.as_raw(), size, size, image::ExtendedColorType::L8)
            .map_err(|e| UserError::InvalidInput(format!("Failed to encode PNG: {}", e)))?;

        Ok(png_bytes)
    }

    // ========================================================================
    // NDSS'21 Private Friend Codes (includes identity key + delivery token)
    // ========================================================================

    /// Generate a private friend code for NDSS'21 compliant anonymous DM.
    ///
    /// Format: `chatter://private/{base32(user_id || device_id || identity_key || delivery_token)}`
    ///
    /// This code embeds all cryptographic material needed for initial contact:
    /// - User ID (16 bytes)
    /// - This device's ID (4 bytes): a secret chat is bound to one device, so
    ///   a request made with this code reaches this device and no other
    /// - Identity key (33 bytes - 0x05 + Curve25519)
    /// - Delivery token (12 bytes - derived from profile key)
    ///
    /// # Privacy Guarantees
    ///
    /// When someone scans this code and connects:
    /// - NO prekey fetch is required (keys are embedded)
    /// - Server learns NOTHING about who is contacting whom
    /// - Initial message is HPKE-encrypted to identity key
    ///
    /// # Security Note
    ///
    /// This code contains your identity key. Only share with people you want
    /// to be able to contact you privately. The code can be regenerated with
    /// a new profile key to invalidate old delivery tokens.
    pub fn current_private_friend_code(&self) -> Result<String, UserError> {
        use crate::domain::PrivateFriendCode;

        let current_user = self.settings_repo.current_user_id()?;
        let device_id: u32 = self
            .settings_repo
            .get(keys::CURRENT_DEVICE_ID)?
            .ok_or(UserError::NotLoggedIn)?;

        // Get identity key
        let identity_key_bytes = self
            .settings_repo
            .get_bytes(keys::IDENTITY_KEY_PUBLIC)?
            .ok_or(UserError::NotLoggedIn)?;

        if identity_key_bytes.len() != 33 {
            return Err(UserError::InvalidInput(format!(
                "Invalid identity key length: {}",
                identity_key_bytes.len()
            )));
        }

        let mut identity_key = [0u8; 33];
        identity_key.copy_from_slice(&identity_key_bytes);

        // Get profile key and derive delivery token
        let profile_key_bytes = self
            .settings_repo
            .get_bytes(keys::PROFILE_KEY)?
            .ok_or(UserError::NotLoggedIn)?;

        if profile_key_bytes.len() != 32 {
            return Err(UserError::InvalidInput(format!(
                "Invalid profile key length: {}",
                profile_key_bytes.len()
            )));
        }

        let mut profile_key = [0u8; 32];
        profile_key.copy_from_slice(&profile_key_bytes);
        let delivery_token = PrivateFriendCode::derive_delivery_token(&profile_key);

        // Build the private friend code
        let code = PrivateFriendCode::new(current_user, device_id, identity_key, delivery_token);

        Ok(code.to_url())
    }

    pub fn generate_private_friend_code(&self) -> Result<String, UserError> {
        self.current_private_friend_code()
    }

    /// Generate a QR code for private friend code.
    ///
    /// Returns PNG image bytes encoding the private friend code URL.
    pub fn generate_private_friend_qr(&self, size: u32) -> Result<Vec<u8>, UserError> {
        use image::{ImageEncoder, Luma};
        use qrcode::QrCode;

        let code_url = self.generate_private_friend_code()?;

        // Generate QR code (private codes are longer, may need higher error correction)
        let qr = QrCode::with_error_correction_level(code_url.as_bytes(), qrcode::EcLevel::M)
            .map_err(|e| {
                UserError::InvalidInput(format!("Failed to generate private QR code: {}", e))
            })?;

        // Render to image with quiet zone
        let qr_image = qr.render::<Luma<u8>>().quiet_zone(true).build();

        // Scale to requested size
        let scaled =
            image::imageops::resize(&qr_image, size, size, image::imageops::FilterType::Nearest);

        // Encode as PNG
        let mut png_bytes = Vec::new();
        let encoder = image::codecs::png::PngEncoder::new(&mut png_bytes);
        encoder
            .write_image(scaled.as_raw(), size, size, image::ExtendedColorType::L8)
            .map_err(|e| UserError::InvalidInput(format!("Failed to encode PNG: {}", e)))?;

        Ok(png_bytes)
    }

    // ========================================================================
    // Username Link Operations (Signal-style, L6)
    // ========================================================================
    //
    // A username link is a shareable `chatter://u/{link_id}#{entropy}` URL/QR
    // that resolves to our username WITHOUT the server learning it: the blob
    // uploaded is libsignal `usernames::create_for_username` ciphertext, and
    // the decryption entropy only ever travels in the URL fragment. See
    // `crate::domain::username_link`.

    /// Create (or rotate) the current user's username link and return the
    /// shareable URL. Setting a new link replaces the old one server-side, so
    /// previously shared URLs stop resolving — that IS the reset button.
    pub async fn create_username_link(&self) -> Result<String, UserError> {
        let _ = self.settings_repo.current_user_id()?;
        let full_username = self
            .settings_repo
            .get_string(keys::FULL_USERNAME)?
            .filter(|u| !u.is_empty())
            .ok_or_else(|| {
                // Shown to the person as-is (`user_message`), so it is copy.
                UserError::InvalidInput(
                    "your username isn't stored on this device, so a link can't be made here"
                        .to_string(),
                )
            })?;

        let (entropy, blob) = usernames::create_for_username(
            &mut rand::rngs::OsRng.unwrap_err(),
            full_username,
            None,
        )
        .map_err(|e| UserError::InvalidInput(format!("Failed to encrypt username: {e}")))?;

        let mut user_client = self.user_client();
        let link_id = user_client.set_username_link(blob).await?;

        let url = crate::domain::UsernameLink { link_id, entropy }.to_url();
        self.settings_repo
            .set_string(keys::USERNAME_LINK_URL, &url)?;
        Ok(url)
    }

    /// The locally stored username-link URL, if one was created on this
    /// device. `None` means "no active link" (never created or deleted).
    pub fn username_link_local(&self) -> Result<Option<String>, UserError> {
        Ok(self.settings_repo.get_string(keys::USERNAME_LINK_URL)?)
    }

    /// Render a username-link URL as a square PNG QR code.
    pub fn username_link_qr(url: &str, size: u32) -> Result<Vec<u8>, UserError> {
        use image::{ImageEncoder, Luma};
        use qrcode::QrCode;

        let qr = QrCode::new(url.as_bytes())
            .map_err(|e| UserError::InvalidInput(format!("Failed to generate QR code: {}", e)))?;
        let qr_image = qr.render::<Luma<u8>>().quiet_zone(true).build();
        let scaled =
            image::imageops::resize(&qr_image, size, size, image::imageops::FilterType::Nearest);
        let mut png_bytes = Vec::new();
        let encoder = image::codecs::png::PngEncoder::new(&mut png_bytes);
        encoder
            .write_image(scaled.as_raw(), size, size, image::ExtendedColorType::L8)
            .map_err(|e| UserError::InvalidInput(format!("Failed to encode PNG: {}", e)))?;
        Ok(png_bytes)
    }

    /// Resolve someone else's username link: parse the URL, fetch the
    /// ciphertext blob and the link owner's id by link id, and decrypt the
    /// blob locally with the fragment entropy. Returns the decrypted username
    /// and the owner's account id.
    ///
    /// The owner id comes from the link row, NOT from `LookupUsername`: a link
    /// is a capability like a friend code, so it must still reach an owner
    /// who set "Who can find me by my username" to Nobody. Decryption still
    /// runs first, so a URL with the wrong fragment fails as before.
    pub async fn resolve_username_link(&self, url: &str) -> Result<(String, UserId), UserError> {
        let link = crate::domain::UsernameLink::parse(url)
            .map_err(|e| UserError::InvalidInput(format!("Invalid username link: {e}")))?;

        let mut user_client = self.user_client();
        let (owner_id, blob) = user_client
            .get_username_link(&link.link_id)
            .await?
            .ok_or_else(|| {
                UserError::InvalidInput(
                    "This username link is no longer active (deleted or reset)".to_string(),
                )
            })?;

        let username = usernames::decrypt_username(&link.entropy, &blob).map_err(|_| {
            UserError::InvalidInput(
                "Could not decrypt this username link (corrupted or wrong key)".to_string(),
            )
        })?;

        let user_id = owner_id.parse::<UserId>().map_err(|_| {
            UserError::InvalidInput(format!("Invalid user ID in username link: {owner_id}"))
        })?;
        if let Err(e) = self.user_repo.set_known_username(&user_id, &username) {
            tracing::warn!(error = %e, "Could not remember linked peer handle");
        }
        Ok((username, user_id))
    }

    /// Delete the current user's username link (server row + local copy).
    pub async fn delete_username_link(&self) -> Result<(), UserError> {
        let _ = self.settings_repo.current_user_id()?;
        let mut user_client = self.user_client();
        user_client.delete_username_link().await?;
        self.settings_repo.delete(keys::USERNAME_LINK_URL)?;
        Ok(())
    }

    /// Parse a friend code and extract the user ID.
    ///
    /// Accepts formats:
    /// - `chatter://connect/{user_id}` (URL format)
    /// - `{user_id}` (plain UUID)
    pub fn parse_friend_code(code: &str) -> Result<UserId, UserError> {
        let code = code.trim();

        // Try URL format first
        if let Some(id_str) = code.strip_prefix("chatter://connect/") {
            return id_str
                .parse::<UserId>()
                .map_err(|_| UserError::InvalidInput("This code isn't valid.".into()));
        }

        // Try plain UUID
        code.parse::<UserId>()
            .map_err(|_| UserError::InvalidInput("This code isn't valid.".into()))
    }

    /// Connect with a user via friend code.
    ///
    /// This initiates the friend request flow using only the user ID.
    /// The profile key will be exchanged via E2E message once connected.
    ///
    /// # Arguments
    ///
    /// * `code` - Friend code (URL or plain user ID)
    /// * `message` - Optional friend request message
    pub async fn connect_via_friend_code(
        &self,
        code: &str,
        message: Option<&str>,
    ) -> Result<Friendship, UserError> {
        // Username links (`chatter://u/…#…`) resolve to their owner via
        // fetch-link → local decrypt; plain friend codes carry the user id
        // directly. Neither goes through the discoverability-gated lookup.
        let friend_id = if crate::domain::UsernameLink::is_username_link(code) {
            self.resolve_username_link(code).await?.1
        } else {
            Self::parse_friend_code(code)?
        };

        // Check we're not adding ourselves
        let current_user = self.settings_repo.current_user_id()?;
        if friend_id == current_user {
            return Err(UserError::InvalidInput(
                "Cannot add yourself as a friend".to_string(),
            ));
        }

        // Check if already a friend. A local Pending is not checked: a
        // recipient's decline is never sent back, so it can be stale, and the
        // server rejects a request that really is still pending.
        if let Ok(Some(existing)) = self.user_repo.get_friendship(&current_user, &friend_id) {
            if existing.status == FriendshipStatus::Accepted {
                return Err(UserError::InvalidInput(
                    "Already friends with this user".to_string(),
                ));
            }
        }

        // Send friend request
        self.send_friend_request(&friend_id, message).await
    }
}

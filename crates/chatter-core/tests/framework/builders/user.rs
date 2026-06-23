//! User builder for creating test users.

use crate::framework::fixtures::{
    TestUser, unique_username, random_password, random_public_key,
    random_device_auth_token, random_profile_key,
};

/// Builder for creating test users.
#[derive(Default)]
pub struct UserBuilder {
    username: Option<String>,
    display_name: Option<String>,
    password: Option<String>,
    user_id: Option<String>,
    device_id: Option<u32>,
    profile_key: Option<[u8; 32]>,
}

impl UserBuilder {
    /// Create a new user builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a user builder with a specific display name.
    pub fn named(name: impl Into<String>) -> Self {
        Self {
            display_name: Some(name.into()),
            ..Default::default()
        }
    }

    /// Set the username.
    pub fn username(mut self, username: impl Into<String>) -> Self {
        self.username = Some(username.into());
        self
    }

    /// Set the display name.
    pub fn display_name(mut self, name: impl Into<String>) -> Self {
        self.display_name = Some(name.into());
        self
    }

    /// Set the password.
    pub fn password(mut self, password: impl Into<String>) -> Self {
        self.password = Some(password.into());
        self
    }

    /// Set the user ID.
    pub fn user_id(mut self, id: impl Into<String>) -> Self {
        self.user_id = Some(id.into());
        self
    }

    /// Set the device ID.
    pub fn device_id(mut self, id: u32) -> Self {
        self.device_id = Some(id);
        self
    }

    /// Set the profile key.
    pub fn profile_key(mut self, key: [u8; 32]) -> Self {
        self.profile_key = Some(key);
        self
    }

    /// Build the test user.
    pub fn build(self) -> TestUser {
        let username = self.username.unwrap_or_else(unique_username);
        let display_name = self.display_name.unwrap_or_else(|| {
            // Use suffix of username if long enough, otherwise use whole username
            if username.len() >= 10 {
                format!("Test User {}", &username[10..18.min(username.len())])
            } else {
                format!("Test User {}", &username)
            }
        });
        let (password, password_hash) = self.password
            .map(|p| {
                use sha2::{Digest, Sha256};
                let hash = Sha256::digest(p.as_bytes()).to_vec();
                (p, hash)
            })
            .unwrap_or_else(random_password);

        let profile_key = self.profile_key.unwrap_or_else(random_profile_key);

        // Encrypt the display name with the profile key (returns Vec<u8>)
        let encrypted_display_name = chatter_core::crypto::profile_crypto::encrypt_display_name(
            &profile_key,
            &display_name,
        )
        .expect("Display name encryption should succeed");

        let full_username = format!("{}.42", username);
        TestUser {
            username,
            full_username,
            display_name,
            encrypted_display_name, // Vec<u8>
            password,
            password_hash,
            public_key: random_public_key(),
            identity_key_pair_bytes: Vec::new(),
            device_auth_token: random_device_auth_token(),
            profile_key,
            user_id: self.user_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            device_id: self.device_id.unwrap_or(1),
        }
    }

    /// Build multiple users.
    pub fn build_many(count: usize) -> Vec<TestUser> {
        (0..count).map(|i| {
            Self::new()
                .display_name(format!("User {}", i + 1))
                .build()
        }).collect()
    }

    /// Build Alice and Bob pair (common test pattern).
    pub fn build_pair() -> (TestUser, TestUser) {
        (
            Self::named("Alice").build(),
            Self::named("Bob").build(),
        )
    }

    /// Build Alice, Bob, and Charlie trio.
    pub fn build_trio() -> (TestUser, TestUser, TestUser) {
        (
            Self::named("Alice").build(),
            Self::named("Bob").build(),
            Self::named("Charlie").build(),
        )
    }
}

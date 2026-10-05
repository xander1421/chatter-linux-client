//! User builder for creating test users.

use crate::framework::fixtures::{
    TestUser, random_device_auth_token, random_password, random_profile_key, random_public_key,
    unique_username,
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
        let password = self.password.unwrap_or_else(random_password);

        let profile_key = self.profile_key.unwrap_or_else(random_profile_key);

        let full_username = format!("{}.42", username);
        TestUser {
            username,
            full_username,
            display_name,
            password,
            public_key: random_public_key(),
            identity_key_pair_bytes: Vec::new(),
            device_auth_token: random_device_auth_token(),
            profile_key,
            user_id: self
                .user_id
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            device_id: self.device_id.unwrap_or(1),
        }
    }

    /// Build Alice and Bob pair (common test pattern).
    pub fn build_pair() -> (TestUser, TestUser) {
        (Self::named("Alice").build(), Self::named("Bob").build())
    }
}

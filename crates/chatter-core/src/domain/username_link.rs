//! Username link (Signal-style, L6): a shareable URL/QR that resolves to a
//! username without the server ever learning it.
//!
//! # How it works
//!
//! libsignal's `usernames` crate (`create_for_username`) encrypts the owner's
//! full `nickname.discriminator` username with AES-256-CBC + HMAC-SHA256, both
//! keys HKDF'd from 32 bytes of entropy. The ciphertext blob is uploaded to
//! the server, which files it under an opaque, server-generated UUID link id.
//! The entropy travels ONLY in the URL fragment:
//!
//! ```text
//! chatter://u/{link_id}#{base64url_nopad(entropy)}
//! ```
//!
//! URL fragments are never sent in requests, so the server (and anyone who
//! steals its database) holds undecryptable ciphertext. A recipient parses the
//! URL, fetches the blob by link id, decrypts locally
//! (`usernames::decrypt_username`), and then discovers the account through the
//! existing hashed `LookupUsername` path.
//!
//! Rotation: creating a new link replaces the old blob AND the old link id
//! server-side, so previously shared URLs stop resolving.

/// Number of entropy bytes in the URL fragment (libsignal's
/// `USERNAME_LINK_ENTROPY_SIZE`).
pub const ENTROPY_SIZE: usize = 32;

/// A parsed username link: server-side handle + client-side decryption entropy.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct UsernameLink {
    /// Opaque server-generated link id (UUID string).
    pub link_id: String,
    /// Fragment entropy; HKDF source for the AES + HMAC keys. Never sent to
    /// the server.
    pub entropy: [u8; ENTROPY_SIZE],
}

/// Error parsing a username-link URL.
#[derive(Debug, thiserror::Error)]
pub enum UsernameLinkParseError {
    /// Not a `chatter://u/` URL or missing the `#fragment`.
    #[error("invalid username link format: {0}")]
    InvalidFormat(String),

    /// Fragment is not valid base64url or not exactly 32 bytes.
    #[error("invalid username link entropy: {0}")]
    InvalidEntropy(String),
}

impl UsernameLink {
    /// URL prefix for username links.
    pub const URL_PREFIX: &'static str = "chatter://u/";

    /// Render as a shareable `chatter://u/{link_id}#{entropy}` URL.
    pub fn to_url(&self) -> String {
        use base64::Engine;
        format!(
            "{}{}#{}",
            Self::URL_PREFIX,
            self.link_id,
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(self.entropy)
        )
    }

    /// Quick shape check: does `s` look like a username link URL?
    pub fn is_username_link(s: &str) -> bool {
        s.trim().starts_with(Self::URL_PREFIX)
    }

    /// Parse a `chatter://u/{link_id}#{base64url(entropy)}` URL.
    pub fn parse(url: &str) -> Result<Self, UsernameLinkParseError> {
        use base64::Engine;

        let rest = url.trim().strip_prefix(Self::URL_PREFIX).ok_or_else(|| {
            UsernameLinkParseError::InvalidFormat(format!("expected {}… URL", Self::URL_PREFIX))
        })?;

        let (link_id, fragment) = rest.split_once('#').ok_or_else(|| {
            UsernameLinkParseError::InvalidFormat(
                "missing #entropy fragment — this link cannot be decrypted".into(),
            )
        })?;
        if link_id.is_empty() {
            return Err(UsernameLinkParseError::InvalidFormat(
                "empty link id".into(),
            ));
        }

        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(fragment)
            .map_err(|e| UsernameLinkParseError::InvalidEntropy(e.to_string()))?;
        let entropy: [u8; ENTROPY_SIZE] = bytes.try_into().map_err(|v: Vec<u8>| {
            UsernameLinkParseError::InvalidEntropy(format!(
                "expected {} bytes, got {}",
                ENTROPY_SIZE,
                v.len()
            ))
        })?;

        Ok(Self {
            link_id: link_id.to_string(),
            entropy,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> UsernameLink {
        UsernameLink {
            link_id: "2f1d94a1-6bb5-4bcb-9d63-1d5c9db1b9a1".into(),
            entropy: [7u8; ENTROPY_SIZE],
        }
    }

    #[test]
    fn url_round_trip() {
        let link = sample();
        let url = link.to_url();
        assert!(url.starts_with("chatter://u/2f1d94a1-"));
        assert!(url.contains('#'));
        let parsed = UsernameLink::parse(&url).expect("round trip");
        assert_eq!(parsed, link);
    }

    #[test]
    fn rejects_wrong_scheme_missing_fragment_bad_entropy() {
        assert!(UsernameLink::parse("chatter://connect/abc").is_err());
        assert!(UsernameLink::parse("chatter://u/abc").is_err()); // no fragment
        assert!(UsernameLink::parse("chatter://u/#AAAA").is_err()); // no id
        assert!(UsernameLink::parse("chatter://u/abc#not!base64").is_err());
        assert!(UsernameLink::parse("chatter://u/abc#AAAA").is_err()); // wrong len
    }

    #[test]
    fn is_username_link_shape_check() {
        assert!(UsernameLink::is_username_link(&sample().to_url()));
        assert!(UsernameLink::is_username_link("  chatter://u/x#y  "));
        assert!(!UsernameLink::is_username_link("chatter://connect/x"));
        assert!(!UsernameLink::is_username_link("alice.42"));
    }

    /// PROOF (a): offline round-trip against libsignal itself — the entropy +
    /// blob our create path produces decrypt back to the exact username, and
    /// (the privacy half) the blob is undecryptable without the right entropy.
    #[test]
    fn libsignal_round_trip_and_wrong_entropy_fails() {
        let username = "proof_user.42".to_string();
        let mut rng = rand::rng();
        let (entropy, blob) = usernames::create_for_username(&mut rng, username.clone(), None)
            .expect("create_for_username");

        // Round trip: decrypts back byte-exact.
        let decrypted = usernames::decrypt_username(&entropy, &blob).expect("decrypt");
        assert_eq!(decrypted, username);

        // The blob never contains the plaintext username (server-side view).
        assert!(
            !blob
                .windows(username.len())
                .any(|w| w == username.as_bytes()),
            "ciphertext blob must not contain the plaintext username"
        );

        // Wrong entropy (what a server or link thief without the fragment
        // has): HMAC mismatch, decryption MUST fail.
        let mut wrong = entropy;
        wrong[0] ^= 0x01;
        assert!(
            usernames::decrypt_username(&wrong, &blob).is_err(),
            "decryption with wrong entropy must fail"
        );
    }
}

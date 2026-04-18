//! Call verification service.
//!
//! Provides Short Authentication String (SAS) verification for E2EE calls.
//! Users can compare emojis verbally to verify they're in a secure call.
//!
//! # Design
//!
//! Similar to Signal's call verification, we derive a set of emojis from
//! the shared encryption key. Both parties see the same emojis if the
//! call is properly encrypted end-to-end.
//!
//! # Usage
//!
//! ```ignore
//! let emojis = CallVerification::derive_verification_emojis(&shared_key, &call_id);
//! // Display: "🔒 Verify: 🐶 🌸 🚀 🎸"
//! // Both users should see the same emojis
//! ```

use hkdf::Hkdf;
use sha2::Sha256;

// HKDF is used here directly (variable-length output sized by
// `VERIFICATION_EMOJI_COUNT`) so we don't go through `crypto::kdf::hkdf_sha256`
// which uses a compile-time const-generic length.

/// Salt for SAS emoji derivation.
const SAS_SALT: &[u8] = b"ChatterCallSAS-v1";

/// Info prefix for SAS derivation.
const SAS_INFO_PREFIX: &[u8] = b"call-sas:";

/// Number of emojis to display for verification.
pub const VERIFICATION_EMOJI_COUNT: usize = 4;

/// Emoji set for call verification (64 distinct, easily distinguishable emojis).
/// Similar to Signal's emoji set for safety numbers.
/// Each emoji includes the variation selector (U+FE0F) to ensure colorful rendering.
const VERIFICATION_EMOJIS: [&str; 64] = [
    // Animals (all have emoji presentation by default, but adding VS16 for consistency)
    "🐶\u{FE0F}", "🐱\u{FE0F}", "🐭\u{FE0F}", "🐹\u{FE0F}", "🐰\u{FE0F}", "🦊\u{FE0F}", "🐻\u{FE0F}", "🐼\u{FE0F}",
    "🐨\u{FE0F}", "🐯\u{FE0F}", "🦁\u{FE0F}", "🐮\u{FE0F}", "🐷\u{FE0F}", "🐸\u{FE0F}", "🐵\u{FE0F}", "🐔\u{FE0F}",
    // Nature
    "🌸\u{FE0F}", "🌺\u{FE0F}", "🌻\u{FE0F}", "🌹\u{FE0F}", "🌴\u{FE0F}", "🌵\u{FE0F}", "🍀\u{FE0F}", "🍁\u{FE0F}",
    "🌙\u{FE0F}", "⭐\u{FE0F}", "🌈\u{FE0F}", "☀\u{FE0F}", "❄\u{FE0F}", "🔥\u{FE0F}", "💧\u{FE0F}", "🌊\u{FE0F}",
    // Objects
    "🚀\u{FE0F}", "✈\u{FE0F}", "🚂\u{FE0F}", "🚗\u{FE0F}", "⛵\u{FE0F}", "🎸\u{FE0F}", "🎺\u{FE0F}", "🎹\u{FE0F}",
    "📱\u{FE0F}", "💻\u{FE0F}", "🔑\u{FE0F}", "💎\u{FE0F}", "🎁\u{FE0F}", "🎈\u{FE0F}", "🎯\u{FE0F}", "🏆\u{FE0F}",
    // Food
    "🍎\u{FE0F}", "🍊\u{FE0F}", "🍋\u{FE0F}", "🍇\u{FE0F}", "🍓\u{FE0F}", "🍕\u{FE0F}", "🍔\u{FE0F}", "🍩\u{FE0F}",
    // Symbols
    "❤\u{FE0F}", "💙\u{FE0F}", "💚\u{FE0F}", "💜\u{FE0F}", "🖤\u{FE0F}", "⚡\u{FE0F}", "💫\u{FE0F}", "🔔\u{FE0F}",
];

/// Call verification data for UI display.
#[derive(Debug, Clone)]
pub struct CallVerificationData {
    /// The verification emojis to display.
    pub emojis: Vec<String>,
    /// Human-readable emoji string for display.
    pub display_string: String,
    /// Whether verification has been confirmed by user.
    pub verified: bool,
}

/// Call verification service.
pub struct CallVerification;

impl CallVerification {
    /// Derive verification emojis from call encryption key.
    ///
    /// Both parties will get the same emojis if they have the same key,
    /// allowing them to verbally verify the call is secure.
    ///
    /// # Arguments
    /// * `encryption_key` - The 32-byte call encryption key
    /// * `call_id` - Unique call identifier (ensures different calls get different emojis)
    ///
    /// # Returns
    /// CallVerificationData with emojis for display.
    pub fn derive_verification_emojis(
        encryption_key: &[u8; 32],
        call_id: &str,
    ) -> CallVerificationData {
        // Derive SAS bytes using HKDF
        let hkdf = Hkdf::<Sha256>::new(Some(SAS_SALT), encryption_key);

        let mut info = SAS_INFO_PREFIX.to_vec();
        info.extend_from_slice(call_id.as_bytes());

        // We need VERIFICATION_EMOJI_COUNT bytes, each maps to one of 64 emojis
        let mut sas_bytes = vec![0u8; VERIFICATION_EMOJI_COUNT];
        hkdf.expand(&info, &mut sas_bytes)
            .expect("HKDF expand should not fail for small output");

        // Map bytes to emojis (each byte mod 64 gives emoji index)
        let emojis: Vec<String> = sas_bytes
            .iter()
            .map(|&b| VERIFICATION_EMOJIS[(b % 64) as usize].to_string())
            .collect();

        let display_string = emojis.join(" ");

        CallVerificationData {
            emojis,
            display_string,
            verified: false,
        }
    }

    /// Get the verification emoji at a specific index.
    ///
    /// Useful for displaying emojis one-by-one or highlighting.
    pub fn get_emoji(index: u8) -> &'static str {
        VERIFICATION_EMOJIS[(index % 64) as usize]
    }

    /// Convert verification data to a format suitable for Slint.
    ///
    /// Returns a tuple of (emoji1, emoji2, emoji3, emoji4) strings.
    pub fn to_slint_tuple(data: &CallVerificationData) -> (String, String, String, String) {
        let e = &data.emojis;
        (
            e.first().cloned().unwrap_or_default(),
            e.get(1).cloned().unwrap_or_default(),
            e.get(2).cloned().unwrap_or_default(),
            e.get(3).cloned().unwrap_or_default(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_derive_verification_emojis() {
        let key = [1u8; 32];
        let call_id = "test-call-123";

        let data = CallVerification::derive_verification_emojis(&key, call_id);

        assert_eq!(data.emojis.len(), VERIFICATION_EMOJI_COUNT);
        assert!(!data.display_string.is_empty());
    }

    #[test]
    fn test_same_inputs_same_output() {
        let key = [42u8; 32];
        let call_id = "call-456";

        let data1 = CallVerification::derive_verification_emojis(&key, call_id);
        let data2 = CallVerification::derive_verification_emojis(&key, call_id);

        assert_eq!(data1.emojis, data2.emojis);
        assert_eq!(data1.display_string, data2.display_string);
    }

    #[test]
    fn test_different_keys_different_output() {
        let key1 = [1u8; 32];
        let key2 = [2u8; 32];
        let call_id = "call-789";

        let data1 = CallVerification::derive_verification_emojis(&key1, call_id);
        let data2 = CallVerification::derive_verification_emojis(&key2, call_id);

        // Very unlikely to be the same
        assert_ne!(data1.emojis, data2.emojis);
    }

    #[test]
    fn test_different_call_ids_different_output() {
        let key = [1u8; 32];

        let data1 = CallVerification::derive_verification_emojis(&key, "call-1");
        let data2 = CallVerification::derive_verification_emojis(&key, "call-2");

        assert_ne!(data1.emojis, data2.emojis);
    }

    #[test]
    fn test_get_emoji() {
        // Test that get_emoji returns valid emojis
        for i in 0..64 {
            let emoji = CallVerification::get_emoji(i);
            assert!(!emoji.is_empty());
        }

        // Test wrapping (index 64 should give same as 0)
        assert_eq!(
            CallVerification::get_emoji(0),
            CallVerification::get_emoji(64)
        );
    }

    #[test]
    fn test_to_slint_tuple() {
        let key = [1u8; 32];
        let data = CallVerification::derive_verification_emojis(&key, "test");

        let (e1, e2, e3, e4) = CallVerification::to_slint_tuple(&data);

        assert!(!e1.is_empty());
        assert!(!e2.is_empty());
        assert!(!e3.is_empty());
        assert!(!e4.is_empty());
    }
}

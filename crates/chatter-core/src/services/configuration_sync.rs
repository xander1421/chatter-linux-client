//! `ConfigurationSync`: our privacy settings, kept the same on every one of
//! our devices (Signal's `SyncMessage.Configuration`).
//!
//! Whenever this device changes the read-receipt or typing-indicator setting
//! it sends the full current set to our own other devices; a device that
//! receives one from our own user adopts every value and never answers. One
//! from anyone else is dropped: a peer must never switch our receipts on.
//!
//! Signal's message also carries `unidentifiedDeliveryIndicators` and
//! `linkPreviews`; Chatter has no such settings, so the message does not.

use crate::domain::ids::UserId;
use crate::error::StorageError;
use crate::proto::content_v1::{ConfigurationSync, Content, content};
use crate::services::current_user::CurrentUserProvider;
use crate::storage::repositories::settings_repo::{SettingsRepository, keys};

/// The settings as they stand on this device (each defaults to on, as the
/// settings screen and the receipt path read them).
#[must_use]
pub fn current(settings: &SettingsRepository) -> ConfigurationSync {
    let read = |key| settings.get_bool(key).ok().flatten().unwrap_or(true);
    ConfigurationSync {
        read_receipts: read(keys::SEND_READ_RECEIPTS),
        typing_indicators: read(keys::SHOW_TYPING_INDICATORS),
    }
}

/// The `Content` to send our other devices, stamped `sent_at`.
#[must_use]
pub fn content(settings: &SettingsRepository, sent_at: u64) -> Content {
    Content {
        timestamp: sent_at,
        content: Some(content::Content::ConfigurationSync(current(settings))),
        ..Default::default()
    }
}

/// Adopt a `ConfigurationSync` sealed by `sender`. Returns `false`, writing
/// nothing, when `sender` is not our own user.
pub fn apply(
    settings: &SettingsRepository,
    sender: &UserId,
    sync: &ConfigurationSync,
) -> Result<bool, StorageError> {
    if settings.current_user_id().ok().as_ref() != Some(sender) {
        tracing::warn!(sender_id = %sender, "ConfigurationSync from another user dropped");
        return Ok(false);
    }
    settings.set_bool(keys::SEND_READ_RECEIPTS, sync.read_receipts)?;
    settings.set_bool(keys::SHOW_TYPING_INDICATORS, sync.typing_indicators)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::Database;
    use std::sync::Arc;

    fn settings_for(me: &UserId) -> (tempfile::TempDir, SettingsRepository) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Arc::new(Database::open(&dir.path().join("c.db"), "testpass").expect("db"));
        db.run_migrations().expect("migrations");
        let settings = SettingsRepository::new(db);
        settings
            .set_string(keys::CURRENT_USER_ID, &me.to_string())
            .expect("me");
        (dir, settings)
    }

    /// What one of our devices builds, another of ours adopts.
    #[test]
    fn build_and_apply_round_trip() {
        use prost::Message as _;
        let me = UserId::new();
        let (_a, sender) = settings_for(&me);
        let (_b, receiver) = settings_for(&me);
        assert_eq!(
            current(&receiver),
            ConfigurationSync {
                read_receipts: true,
                typing_indicators: true
            }
        );

        sender
            .set_bool(keys::SEND_READ_RECEIPTS, false)
            .expect("set");
        let wire = content(&sender, 7).encode_to_vec();
        let Some(content::Content::ConfigurationSync(sync)) =
            Content::decode(wire.as_slice()).expect("decode").content
        else {
            panic!("a ConfigurationSync");
        };

        assert!(apply(&receiver, &me, &sync).expect("apply"));
        assert_eq!(
            current(&receiver),
            ConfigurationSync {
                read_receipts: false,
                typing_indicators: true
            }
        );
    }

    #[test]
    fn a_sync_from_another_user_is_rejected() {
        let me = UserId::new();
        let (_dir, settings) = settings_for(&me);
        let forged = ConfigurationSync {
            read_receipts: false,
            typing_indicators: false,
        };

        assert!(!apply(&settings, &UserId::new(), &forged).expect("apply"));
        assert_eq!(
            current(&settings),
            ConfigurationSync {
                read_receipts: true,
                typing_indicators: true
            },
            "nothing written"
        );
    }
}

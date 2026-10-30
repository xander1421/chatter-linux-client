//! What a login must do when this device holds no Signal identity key.
//!
//! # The failure this exists to prevent
//!
//! A fresh install ("I lost my phone, restore with my PIN") used to mint a
//! brand-new Signal identity key inside `login` BEFORE the SVR restore ran.
//! The restore then saw a local identity, concluded this was an existing
//! device, logged `SVR restore skipped: local identity keys already exist`, and
//! returned — so the PIN backup, the entire point of the flow, was never
//! fetched and the user's real identity key was lost.
//!
//! Nothing published the new key either: neither `LoginRequest` nor
//! `RefreshPreKeysRequest` carries an identity key, and the server's prekey
//! upload path refuses any bundle whose identity key differs from the one on
//! file. Meanwhile the background replenisher cheerfully overwrote the server's
//! signed prekey with one signed by the NEW key. The account was then serving
//! the OLD identity key next to a signed prekey signed by the NEW one:
//! signature verification failed for every peer, forever, with no error on
//! either side.
//!
//! # The rule
//!
//! An identity key is minted only when it is *established* that the original
//! cannot be recovered, and never without the user being told. Everything else
//! — a wrong PIN, an unreachable server, a PIN that was never supplied — is a
//! situation where the real key is still out there, so minting would destroy
//! it. [`KeyRecoveryNeed::is_retryable`] is that distinction.

use crate::services::svr_backup_service::SvrBackupError;

/// Why a login could not establish this device's Signal identity key.
///
/// Carried out of `login` as an error rather than a flag on a success result:
/// the previous design set `keys_missing = true` on an `Ok`, and every caller
/// in the tree ignored it, which is how the loss stayed silent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyRecoveryNeed {
    /// No PIN was supplied, so the SVR backup was never even attempted.
    NoPinProvided,
    /// The backup exists but did not decrypt. Overwhelmingly a wrong PIN: the
    /// PIN is the only input to the key that opens it.
    WrongPin,
    /// The account has no SVR backup at all. The original identity key is
    /// genuinely unrecoverable, so a new one is the only way forward — but the
    /// user has to choose it, because their contacts will see the change.
    NoBackup,
    /// The server destroyed the backup after too many wrong PINs. Same
    /// finality as [`Self::NoBackup`], different cause, and worth saying so.
    BackupDestroyed,
    /// Too many recent restore attempts. The backup is intact; only waiting is
    /// required.
    RateLimited,
    /// The SVR server could not be reached, or local storage failed. Says
    /// nothing about whether a backup exists.
    Unavailable,
}

impl KeyRecoveryNeed {
    /// True when the original identity key may still be recoverable, so
    /// minting a replacement would throw away a live key.
    ///
    /// This is the guard on [`crate::client::ChatterClient::accept_new_identity`]:
    /// a new identity is a destructive, irreversible act that breaks every
    /// existing session and changes every contact's safety number. It is only
    /// correct once recovery has been *ruled out*, not merely failed once.
    pub fn is_retryable(self) -> bool {
        match self {
            // A PIN that was never entered, entered wrongly, or that the
            // server is currently refusing to check — all still lead back to
            // the real key.
            Self::NoPinProvided | Self::WrongPin | Self::RateLimited | Self::Unavailable => true,
            // There is nothing left to recover from.
            Self::NoBackup | Self::BackupDestroyed => false,
        }
    }

    /// What to tell the user. Core data, not UI copy: which of these a user
    /// sees decides whether they retype a PIN, wait, or accept losing their
    /// conversations, and that decision must not be re-derived per front end.
    pub fn user_message(self) -> &'static str {
        match self {
            Self::NoPinProvided => {
                "This device has no encryption keys. Enter your PIN to restore them from your backup."
            }
            Self::WrongPin => {
                "That PIN did not unlock your key backup. Check the PIN and try again."
            }
            Self::NoBackup => {
                "No key backup was found for this account. Your previous conversations cannot be recovered. \
                 You can start over with new encryption keys — your contacts will be asked to verify you again."
            }
            Self::BackupDestroyed => {
                "Your key backup was destroyed after too many incorrect PIN attempts. Your previous conversations \
                 cannot be recovered. You can start over with new encryption keys — your contacts will be asked \
                 to verify you again."
            }
            Self::RateLimited => {
                "Too many recovery attempts. Wait before trying your PIN again — your backup is still intact."
            }
            Self::Unavailable => {
                "Could not reach the key backup service. Your keys are still safe; try again when you are online."
            }
        }
    }
}

/// Classify a failed SVR restore.
///
/// The mapping matters more than it looks: it is what separates "mint a new
/// identity" from "do not touch anything". A misclassification in the lenient
/// direction silently destroys a recoverable account.
pub fn classify_restore_failure(err: &SvrBackupError) -> KeyRecoveryNeed {
    match err {
        // The server answers NOT_FOUND only when the account has no backup at
        // all; a backup under a different PIN-derived id is PERMISSION_DENIED
        // (`WrongPin`). OPEN-ITEMS §114.
        SvrBackupError::BackupNotFound => KeyRecoveryNeed::NoBackup,
        // The server says the PIN-derived id does not match, or the backup was
        // found and did not decrypt: either way the PIN is wrong.
        SvrBackupError::WrongPin | SvrBackupError::Crypto(_) => KeyRecoveryNeed::WrongPin,
        SvrBackupError::BackupDestroyed => KeyRecoveryNeed::BackupDestroyed,
        SvrBackupError::RateLimited => KeyRecoveryNeed::RateLimited,
        SvrBackupError::Network(_) | SvrBackupError::Storage(_) | SvrBackupError::Generic(_) => {
            KeyRecoveryNeed::Unavailable
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{CryptoError, NetworkError, StorageError};

    #[test]
    fn a_wrong_pin_never_authorises_a_new_identity() {
        // The single most dangerous misclassification: a typo must not be
        // allowed to look like "this account has no backup", because the
        // response to that is to mint a key and abandon the real one.
        let need = classify_restore_failure(&SvrBackupError::Crypto(CryptoError::Protocol(
            "decrypt failed".into(),
        )));
        assert_eq!(need, KeyRecoveryNeed::WrongPin);
        assert!(
            need.is_retryable(),
            "a wrong PIN leaves the original key recoverable — minting over it destroys the account"
        );
    }

    #[test]
    fn a_server_wrong_pin_is_retryable_and_a_missing_backup_is_final() {
        // The server tells the two apart (NOT_FOUND vs PERMISSION_DENIED); a
        // typo must stay retryable, an account with no backup must not.
        let need = classify_restore_failure(&SvrBackupError::WrongPin);
        assert_eq!(need, KeyRecoveryNeed::WrongPin);
        assert!(need.is_retryable());
        let need = classify_restore_failure(&SvrBackupError::BackupNotFound);
        assert_eq!(need, KeyRecoveryNeed::NoBackup);
        assert!(!need.is_retryable());
    }

    #[test]
    fn an_unreachable_server_never_authorises_a_new_identity() {
        for err in [
            SvrBackupError::Network(NetworkError::Unavailable),
            SvrBackupError::Storage(StorageError::NotFound("db closed".into())),
            SvrBackupError::Generic("no profile key".into()),
        ] {
            let need = classify_restore_failure(&err);
            assert_eq!(need, KeyRecoveryNeed::Unavailable, "for {err:?}");
            assert!(
                need.is_retryable(),
                "a transport failure says nothing about whether a backup exists"
            );
        }
    }

    #[test]
    fn rate_limiting_is_retryable() {
        let need = classify_restore_failure(&SvrBackupError::RateLimited);
        assert_eq!(need, KeyRecoveryNeed::RateLimited);
        assert!(
            need.is_retryable(),
            "the backup is intact; only waiting is needed"
        );
    }

    #[test]
    fn only_a_destroyed_backup_makes_a_new_identity_correct() {
        let need = classify_restore_failure(&SvrBackupError::BackupDestroyed);
        assert_eq!(need, KeyRecoveryNeed::BackupDestroyed);
        assert!(
            !need.is_retryable(),
            "nothing is left to recover, so a new identity is the only way forward"
        );
    }

    #[test]
    fn no_pin_is_retryable_and_not_a_reason_to_mint() {
        assert!(KeyRecoveryNeed::NoPinProvided.is_retryable());
    }

    #[test]
    fn every_need_says_something_actionable() {
        for need in [
            KeyRecoveryNeed::NoPinProvided,
            KeyRecoveryNeed::WrongPin,
            KeyRecoveryNeed::NoBackup,
            KeyRecoveryNeed::BackupDestroyed,
            KeyRecoveryNeed::RateLimited,
            KeyRecoveryNeed::Unavailable,
        ] {
            // The whole defect was silence. An empty message would reproduce it.
            assert!(
                !need.user_message().is_empty(),
                "{need:?} has nothing to say"
            );
        }
    }
}

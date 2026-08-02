//! Current user provider trait for cross-cutting user identity access.
//!
//! This module centralizes the "get current logged-in user" pattern that was
//! duplicated across 13+ services. Instead of each service implementing its own
//! `current_user_id()` method, they can use this trait implementation on SettingsRepository.

use crate::domain::ids::UserId;
use crate::error::StorageError;
use crate::storage::repositories::settings_repo::keys;
use crate::storage::SettingsRepository;

/// Error type for current user operations.
#[derive(Debug, thiserror::Error)]
pub enum CurrentUserError {
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),
    #[error("not logged in")]
    NotLoggedIn,
}

/// Trait for providing the current logged-in user's ID.
///
/// This trait abstracts the common pattern of retrieving the current user ID
/// from settings, allowing services to depend on this capability without
/// duplicating the implementation.
pub trait CurrentUserProvider {
    /// Get the current logged-in user's ID.
    ///
    /// Returns `Err(CurrentUserError::NotLoggedIn)` if no user is logged in.
    fn current_user_id(&self) -> Result<UserId, CurrentUserError>;
}

/// Carry the *cause* into `ServiceError` rather than flattening it to a string.
///
/// The inbox ack policy classifies a `ServiceError` into "retry this" or
/// "delete this and tell the user", and `Validation` is on the delete side
/// because validation means the bytes are bad. Neither variant here is about
/// the bytes: a failed settings read is our disk, and not-being-logged-in-yet
/// is local state a later poll resolves. Formatting either into
/// `Validation(format!("…: {e}"))` — which is what the `SyncSent` path did —
/// made a locked database look like a malformed message, so a linked device's
/// sync copy was deleted from the server and replaced with a delivery-issue
/// marker it did not deserve.
impl From<CurrentUserError> for crate::error::ServiceError {
    fn from(e: CurrentUserError) -> Self {
        match e {
            CurrentUserError::Storage(s) => Self::Storage(s),
            CurrentUserError::NotLoggedIn => Self::NotLoggedIn,
        }
    }
}

impl CurrentUserProvider for SettingsRepository {
    fn current_user_id(&self) -> Result<UserId, CurrentUserError> {
        let user_id_str = self
            .get_string(keys::CURRENT_USER_ID)
            .map_err(CurrentUserError::Storage)?
            .ok_or(CurrentUserError::NotLoggedIn)?;
        user_id_str.parse().map_err(|_| CurrentUserError::NotLoggedIn)
    }
}

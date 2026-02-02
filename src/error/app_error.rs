//! Application error types.
//!
//! Top-level application errors.

/// Application error.
pub enum AppError {
    /// Network unavailable.
    Offline,
    /// Connection timeout.
    Timeout,
    /// Server error.
    Server,
    /// Session expired.
    SessionExpired,
    /// Invalid credentials.
    InvalidCredentials,
    /// Not found.
    NotFound,
    /// Permission denied.
    PermissionDenied,
}

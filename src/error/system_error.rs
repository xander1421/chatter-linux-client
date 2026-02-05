//! System integration errors.
//!
//! Error types for OS integration operations.

/// Errors from system integration operations.
#[derive(Debug, thiserror::Error)]
pub enum SystemError {
    /// Failed to determine system directories.
    #[error("Failed to determine system directories")]
    DirectoryInit,

    /// Failed to create a directory.
    #[error("Failed to create directory: {0}")]
    DirectoryCreate(#[source] std::io::Error),

    /// Keyring operation failed.
    #[error("Keyring error: {0}")]
    Keyring(String),

    /// Failed to send notification.
    #[error("Failed to send notification: {0}")]
    NotificationSend(String),

    /// Failed to initialize notification system.
    #[error("Failed to initialize notification system: {0}")]
    NotificationInit(String),

    /// Failed to initialize system tray.
    #[error("Failed to initialize system tray: {0}")]
    TrayInit(String),

    /// D-Bus communication error.
    #[error("D-Bus error: {0}")]
    Dbus(String),

    /// Failed to initialize clipboard.
    #[error("Failed to initialize clipboard: {0}")]
    ClipboardInit(String),

    /// Failed to read from clipboard.
    #[error("Failed to read from clipboard: {0}")]
    ClipboardRead(String),

    /// Failed to write to clipboard.
    #[error("Failed to write to clipboard: {0}")]
    ClipboardWrite(String),
}

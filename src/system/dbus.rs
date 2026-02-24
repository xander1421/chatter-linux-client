//! D-Bus integration.
//!
//! D-Bus communication for system services.
//! Foundation for future integrations: media player, screen lock detection, etc.

use crate::error::SystemError;
use zbus::Connection;

/// D-Bus client for system service integration.
///
/// Provides a connection to the session D-Bus for communicating with
/// system services like media players, screen lock, etc.
pub struct DbusClient {
    connection: Connection,
}

impl DbusClient {
    /// Create a new D-Bus client connected to the session bus.
    ///
    /// # Errors
    ///
    /// Returns `SystemError::Dbus` if connection to the session bus fails.
    pub async fn new() -> Result<Self, SystemError> {
        let connection = Connection::session()
            .await
            .map_err(|e| SystemError::Dbus(e.to_string()))?;

        Ok(Self { connection })
    }

    /// Get a reference to the underlying D-Bus connection.
    ///
    /// Use this to make custom D-Bus calls or create proxies.
    pub fn connection(&self) -> &Connection {
        &self.connection
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Note: Actual D-Bus tests require a running session bus,
    // which may not be available in CI environments.
    // These tests are marked #[ignore] for that reason.

    #[tokio::test]
    #[ignore = "requires D-Bus session bus"]
    async fn test_dbus_client_new() {
        let client = DbusClient::new().await;
        assert!(client.is_ok());
    }

    #[tokio::test]
    #[ignore = "requires D-Bus session bus"]
    async fn test_dbus_client_connection() {
        let client = DbusClient::new().await.unwrap();
        // Connection should be accessible
        let _conn = client.connection();
    }
}

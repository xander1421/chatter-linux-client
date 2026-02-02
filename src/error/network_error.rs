//! Network-specific errors.
//!
//! Errors related to network operations.

/// Network error.
pub enum NetworkError {
    /// Connection failed.
    ConnectionFailed,
    /// Request timeout.
    Timeout,
    /// Invalid response.
    InvalidResponse,
}

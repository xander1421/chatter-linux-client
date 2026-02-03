//! Network-specific errors.
//!
//! Errors related to network operations with gRPC status mapping.

use thiserror::Error;
use tonic::{Code, Status};

/// Network error with gRPC status code classification.
#[derive(Debug, Error)]
pub enum NetworkError {
    /// Connection to server failed.
    #[error("connection failed: {0}")]
    ConnectionFailed(String),

    /// Request timed out.
    #[error("request timeout")]
    Timeout,

    /// Server is unavailable (temporary).
    #[error("server unavailable")]
    Unavailable,

    /// Authentication required or invalid.
    #[error("authentication required")]
    Unauthenticated,

    /// Permission denied for operation.
    #[error("permission denied")]
    PermissionDenied,

    /// Requested resource not found.
    #[error("resource not found: {0}")]
    NotFound(String),

    /// Invalid request data.
    #[error("invalid request: {0}")]
    InvalidRequest(String),

    /// Rate limited by server.
    #[error("rate limited")]
    RateLimited,

    /// Server-side error.
    #[error("server error: {0}")]
    ServerError(String),

    /// Unknown or unclassified error.
    #[error("unknown error: {0}")]
    Unknown(String),
}

impl NetworkError {
    /// Convert a tonic Status to NetworkError.
    ///
    /// Maps gRPC status codes to appropriate error variants.
    pub fn from_status(status: &Status) -> Self {
        let message = status.message().to_string();

        match status.code() {
            Code::Ok => NetworkError::Unknown("unexpected OK status as error".to_string()),
            Code::Cancelled => NetworkError::Unknown(format!("cancelled: {}", message)),
            Code::Unknown => NetworkError::Unknown(message),
            Code::InvalidArgument => NetworkError::InvalidRequest(message),
            Code::DeadlineExceeded => NetworkError::Timeout,
            Code::NotFound => NetworkError::NotFound(message),
            Code::AlreadyExists => NetworkError::InvalidRequest(format!("already exists: {}", message)),
            Code::PermissionDenied => NetworkError::PermissionDenied,
            Code::ResourceExhausted => NetworkError::RateLimited,
            Code::FailedPrecondition => NetworkError::InvalidRequest(format!("precondition failed: {}", message)),
            Code::Aborted => NetworkError::ServerError(format!("aborted: {}", message)),
            Code::OutOfRange => NetworkError::InvalidRequest(format!("out of range: {}", message)),
            Code::Unimplemented => NetworkError::ServerError(format!("unimplemented: {}", message)),
            Code::Internal => NetworkError::ServerError(message),
            Code::Unavailable => NetworkError::Unavailable,
            Code::DataLoss => NetworkError::ServerError(format!("data loss: {}", message)),
            Code::Unauthenticated => NetworkError::Unauthenticated,
        }
    }

    /// Returns true if this error is potentially retryable.
    ///
    /// Retryable errors include:
    /// - `Unavailable` - Server temporarily down
    /// - `RateLimited` - Can retry after backoff
    /// - `ServerError` - Transient server issues
    /// - `Timeout` - Network or server slow
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            NetworkError::Unavailable
                | NetworkError::RateLimited
                | NetworkError::ServerError(_)
                | NetworkError::Timeout
        )
    }

    /// Returns true if this is an authentication-related error.
    ///
    /// Auth errors include:
    /// - `Unauthenticated` - No valid credentials
    /// - `PermissionDenied` - Valid credentials but insufficient permissions
    pub fn is_auth_error(&self) -> bool {
        matches!(
            self,
            NetworkError::Unauthenticated | NetworkError::PermissionDenied
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_from_status_unavailable() {
        let status = Status::unavailable("server down");
        let err = NetworkError::from_status(&status);
        assert!(matches!(err, NetworkError::Unavailable));
        assert!(err.is_retryable());
    }

    #[test]
    fn test_from_status_unauthenticated() {
        let status = Status::unauthenticated("invalid token");
        let err = NetworkError::from_status(&status);
        assert!(matches!(err, NetworkError::Unauthenticated));
        assert!(err.is_auth_error());
        assert!(!err.is_retryable());
    }

    #[test]
    fn test_from_status_permission_denied() {
        let status = Status::permission_denied("access denied");
        let err = NetworkError::from_status(&status);
        assert!(matches!(err, NetworkError::PermissionDenied));
        assert!(err.is_auth_error());
    }

    #[test]
    fn test_from_status_not_found() {
        let status = Status::not_found("user not found");
        let err = NetworkError::from_status(&status);
        assert!(matches!(err, NetworkError::NotFound(_)));
        assert!(!err.is_retryable());
    }

    #[test]
    fn test_from_status_deadline_exceeded() {
        let status = Status::deadline_exceeded("request timed out");
        let err = NetworkError::from_status(&status);
        assert!(matches!(err, NetworkError::Timeout));
        assert!(err.is_retryable());
    }

    #[test]
    fn test_from_status_resource_exhausted() {
        let status = Status::resource_exhausted("rate limit exceeded");
        let err = NetworkError::from_status(&status);
        assert!(matches!(err, NetworkError::RateLimited));
        assert!(err.is_retryable());
    }

    #[test]
    fn test_from_status_internal() {
        let status = Status::internal("internal error");
        let err = NetworkError::from_status(&status);
        assert!(matches!(err, NetworkError::ServerError(_)));
        assert!(err.is_retryable());
    }

    #[test]
    fn test_from_status_invalid_argument() {
        let status = Status::invalid_argument("bad input");
        let err = NetworkError::from_status(&status);
        assert!(matches!(err, NetworkError::InvalidRequest(_)));
        assert!(!err.is_retryable());
    }

    #[test]
    fn test_error_display() {
        let err = NetworkError::ConnectionFailed("host unreachable".to_string());
        assert_eq!(err.to_string(), "connection failed: host unreachable");

        let err = NetworkError::Timeout;
        assert_eq!(err.to_string(), "request timeout");

        let err = NetworkError::NotFound("channel-123".to_string());
        assert_eq!(err.to_string(), "resource not found: channel-123");
    }
}

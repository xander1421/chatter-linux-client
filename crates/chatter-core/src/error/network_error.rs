//! Network-specific errors.
//!
//! Errors related to network operations with gRPC status mapping.

use std::error::Error as StdError;
use thiserror::Error;
use tonic::{Code, Status};

/// Network error with gRPC status code classification.
#[derive(Debug, Clone, Error)]
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

    /// Permanent data loss reported by server.
    #[error("data loss: {0}")]
    DataLoss(String),

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
        let code = status.code();

        // Log detailed error info for debugging
        tracing::debug!(
            code = ?code,
            message = %message,
            metadata = ?status.metadata(),
            "gRPC error status"
        );

        // Check for h2/http2 errors in the message (these come from transport layer)
        if message.contains("h2 protocol error") || message.contains("http2 error") {
            tracing::warn!(
                code = ?code,
                message = %message,
                source = ?StdError::source(status),
                "HTTP/2 protocol error detected - this may be a transport issue"
            );
        }

        match code {
            Code::Ok => NetworkError::Unknown("unexpected OK status as error".to_string()),
            Code::Cancelled => NetworkError::Unknown(format!("cancelled: {}", message)),
            Code::Unknown => {
                // "Service was not ready: transport error" comes from tonic's .ready()
                // check when the HTTP/2 connection is temporarily down. Treat as Unavailable
                // so retry logic can recover.
                if message.contains("transport error") || message.contains("Service was not ready") {
                    tracing::warn!(
                        message = %message,
                        "Transport-level error detected in Code::Unknown — treating as Unavailable"
                    );
                    NetworkError::Unavailable
                } else {
                    NetworkError::Unknown(message)
                }
            }
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
            Code::Unavailable => {
                // NetworkError::Unavailable is a unit variant, so everything the
                // transport had to say is dropped here. That is why OPEN-ITEMS
                // §73 could only report "Unavailable in 0.00s, server logs
                // nothing" after eliminating six hypotheses: the answer was in
                // this `message` and in `status`'s source chain, and neither ever
                // reached a log. Say it before discarding it.
                tracing::warn!(
                    message = %message,
                    source = ?StdError::source(status),
                    metadata = ?status.metadata(),
                    "gRPC UNAVAILABLE — the transport's own reason, which the \
                     NetworkError variant cannot carry"
                );
                NetworkError::Unavailable
            }
            Code::DataLoss => NetworkError::DataLoss(message),
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

    /// Returns true if this error indicates a transport/connection failure.
    ///
    /// Transport errors mean the HTTP/2 connection is stale or broken.
    /// Callers should reconnect the pool before retrying.
    pub fn is_transport_error(&self) -> bool {
        matches!(
            self,
            NetworkError::Unavailable | NetworkError::ConnectionFailed(_)
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

    /// The server refused a sealed send because the destination device is
    /// retired (FailedPrecondition "unknown destination device"). The sender
    /// should stop encrypting to that device, not fail the whole send.
    pub fn is_unknown_destination_device(&self) -> bool {
        matches!(self, NetworkError::InvalidRequest(m) if m.contains("unknown destination device"))
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
    fn test_from_status_unknown_transport_error_is_retryable() {
        // "Service was not ready: transport error" from tonic .ready() check
        let status = Status::new(Code::Unknown, "Service was not ready: transport error");
        let err = NetworkError::from_status(&status);
        assert!(matches!(err, NetworkError::Unavailable));
        assert!(err.is_retryable());
    }

    #[test]
    fn test_from_status_unknown_generic_not_retryable() {
        let status = Status::new(Code::Unknown, "something else went wrong");
        let err = NetworkError::from_status(&status);
        assert!(matches!(err, NetworkError::Unknown(_)));
        assert!(!err.is_retryable());
    }

    #[test]
    fn test_from_status_data_loss() {
        let status = Status::new(Code::DataLoss, "backup destroyed");
        let err = NetworkError::from_status(&status);
        assert!(matches!(err, NetworkError::DataLoss(_)));
        assert!(!err.is_retryable());
        assert!(!err.is_transport_error());
    }

    #[test]
    fn test_is_transport_error() {
        assert!(NetworkError::Unavailable.is_transport_error());
        assert!(NetworkError::ConnectionFailed("stale".into()).is_transport_error());
        // Retryable but not transport-specific:
        assert!(!NetworkError::RateLimited.is_transport_error());
        assert!(!NetworkError::ServerError("boom".into()).is_transport_error());
        assert!(!NetworkError::Timeout.is_transport_error());
        // Non-retryable:
        assert!(!NetworkError::NotFound("x".into()).is_transport_error());
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

    #[test]
    fn test_unknown_destination_device() {
        let err = NetworkError::from_status(&Status::failed_precondition("unknown destination device"));
        assert!(err.is_unknown_destination_device());
        let err = NetworkError::from_status(&Status::failed_precondition("something else"));
        assert!(!err.is_unknown_destination_device());
        let err = NetworkError::from_status(&Status::not_found("unknown destination device"));
        assert!(!err.is_unknown_destination_device());
    }
}

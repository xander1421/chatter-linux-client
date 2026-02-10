//! Custom assertions for test validation.
//!
//! Provides assertion helpers for common error checking patterns.

use chatter_linux::error::NetworkError;

/// Assert that an error indicates a resource was not found.
///
/// Servers may return different error types for non-existent resources:
/// - NotFound: Explicitly indicates resource doesn't exist
/// - PermissionDenied: Avoids leaking information about resource existence
/// - ServerError: Some endpoints return generic errors for non-existent resources
///
/// # Panics
///
/// Panics if the result is Ok or an unexpected error type.
pub fn assert_not_found<T>(result: Result<T, NetworkError>) {
    match result {
        Err(NetworkError::NotFound(_)) => {}
        Err(NetworkError::PermissionDenied) => {}
        Err(NetworkError::ServerError(_)) => {} // Some endpoints return this for non-existent resources
        Err(e) => panic!("Expected NotFound/PermissionDenied/ServerError, got: {:?}", e),
        Ok(_) => panic!("Expected error for non-existent resource, but got Ok"),
    }
}

/// Assert that an error is NotFound and contains the expected message.
pub fn assert_not_found_with_message<T>(result: Result<T, NetworkError>, expected: &str) {
    match result {
        Err(NetworkError::NotFound(msg)) => {
            assert!(
                msg.contains(expected),
                "NotFound message '{}' does not contain '{}'",
                msg,
                expected
            );
        }
        Err(e) => panic!("Expected NotFound, got: {:?}", e),
        Ok(_) => panic!("Expected NotFound error, but got Ok"),
    }
}

/// Assert that an error is InvalidRequest.
pub fn assert_invalid_request<T>(result: Result<T, NetworkError>) {
    match result {
        Err(NetworkError::InvalidRequest(_)) => {}
        Err(e) => panic!("Expected InvalidRequest, got: {:?}", e),
        Ok(_) => panic!("Expected InvalidRequest error, but got Ok"),
    }
}

/// Assert that an error is InvalidRequest and contains the expected message.
pub fn assert_invalid_request_with_message<T>(result: Result<T, NetworkError>, expected: &str) {
    match result {
        Err(NetworkError::InvalidRequest(msg)) => {
            assert!(
                msg.contains(expected),
                "InvalidRequest message '{}' does not contain '{}'",
                msg,
                expected
            );
        }
        Err(e) => panic!("Expected InvalidRequest, got: {:?}", e),
        Ok(_) => panic!("Expected InvalidRequest error, but got Ok"),
    }
}

/// Assert that an error is Unauthenticated.
pub fn assert_unauthenticated<T>(result: Result<T, NetworkError>) {
    match result {
        Err(NetworkError::Unauthenticated) => {}
        Err(e) => panic!("Expected Unauthenticated, got: {:?}", e),
        Ok(_) => panic!("Expected Unauthenticated error, but got Ok"),
    }
}

/// Assert that an error is PermissionDenied.
pub fn assert_permission_denied<T>(result: Result<T, NetworkError>) {
    match result {
        Err(NetworkError::PermissionDenied) => {}
        Err(e) => panic!("Expected PermissionDenied, got: {:?}", e),
        Ok(_) => panic!("Expected PermissionDenied error, but got Ok"),
    }
}

/// Assert that an error is an authentication error (Unauthenticated or PermissionDenied).
pub fn assert_auth_error<T>(result: Result<T, NetworkError>) {
    match result {
        Err(ref e) if e.is_auth_error() => {}
        Err(e) => panic!("Expected auth error, got: {:?}", e),
        Ok(_) => panic!("Expected auth error, but got Ok"),
    }
}

/// Assert that an error is retryable.
pub fn assert_retryable<T>(result: Result<T, NetworkError>) {
    match result {
        Err(ref e) if e.is_retryable() => {}
        Err(e) => panic!("Expected retryable error, got: {:?}", e),
        Ok(_) => panic!("Expected retryable error, but got Ok"),
    }
}

/// Assert that an error is ServerError.
pub fn assert_server_error<T>(result: Result<T, NetworkError>) {
    match result {
        Err(NetworkError::ServerError(_)) => {}
        Err(e) => panic!("Expected ServerError, got: {:?}", e),
        Ok(_) => panic!("Expected ServerError, but got Ok"),
    }
}

/// Assert that a result is Ok.
///
/// This is a convenience wrapper that provides better error messages.
pub fn assert_ok<T, E: std::fmt::Debug>(result: Result<T, E>) -> T {
    match result {
        Ok(v) => v,
        Err(e) => panic!("Expected Ok, got error: {:?}", e),
    }
}

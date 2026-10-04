//! Error type shared by the in-memory test doubles.

/// Result type for mock services.
pub type MockResult<T> = Result<T, MockError>;

/// Error type for mock services.
#[derive(Debug, Clone, PartialEq)]
pub enum MockError {
    NotFound(String),
    InvalidRequest(String),
    Unauthorized,
    PermissionDenied,
    NetworkError(String),
    CryptoError(String),
    Internal(String),
}

impl std::fmt::Display for MockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(msg) => write!(f, "Not found: {}", msg),
            Self::InvalidRequest(msg) => write!(f, "Invalid request: {}", msg),
            Self::Unauthorized => write!(f, "Unauthorized"),
            Self::PermissionDenied => write!(f, "Permission denied"),
            Self::NetworkError(msg) => write!(f, "Network error: {}", msg),
            Self::CryptoError(msg) => write!(f, "Crypto error: {}", msg),
            Self::Internal(msg) => write!(f, "Internal error: {}", msg),
        }
    }
}

impl std::error::Error for MockError {}

// ============================================================================
// Message Service Trait
// ============================================================================

// ============================================================================
// Channel Service Trait
// ============================================================================

// ============================================================================
// User Service Trait
// ============================================================================

// ============================================================================
// Auth Service Trait
// ============================================================================

// ============================================================================
// Attachment Service Trait
// ============================================================================

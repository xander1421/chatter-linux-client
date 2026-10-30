//! Secure string wrapper with automatic zeroization.
//!
//! Provides a String wrapper that automatically zeroizes its contents
//! when dropped, preventing sensitive data from lingering in memory.
//!
//! # Security Features
//!
//! - Automatic zeroization on drop
//! - Optional memory locking (mlock) to prevent swapping
//! - Debug output redacted
//! - Clone requires explicit method call
//!
//! # Example
//!
//! ```ignore
//! use crate::crypto::SecretString;
//!
//! let secret = SecretString::new("my-secret-passphrase".to_string());
//! // Use the secret
//! let value = secret.expose_secret();
//! // When `secret` goes out of scope, memory is zeroized
//! ```

use std::fmt;

use zeroize::Zeroize;

/// A String wrapper that automatically zeroizes its contents on drop.
///
/// Use this for sensitive data like passphrases, API keys, or tokens
/// that should not linger in memory after use.
pub struct SecretString {
    inner: String,
    /// Whether memory is locked (mlock'd)
    locked: bool,
}

impl SecretString {
    /// Create a new SecretString from a String.
    ///
    /// The string will be zeroized when this wrapper is dropped.
    pub fn new(secret: String) -> Self {
        Self {
            inner: secret,
            locked: false,
        }
    }

    /// Create a new SecretString and attempt to lock its memory.
    ///
    /// Memory locking (mlock/VirtualLock) prevents the memory from being swapped to disk.
    /// Falls back to unlocked if locking fails (requires appropriate privileges).
    #[cfg(any(unix, windows))]
    pub fn new_locked(secret: String) -> Self {
        let mut s = Self {
            inner: secret,
            locked: false,
        };
        s.try_lock();
        s
    }

    /// Create a new SecretString (fallback for unsupported platforms).
    #[cfg(not(any(unix, windows)))]
    pub fn new_locked(secret: String) -> Self {
        Self::new(secret)
    }

    /// Attempt to lock the memory pages containing this secret (Unix).
    #[cfg(unix)]
    fn try_lock(&mut self) {
        use libc::{mlock, size_t};

        let ptr = self.inner.as_ptr();
        let len = self.inner.len();

        // mlock the memory region
        let result = unsafe { mlock(ptr as *const libc::c_void, len as size_t) };

        if result == 0 {
            self.locked = true;
            tracing::trace!("Memory locked for secret ({} bytes)", len);
        } else {
            // mlock failed - usually due to RLIMIT_MEMLOCK
            // This is not critical, just means the memory could be swapped
            tracing::trace!(
                "Could not lock memory for secret (this is normal for unprivileged processes)"
            );
        }
    }

    /// Attempt to lock the memory pages containing this secret (Windows).
    #[cfg(windows)]
    fn try_lock(&mut self) {
        use windows::Win32::System::Memory::VirtualLock;

        let ptr = self.inner.as_ptr();
        let len = self.inner.len();

        // VirtualLock the memory region
        let result = unsafe { VirtualLock(ptr as *mut _, len) };

        if result.is_ok() {
            self.locked = true;
            tracing::trace!("Memory locked for secret ({} bytes)", len);
        } else {
            tracing::trace!("Could not lock memory for secret (normal for unprivileged processes)");
        }
    }

    /// Expose the secret value for use.
    ///
    /// Returns a reference to the inner string. Be careful not to
    /// copy this value unnecessarily.
    #[inline]
    pub fn expose_secret(&self) -> &str {
        &self.inner
    }

    /// Explicitly clone the secret.
    ///
    /// This is intentionally not implementing Clone trait to make
    /// copies explicit and deliberate.
    pub fn clone_secret(&self) -> Self {
        Self::new(self.inner.clone())
    }

    /// Returns the length of the secret.
    #[inline]
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Returns true if the secret is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Explicitly zeroize and clear the secret.
    pub fn zeroize(&mut self) {
        self.inner.zeroize();
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecretString")
            .field("inner", &"[REDACTED]")
            .field("locked", &self.locked)
            .finish()
    }
}

impl fmt::Display for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[REDACTED]")
    }
}

// Intentionally NOT implementing Clone, PartialEq, or other traits
// that could accidentally leak the secret

impl Drop for SecretString {
    fn drop(&mut self) {
        // Unlock memory first (if locked) - Unix
        #[cfg(unix)]
        if self.locked {
            use libc::{munlock, size_t};

            let ptr = self.inner.as_ptr();
            let len = self.inner.len();

            unsafe {
                munlock(ptr as *const libc::c_void, len as size_t);
            }
        }

        // Unlock memory first (if locked) - Windows
        #[cfg(windows)]
        if self.locked {
            use windows::Win32::System::Memory::VirtualUnlock;

            let ptr = self.inner.as_ptr();
            let len = self.inner.len();

            unsafe {
                let _ = VirtualUnlock(ptr as *mut _, len);
            }
        }

        // Zeroize the secret
        self.inner.zeroize();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_secret_string_basic() {
        let secret = SecretString::new("test-secret".to_string());
        assert_eq!(secret.expose_secret(), "test-secret");
        assert_eq!(secret.len(), 11);
        assert!(!secret.is_empty());
    }

    #[test]
    fn test_secret_string_debug_redacted() {
        let secret = SecretString::new("super-secret".to_string());
        let debug = format!("{:?}", secret);
        assert!(debug.contains("REDACTED"));
        assert!(!debug.contains("super-secret"));
    }

    #[test]
    fn test_secret_string_display_redacted() {
        let secret = SecretString::new("super-secret".to_string());
        let display = format!("{}", secret);
        assert_eq!(display, "[REDACTED]");
    }

    #[test]
    fn test_secret_string_clone() {
        let secret = SecretString::new("test".to_string());
        let cloned = secret.clone_secret();
        assert_eq!(cloned.expose_secret(), "test");
    }

    #[test]
    fn test_secret_string_zeroize() {
        let mut secret = SecretString::new("test-secret".to_string());
        secret.zeroize();
        assert!(secret.is_empty() || secret.expose_secret().chars().all(|c| c == '\0'));
    }
}

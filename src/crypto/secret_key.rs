//! Secure key wrapper with automatic zeroization and optional memory locking.
//!
//! Provides a fixed-size key wrapper that automatically zeroizes its contents
//! when dropped, preventing sensitive key material from lingering in memory.
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
//! use crate::crypto::SecretKey32;
//!
//! let key = SecretKey32::new([0u8; 32]);
//! // Use the key
//! let value = key.expose();
//! // When `key` goes out of scope, memory is zeroized
//! ```

use std::fmt;

use zeroize::Zeroize;

/// A 32-byte key wrapper that automatically zeroizes its contents on drop.
///
/// Use this for sensitive cryptographic keys like shared secrets, private keys,
/// and symmetric encryption keys that should not linger in memory after use.
pub struct SecretKey32 {
    inner: [u8; 32],
    /// Whether memory is locked (mlock'd)
    locked: bool,
}

impl SecretKey32 {
    /// Create a new SecretKey32 from a 32-byte array.
    ///
    /// The key will be zeroized when this wrapper is dropped.
    pub fn new(key: [u8; 32]) -> Self {
        Self {
            inner: key,
            locked: false,
        }
    }

    /// Create a new SecretKey32 and attempt to lock its memory.
    ///
    /// Memory locking (mlock/VirtualLock) prevents the memory from being swapped to disk.
    /// Falls back to unlocked if locking fails (requires appropriate privileges).
    #[cfg(any(unix, windows))]
    pub fn new_locked(key: [u8; 32]) -> Self {
        let mut s = Self {
            inner: key,
            locked: false,
        };
        s.try_lock();
        s
    }

    /// Create a new SecretKey32 (fallback for unsupported platforms).
    #[cfg(not(any(unix, windows)))]
    pub fn new_locked(key: [u8; 32]) -> Self {
        Self::new(key)
    }

    /// Attempt to lock the memory pages containing this key (Unix).
    #[cfg(unix)]
    fn try_lock(&mut self) {
        use libc::{mlock, size_t};

        let ptr = self.inner.as_ptr();
        let len = self.inner.len();

        // mlock the memory region
        let result = unsafe { mlock(ptr as *const libc::c_void, len as size_t) };

        if result == 0 {
            self.locked = true;
            tracing::trace!("Memory locked for secret key ({} bytes)", len);
        } else {
            // mlock failed - usually due to RLIMIT_MEMLOCK
            // This is not critical, just means the memory could be swapped
            tracing::trace!("Could not lock memory for key (normal for unprivileged processes)");
        }
    }

    /// Attempt to lock the memory pages containing this key (Windows).
    #[cfg(windows)]
    fn try_lock(&mut self) {
        use windows::Win32::System::Memory::VirtualLock;

        let ptr = self.inner.as_ptr();
        let len = self.inner.len();

        // VirtualLock the memory region
        let result = unsafe { VirtualLock(ptr as *mut _, len) };

        if result.is_ok() {
            self.locked = true;
            tracing::trace!("Memory locked for secret key ({} bytes)", len);
        } else {
            // VirtualLock failed - usually due to working set quota
            tracing::trace!("Could not lock memory for key (normal for unprivileged processes)");
        }
    }

    /// Expose the secret key for use.
    ///
    /// Returns a reference to the inner key. Be careful not to
    /// copy this value unnecessarily.
    #[inline]
    pub fn expose(&self) -> &[u8; 32] {
        &self.inner
    }

    /// Expose as a slice.
    #[inline]
    pub fn as_slice(&self) -> &[u8] {
        &self.inner
    }

    /// Explicitly clone the key.
    ///
    /// This is intentionally not implementing Clone trait to make
    /// copies explicit and deliberate.
    pub fn clone_key(&self) -> Self {
        Self::new(self.inner)
    }

    /// Explicitly zeroize and clear the key.
    pub fn zeroize(&mut self) {
        self.inner.zeroize();
    }

    /// Check if the key is all zeros (indicates uninitialized or zeroized).
    pub fn is_zeroed(&self) -> bool {
        self.inner.iter().all(|&b| b == 0)
    }
}

impl fmt::Debug for SecretKey32 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecretKey32")
            .field("inner", &"[REDACTED]")
            .field("locked", &self.locked)
            .finish()
    }
}

impl fmt::Display for SecretKey32 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[REDACTED 32-byte key]")
    }
}

// Intentionally NOT implementing Clone, PartialEq, or other traits
// that could accidentally leak the key

impl Drop for SecretKey32 {
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

        // Zeroize the key
        self.inner.zeroize();
    }
}

/// A 64-byte key wrapper (for combined AES+HMAC keys).
pub struct SecretKey64 {
    inner: [u8; 64],
    locked: bool,
}

impl SecretKey64 {
    /// Create a new SecretKey64 from a 64-byte array.
    pub fn new(key: [u8; 64]) -> Self {
        Self {
            inner: key,
            locked: false,
        }
    }

    /// Create a new SecretKey64 and attempt to lock its memory.
    #[cfg(any(unix, windows))]
    pub fn new_locked(key: [u8; 64]) -> Self {
        let mut s = Self {
            inner: key,
            locked: false,
        };
        s.try_lock();
        s
    }

    #[cfg(not(any(unix, windows)))]
    pub fn new_locked(key: [u8; 64]) -> Self {
        Self::new(key)
    }

    #[cfg(unix)]
    fn try_lock(&mut self) {
        use libc::{mlock, size_t};

        let ptr = self.inner.as_ptr();
        let len = self.inner.len();

        let result = unsafe { mlock(ptr as *const libc::c_void, len as size_t) };

        if result == 0 {
            self.locked = true;
            tracing::trace!("Memory locked for secret key ({} bytes)", len);
        }
    }

    #[cfg(windows)]
    fn try_lock(&mut self) {
        use windows::Win32::System::Memory::VirtualLock;

        let ptr = self.inner.as_ptr();
        let len = self.inner.len();

        let result = unsafe { VirtualLock(ptr as *mut _, len) };

        if result.is_ok() {
            self.locked = true;
            tracing::trace!("Memory locked for secret key ({} bytes)", len);
        }
    }

    /// Expose the secret key for use.
    #[inline]
    pub fn expose(&self) -> &[u8; 64] {
        &self.inner
    }

    /// Expose as a slice.
    #[inline]
    pub fn as_slice(&self) -> &[u8] {
        &self.inner
    }

    /// Get the AES key portion (first 32 bytes).
    #[inline]
    pub fn aes_key(&self) -> &[u8; 32] {
        self.inner[..32].try_into().expect("slice is 32 bytes")
    }

    /// Get the HMAC key portion (last 32 bytes).
    #[inline]
    pub fn hmac_key(&self) -> &[u8; 32] {
        self.inner[32..].try_into().expect("slice is 32 bytes")
    }

    pub fn zeroize(&mut self) {
        self.inner.zeroize();
    }
}

impl fmt::Debug for SecretKey64 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecretKey64")
            .field("inner", &"[REDACTED]")
            .field("locked", &self.locked)
            .finish()
    }
}

impl Drop for SecretKey64 {
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

        // Zeroize the key
        self.inner.zeroize();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_secret_key32_basic() {
        let key = SecretKey32::new([42u8; 32]);
        assert_eq!(key.expose()[0], 42);
        assert!(!key.is_zeroed());
    }

    #[test]
    fn test_secret_key32_debug_redacted() {
        let key = SecretKey32::new([42u8; 32]);
        let debug = format!("{:?}", key);
        assert!(debug.contains("REDACTED"));
        assert!(!debug.contains("42"));
    }

    #[test]
    fn test_secret_key32_zeroize() {
        let mut key = SecretKey32::new([42u8; 32]);
        key.zeroize();
        assert!(key.is_zeroed());
    }

    #[test]
    fn test_secret_key64_basic() {
        let key = SecretKey64::new([42u8; 64]);
        assert_eq!(key.expose()[0], 42);
        assert_eq!(key.aes_key()[0], 42);
        assert_eq!(key.hmac_key()[0], 42);
    }
}

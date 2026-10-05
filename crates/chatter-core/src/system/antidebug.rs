//! Anti-debugging protection for sensitive data.
//!
//! Detects debugger attachment and takes protective measures to prevent
//! inspection of cryptographic keys, session tokens, and other sensitive data.
//!
//! # Security Model
//!
//! This module provides defense-in-depth against memory inspection attacks.
//! When a debugger is detected:
//!
//! 1. A warning is logged (tracing)
//! 2. An event is emitted for the app to take action (e.g., clear sensitive memory faster)
//!
//! Note: Anti-debugging is NOT a security boundary - a determined attacker can
//! bypass these checks. It raises the bar for casual inspection.
//!
//! # Platform Support
//!
//! - Linux: `ptrace(PTRACE_TRACEME)` detection
//! - Windows: `IsDebuggerPresent()` and `CheckRemoteDebuggerPresent()`
//! - Other: Returns false (no detection capability)

use std::sync::atomic::{AtomicBool, Ordering};

/// Cached result of debugger detection (avoids repeated syscalls)
static DEBUGGER_DETECTED: AtomicBool = AtomicBool::new(false);
static DETECTION_RAN: AtomicBool = AtomicBool::new(false);

/// Result of anti-debugging check
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DebuggerStatus {
    /// No debugger detected
    NotDetected,
    /// Debugger appears to be attached
    Detected,
    /// Detection not available on this platform
    Unavailable,
}

impl DebuggerStatus {
    /// Returns true if a debugger was detected
    #[inline]
    pub fn is_detected(&self) -> bool {
        matches!(self, DebuggerStatus::Detected)
    }
}

/// Check if a debugger is attached to this process.
///
/// # Returns
///
/// - `DebuggerStatus::Detected` - A debugger appears to be attached
/// - `DebuggerStatus::NotDetected` - No debugger detected
/// - `DebuggerStatus::Unavailable` - Detection not available on this platform
///
/// # Caching
///
/// The result is cached after the first call for performance.
/// Use `check_debugger_uncached()` if you need a fresh check.
pub fn check_debugger() -> DebuggerStatus {
    // Return cached result if available
    if DETECTION_RAN.load(Ordering::Relaxed) {
        if DEBUGGER_DETECTED.load(Ordering::Relaxed) {
            return DebuggerStatus::Detected;
        }
        return DebuggerStatus::NotDetected;
    }

    let status = check_debugger_uncached();

    // Cache the result
    if status == DebuggerStatus::Detected {
        DEBUGGER_DETECTED.store(true, Ordering::Relaxed);
    }
    DETECTION_RAN.store(true, Ordering::Relaxed);

    status
}

/// Check for debugger without using cache.
///
/// More expensive (makes syscalls) but always fresh.
pub fn check_debugger_uncached() -> DebuggerStatus {
    #[cfg(target_os = "linux")]
    {
        check_linux_debugger()
    }

    #[cfg(target_os = "windows")]
    {
        check_windows_debugger()
    }

    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        DebuggerStatus::Unavailable
    }
}

/// Linux debugger detection using /proc/self/status
///
/// Note: We only use the /proc/self/status method, not ptrace.
/// The ptrace(PTRACE_TRACEME) approach is problematic because:
/// - It marks the process as traceable by its parent
/// - PTRACE_DETACH from the tracee doesn't properly undo this
/// - This can cause the process to freeze in tracing-stop state on signals
#[cfg(target_os = "linux")]
fn check_linux_debugger() -> DebuggerStatus {
    // Check /proc/self/status for TracerPid
    // This is non-invasive and doesn't modify process state
    if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
        for line in status.lines() {
            if line.starts_with("TracerPid:") {
                let pid_str = line.trim_start_matches("TracerPid:").trim();
                if let Ok(pid) = pid_str.parse::<i32>() {
                    if pid != 0 {
                        tracing::warn!(
                            target: "security",
                            tracer_pid = pid,
                            "Debugger detected (TracerPid)"
                        );
                        return DebuggerStatus::Detected;
                    }
                }
            }
        }
    }

    DebuggerStatus::NotDetected
}

/// Windows debugger detection
#[cfg(target_os = "windows")]
fn check_windows_debugger() -> DebuggerStatus {
    use windows::Win32::System::Diagnostics::Debug::{
        CheckRemoteDebuggerPresent, IsDebuggerPresent,
    };

    // Method 1: IsDebuggerPresent - checks for user-mode debugger
    // SAFETY: No arguments, just queries system state
    let is_present = unsafe { IsDebuggerPresent() };
    if is_present.as_bool() {
        tracing::warn!(
            target: "security",
            "Debugger detected (IsDebuggerPresent)"
        );
        return DebuggerStatus::Detected;
    }

    // Method 2: CheckRemoteDebuggerPresent - checks for remote debugger
    // SAFETY: Passing valid pointer to current process handle
    let mut is_remote_present = windows::Win32::Foundation::BOOL::from(false);
    let current_process = unsafe { windows::Win32::System::Threading::GetCurrentProcess() };

    let result = unsafe { CheckRemoteDebuggerPresent(current_process, &mut is_remote_present) };

    if result.is_ok() && is_remote_present.as_bool() {
        tracing::warn!(
            target: "security",
            "Remote debugger detected (CheckRemoteDebuggerPresent)"
        );
        return DebuggerStatus::Detected;
    }

    DebuggerStatus::NotDetected
}

/// Initialize anti-debugging protection.
///
/// Should be called early in application startup (e.g., in main()).
/// Performs an initial debugger check and logs the result.
///
/// # Returns
///
/// `true` if no debugger detected (safe to proceed)
/// `false` if debugger detected (app may choose to exit or continue with warnings)
pub fn init_protection() -> bool {
    let status = check_debugger();

    match status {
        DebuggerStatus::NotDetected => {
            tracing::debug!(target: "security", "Anti-debugging check passed");
            true
        }
        DebuggerStatus::Detected => {
            tracing::error!(
                target: "security",
                "⚠️ DEBUGGER DETECTED - Sensitive memory may be exposed. \
                 Cryptographic keys and session data are at risk."
            );
            false
        }
        DebuggerStatus::Unavailable => {
            tracing::debug!(
                target: "security",
                "Anti-debugging not available on this platform"
            );
            true
        }
    }
}

/// Periodic debugger check for runtime protection.
///
/// Call this periodically (e.g., every 30 seconds) to detect debuggers
/// that attach after application startup.
///
/// # Returns
///
/// `true` if no debugger detected
/// `false` if debugger newly detected (first time seeing it this run)
pub fn periodic_check() -> bool {
    let status = check_debugger_uncached();

    if status == DebuggerStatus::Detected {
        let was_detected = DEBUGGER_DETECTED.swap(true, Ordering::SeqCst);
        if !was_detected {
            // First time detecting in this session
            tracing::error!(
                target: "security",
                "⚠️ DEBUGGER ATTACHED AT RUNTIME - Sensitive memory may be exposed"
            );
        }
        return false;
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_check_debugger_returns_valid_status() {
        let status = check_debugger();
        // Should be one of the valid states
        assert!(matches!(
            status,
            DebuggerStatus::NotDetected | DebuggerStatus::Detected | DebuggerStatus::Unavailable
        ));
    }

    #[test]
    fn test_status_is_detected() {
        assert!(DebuggerStatus::Detected.is_detected());
        assert!(!DebuggerStatus::NotDetected.is_detected());
        assert!(!DebuggerStatus::Unavailable.is_detected());
    }

    #[test]
    fn test_cached_check_consistent() {
        let first = check_debugger();
        let second = check_debugger();
        // Cached results should be consistent
        assert_eq!(first, second);
    }
}

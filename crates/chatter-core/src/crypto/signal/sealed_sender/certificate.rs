use std::time::{Duration, SystemTime};

use libsignal_protocol::{SenderCertificate, Timestamp};

use crate::config::TRUST_ROOT_PUBLIC_KEY;

// ============================================================================
// Certificate Validation Functions
// ============================================================================

/// Default certificate validity period (7 days).
pub const DEFAULT_CERTIFICATE_VALIDITY: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Refresh threshold (refresh when less than 1 day remaining).
pub const CERTIFICATE_REFRESH_THRESHOLD: Duration = Duration::from_secs(24 * 60 * 60);

/// Validate a sender certificate against the hardcoded trust root.
///
/// This function verifies that:
/// 1. The certificate was signed by the trusted server
/// 2. The certificate has not expired
///
/// # Arguments
///
/// * `cert` - The sender certificate to validate
///
/// # Returns
///
/// `true` if the certificate is valid, `false` otherwise.
///
/// # Security
///
/// The trust root is hardcoded in the binary (see `config.rs`) following
/// Signal's security model. This prevents MITM attacks during registration
/// or certificate renewal.
pub fn validate_trust_root(cert: &SenderCertificate) -> bool {
    // Get current timestamp
    let now_ms = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let now = Timestamp::from_epoch_millis(now_ms);

    // Check expiration
    let expiry = match cert.expiration() {
        Ok(ts) => ts,
        Err(e) => {
            tracing::warn!("Failed to get certificate expiration: {}", e);
            tracing::warn!(
                target: "chatter::crypto::sealed_sender_cert_reject",
                reason = "chain",
                "sealed sender certificate rejected"
            );
            return false;
        }
    };

    if expiry.epoch_millis() < now_ms {
        tracing::warn!(
            "Sender certificate expired: cert_expiry={}, now={}",
            expiry.epoch_millis(),
            now_ms
        );
        tracing::warn!(
            target: "chatter::crypto::sealed_sender_cert_reject",
            reason = "expired",
            "sealed sender certificate rejected"
        );
        return false;
    }

    // Validate certificate chain against trust root
    match cert.validate(&TRUST_ROOT_PUBLIC_KEY, now) {
        Ok(true) => true,
        Ok(false) => {
            tracing::warn!("Sender certificate failed trust root validation");
            tracing::warn!(
                target: "chatter::crypto::sealed_sender_cert_reject",
                reason = "trust_root",
                "sealed sender certificate rejected"
            );
            false
        }
        Err(e) => {
            tracing::warn!("Sender certificate validation error: {}", e);
            // Distinguish signature errors from generic chain errors when
            // libsignal's error string makes that clear; otherwise default
            // to "signature" since validate() failures are signature checks.
            let err_str = e.to_string();
            let reason = if err_str.to_ascii_lowercase().contains("signature") {
                "signature"
            } else {
                "chain"
            };
            tracing::warn!(
                target: "chatter::crypto::sealed_sender_cert_reject",
                reason = reason,
                "sealed sender certificate rejected"
            );
            false
        }
    }
}

/// Check if a sender certificate needs refresh.
///
/// Returns `true` if the certificate is expired or will expire within
/// the refresh threshold period.
///
/// # Arguments
///
/// * `cert` - The sender certificate to check
///
/// # Returns
///
/// `true` if the certificate should be refreshed.
pub fn certificate_needs_refresh(cert: &SenderCertificate) -> bool {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    let threshold_ms = CERTIFICATE_REFRESH_THRESHOLD.as_millis() as u64;
    let expiry_threshold = now.saturating_add(threshold_ms);

    match cert.expiration() {
        Ok(ts) => ts.epoch_millis() < expiry_threshold,
        Err(_) => true, // If we can't get expiration, assume refresh needed
    }
}

/// Cached sender certificate with expiration tracking.
///
/// This wrapper helps manage certificate lifecycle by tracking when
/// the certificate was obtained and when it expires.
#[derive(Clone)]
pub struct CachedSenderCertificate {
    /// The actual sender certificate.
    pub certificate: SenderCertificate,
    /// When this certificate was cached.
    pub cached_at: SystemTime,
}

impl CachedSenderCertificate {
    /// Create a new cached certificate.
    pub fn new(certificate: SenderCertificate) -> Self {
        Self {
            certificate,
            cached_at: SystemTime::now(),
        }
    }

    /// Check if this certificate needs refresh.
    pub fn needs_refresh(&self) -> bool {
        certificate_needs_refresh(&self.certificate)
    }

    /// Check if this certificate is still valid.
    pub fn is_valid(&self) -> bool {
        validate_trust_root(&self.certificate)
    }

    /// Get the expiration timestamp in milliseconds since epoch.
    pub fn expiration_ms(&self) -> Option<u64> {
        self.certificate
            .expiration()
            .ok()
            .map(|ts| ts.epoch_millis())
    }

    /// Get how long until expiration.
    pub fn time_until_expiration(&self) -> Option<Duration> {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        let expiry = self.certificate.expiration().ok()?.epoch_millis();
        if expiry > now {
            Some(Duration::from_millis(expiry - now))
        } else {
            None
        }
    }
}

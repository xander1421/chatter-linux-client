use super::*;

/// URL scheme prefix for QR payloads.
pub const LINK_URL_SCHEME: &str = "chatter";
pub(super) const LINK_URL_HOST: &str = "link";

/// Errors from device linking flows.
#[derive(Debug, thiserror::Error)]
pub enum LinkDeviceError {
    #[error("not logged in")]
    NotLoggedIn,
    #[error("re-authentication failed")]
    ReauthFailed,
    #[error("link URL malformed: {0}")]
    InvalidLinkUrl(String),
    #[error("provisioning session expired or cancelled: {0}")]
    SessionEnded(String),
    #[error("network: {0}")]
    Network(#[from] NetworkError),
    #[error("crypto: {0}")]
    Crypto(#[from] CryptoError),
    #[error("storage: {0}")]
    Storage(#[from] StorageError),
    #[error("invalid input: {0}")]
    Invalid(String),
}

impl LinkDeviceError {
    /// Text for the link dialog; the Display form is for logs.
    pub fn user_message(&self) -> String {
        match self {
            Self::NotLoggedIn => "You're signed out.",
            Self::ReauthFailed => "Wrong password or PIN.",
            Self::InvalidLinkUrl(_) => {
                "That isn't a device link. Copy the whole link from the new device."
            }
            Self::SessionEnded(_) => "This link expired. Generate a new QR on the new device.",
            Self::Network(_) => "Couldn't reach the server. Try again.",
            Self::Crypto(_) | Self::Storage(_) | Self::Invalid(_) => {
                "Couldn't link the device. Try again."
            }
        }
        .to_string()
    }
}

/// PIN gate outcomes contain categories only, never submitted or stored secrets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinVerification {
    Match,
    Mismatch,
    NotSetUpOnThisDevice,
    Error(PinVerificationError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinVerificationError {
    InvalidInput,
    Storage,
    InvalidFingerprint,
    Crypto,
    Network,
    RateLimited,
    BackupDestroyed,
}

impl PinVerification {
    pub fn message(self) -> &'static str {
        match self {
            Self::Match => "",
            Self::Mismatch => "Incorrect PIN. Online checks have limited attempts.",
            Self::NotSetUpOnThisDevice => "Shield isn't set up on this device yet",
            Self::Error(PinVerificationError::InvalidInput) => {
                "Enter a PIN with 8–64 printable ASCII characters"
            }
            Self::Error(PinVerificationError::RateLimited) => {
                "Too many PIN attempts. Wait before trying again"
            }
            Self::Error(PinVerificationError::BackupDestroyed) => {
                "Your key backup was deleted after too many PIN attempts. Set up your PIN again"
            }
            Self::Error(_) => "Couldn't check your PIN, try again",
        }
    }
}

/// Persistent state for a new-device provisioning session.
///
/// Owns the ephemeral keypair until the envelope arrives — losing this
/// (e.g. process restart) invalidates the in-flight session.
pub struct NewDeviceSession {
    /// Server-issued session UUID (also embedded in QR URL).
    pub uuid: String,
    /// `chatter://link?…` URL for QR encoding.
    pub qr_url: String,
    /// Server-side session expiry (Unix nanoseconds).
    pub expires_at: u64,
    /// Owned ephemeral keypair; secret zeroizes on drop.
    pub(crate) ephemeral: ProvisioningKeypair,
    pub device_info: String,
    pub device_name: String,
}

impl NewDeviceSession {
    pub fn public_key(&self) -> [u8; 32] {
        self.ephemeral.public_bytes()
    }
}

/// Pre-image of a parsed link URL (primary-side internal).
///
/// `pub(crate)` — outside callers receive a [`PendingDeviceLink`] from
/// [`LinkDeviceService::prepare_link_from_url`], which gates installation
/// on a separate `approve_link` step. Direct construction from outside the
/// crate would let UI / external code skip the user-confirmation gate.
#[derive(Debug, Clone)]
pub(crate) struct LinkRequest {
    pub uuid: String,
    /// Raw 32-byte X25519 ephemeral public key from new device.
    pub peer_pub: [u8; 32],
    /// Server URL hint embedded by the new device (so primary can warn on
    /// cross-server linking attempts).
    pub server_url: Option<String>,
    pub device_name: Option<String>,
}

/// A parsed, previewed device-link request that has **not yet** been
/// approved or installed. Returned by
/// [`LinkDeviceService::prepare_link_from_url`].
///
/// The UI **MUST** display [`Self::verification_code`] alongside the
/// device name and require explicit user confirmation that the code
/// matches the value shown on the new device before passing this to
/// [`LinkDeviceService::approve_link`].
///
/// Properties enforced at the type level:
/// - **Not [`Clone`]**: a single pending link cannot be silently fanned
///   out to multiple approval calls.
/// - **Move-consumed by `approve_link`**: the value is taken by value
///   (not by reference), so it cannot be reused after a successful or
///   failed approval.
/// - The inner `LinkRequest` is `pub(crate)` and there is no public
///   constructor — outside code can only obtain a `PendingDeviceLink`
///   from `prepare_link_from_url`.
///
/// The verification code is derived deterministically from the new
/// device's ephemeral X25519 public key (truncated SHA-256, first 6 hex
/// chars, uppercased), so the **same** value can be displayed on both
/// devices — matching it confirms both sides see the same QR payload and
/// no MITM substituted a different ephemeral key.
#[derive(Debug)]
pub struct PendingDeviceLink {
    pub(super) inner: LinkRequest,
    pub(super) verification_code: String,
}

impl PendingDeviceLink {
    /// Short verification code for UI display. Compare with the code shown
    /// on the new device — if they match, the QR payload was not tampered
    /// with in transit.
    pub fn verification_code(&self) -> &str {
        &self.verification_code
    }

    /// New device's user-supplied display name (if any).
    pub fn device_name(&self) -> Option<&str> {
        self.inner.device_name.as_deref()
    }

    /// Server URL the new device intends to talk to (if embedded in the
    /// QR). Lets UI flag cross-server linking attempts.
    pub fn server_url(&self) -> Option<&str> {
        self.inner.server_url.as_deref()
    }

    /// Provisioning session UUID. Useful for cancel / list flows.
    pub fn uuid(&self) -> &str {
        &self.inner.uuid
    }
}

/// Compute the 6-character verification code for a peer public key.
///
/// `SHA-256(peer_pub)` truncated to 3 bytes, hex-encoded uppercase.
/// Deterministic: both the primary and the new device produce the same
/// code from the same QR payload, so the user can compare them by eye.
pub(super) fn verification_code_for_pub(peer_pub: &[u8; 32]) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write;
    let digest = Sha256::digest(peer_pub);
    let mut out = String::with_capacity(6);
    for byte in digest.iter().take(3) {
        let _ = write!(out, "{byte:02X}");
    }
    out
}

/// Result of completing linking on the new device side.
#[derive(Debug, Clone)]
pub struct CompletedLink {
    pub user_id: crate::domain::ids::UserId,
    pub device_id: u32,
    pub display_name: Option<String>,
}

/// Streamed events from the new-device flow. Consumed by UI as a single
/// stream — the UI never has to interleave start/poll/complete itself.
#[derive(Debug, Clone)]
pub enum LinkProgress {
    /// QR rendered + ready to scan/paste. Includes the link URL and a PNG
    /// of the QR code (already-rendered to avoid the UI depending on the
    /// `qrcode` crate).
    QrReady {
        url: String,
        qr_png: Vec<u8>,
        expires_at: u64,
        /// The code the approving device shows for this QR
        /// ([`PendingDeviceLink::verification_code`]); display it so the user
        /// can compare the two.
        verification_code: String,
    },
    /// Polling started — primary hasn't approved yet.
    WaitingForApproval,
    /// Primary approved — keys received, finishing linking.
    Approved,
    /// Linking complete — caller should switch to main view + run post-auth
    /// bring-up.
    Linked(CompletedLink),
    /// Flow ended unsuccessfully. `String` is a user-facing message.
    Failed(String),
    /// Cancelled by user or session expiry.
    Cancelled,
}

/// A link this account approved whose new device has not finished linking.
/// Cancelling it ([`LinkDeviceService::cancel`]) stops the new device from
/// completing: the server only completes a session still in `Provisioned`.
#[derive(Debug, Clone)]
pub struct PendingLink {
    pub uuid: String,
    pub device_name: String,
    /// Unix nanoseconds.
    pub expires_at: u64,
}

/// The server lists a session for this account only once its primary has
/// approved it (`ProvisionDevice` is what ties it to a user), so an
/// unapproved request never reaches this list — approval needs the new
/// device's QR key, which the listing does not carry. Completed, cancelled
/// and expired sessions are dropped.
pub fn pending_links(sessions: Vec<ProvisioningSession>, now_ns: u64) -> Vec<PendingLink> {
    use crate::proto::provisioning_v1::provisioning_session::Status;
    sessions
        .into_iter()
        .filter(|s| s.status == Status::Provisioned as i32 && s.expires_at > now_ns)
        .map(|s| PendingLink {
            // The server holds no name for a pending link (the new device
            // only encrypts one once it has the identity key); label by info.
            device_name: s.device_info,
            uuid: s.provisioning_uuid,
            expires_at: s.expires_at,
        })
        .collect()
}

/// Summary of a just-approved (or previewed) link request, for UI display.
#[derive(Debug, Clone)]
pub struct ApprovedDevice {
    pub uuid: String,
    pub device_name: Option<String>,
    pub server_url: Option<String>,
}

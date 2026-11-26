use super::*;
use crate::services::signal::VerifiedSenderCert;

impl SignalService {
    /// Get sender certificate from cache or request a new one.
    ///
    /// The server picks the device id from the authenticated session, so
    /// this no longer takes a device_id argument.
    pub(crate) async fn get_or_request_sender_certificate(
        &self,
        auth_client: &mut AuthClient,
    ) -> Result<SenderCertificate, ServiceError> {
        if let Some(cert) = self.get_cached_sender_certificate()? {
            return Ok(cert);
        }

        let cert_response = auth_client.request_sender_certificate().await?;
        let proto_cert = cert_response
            .certificate
            .ok_or(ServiceError::Validation("Missing certificate".into()))?;
        let sender_cert = SenderCertificate::deserialize(&proto_cert.certificate)
            .map_err(|_| ServiceError::Validation("Invalid cert".into()))?;

        use base64::Engine;
        let cert_b64 = base64::engine::general_purpose::STANDARD.encode(&proto_cert.certificate);
        let _ = self.settings_repo.set(keys::SENDER_CERTIFICATE, &cert_b64);
        let _ = self.settings_repo.set(
            keys::SENDER_CERTIFICATE_REFRESH_AFTER,
            &cert_response.refresh_after,
        );
        let _ = self.settings_repo.set(
            keys::SENDER_CERTIFICATE_SERVER_KEY_ID,
            &proto_cert.server_key_id,
        );

        Ok(sender_cert)
    }
    /// Seal sender-key `contents` (a serialized GroupEncrypted) for every
    /// device of every one of `members`, our own sending device included
    /// (crypto/signal/sealed_sender/multi.rs says why), as one SSv2
    /// multi-recipient message. Devices are the ones we hold sessions with; a
    /// member the server says has others is reconciled by the caller.
    pub(crate) async fn seal_group_message(
        &self,
        auth_client: &mut AuthClient,
        channel_id: &crate::domain::ids::ChannelId,
        contents: Vec<u8>,
        members: &[String],
    ) -> Result<(Vec<u8>, Vec<ProtocolAddress>), ServiceError> {
        let sender_cert = self.get_or_request_sender_certificate(auth_client).await?;
        let ours = self.get_our_address()?;
        let mut destinations = Vec::new();
        for member in members {
            for device in self.session_devices(member).await? {
                let device = libsignal_protocol::DeviceId::try_from(device)
                    .map_err(|e| ServiceError::Validation(format!("device id: {e}")))?;
                destinations.push(ProtocolAddress::new(member.clone(), device));
            }
            if *member == ours.name() {
                destinations.push(ours.clone());
            }
        }
        let sealed = self.actor
            .encrypt_multi_recipient(contents, sender_cert, destinations.clone(), channel_id.as_bytes().to_vec())
            .await?;
        Ok((sealed, destinations))
    }

    /// Get cached sender certificate without making auth request.
    ///
    /// Returns None if no valid cached certificate exists.
    pub fn get_cached_sender_certificate(&self) -> Result<Option<SenderCertificate>, ServiceError> {
        let cached_cert_b64: Option<String> =
            self.settings_repo.get_string(keys::SENDER_CERTIFICATE)?;
        let cached_refresh_after: Option<u64> = self
            .settings_repo
            .get::<u64>(keys::SENDER_CERTIFICATE_REFRESH_AFTER)?;

        if let (Some(cert_b64), Some(refresh_after)) = (&cached_cert_b64, cached_refresh_after) {
            if !sender_cert_refresh_due(std::time::SystemTime::now(), refresh_after) {
                let root = self.trust_root.serialize().to_vec();
                if let Ok(cache) = self.sender_cert_cache.lock() {
                    if let Some(saved) = cache.as_ref() {
                        // Settings remain authoritative. A changed certificate or
                        // trust root must go through signature verification again.
                        if saved.cert_b64 == *cert_b64
                            && saved.root == root
                            && cert_not_expired(&saved.cert)
                            && self.cert_is_for_this_device(&saved.cert)
                        {
                            return Ok(Some(saved.cert.clone()));
                        }
                    }
                }
                use base64::Engine;
                if let Ok(cert_bytes) = base64::engine::general_purpose::STANDARD.decode(cert_b64) {
                    if let Ok(cert) = SenderCertificate::deserialize(&cert_bytes) {
                        // The certificate names the sender's user AND device.
                        // Every login is a new device id, so a cert cached
                        // before a Log Out -> Log In names a device that no
                        // longer exists: recipients looked up the wrong session
                        // and every sealed message failed to decrypt. Only a
                        // cert for the current user and device is reusable.
                        //
                        // It must also still chain to the trust root this build
                        // pins: a cert cached under an older server signing key
                        // (a key rotation, or a dev stack re-seeded) is rejected
                        // by every recipient, who then drops the message
                        // silently. Refetch instead of sending into that.
                        if self.cert_is_for_this_device(&cert)
                            && crate::crypto::signal::sealed_sender::validate_trust_root(&cert)
                        {
                            if let Ok(mut cache) = self.sender_cert_cache.lock() {
                                *cache = Some(VerifiedSenderCert {
                                    cert_b64: cert_b64.clone(),
                                    root,
                                    cert: cert.clone(),
                                });
                            }
                            return Ok(Some(cert));
                        }
                    }
                }
            }
        }

        Ok(None)
    }

    fn cert_is_for_this_device(&self, cert: &SenderCertificate) -> bool {
        let user = self
            .settings_repo
            .get_string(keys::CURRENT_USER_ID)
            .ok()
            .flatten();
        let device: Option<u32> = self
            .settings_repo
            .get(keys::CURRENT_DEVICE_ID)
            .ok()
            .flatten();
        match (user, device, cert.sender_uuid(), cert.sender_device_id()) {
            (Some(u), Some(d), Ok(cu), Ok(cd)) => u == cu && u32::from(cd) == d,
            _ => false,
        }
    }

    /// Get the cached server key ID from the sender certificate.
    ///
    /// Returns the server_key_id that was cached when the sender certificate
    /// was fetched. Falls back to 1 if not yet cached (pre-first-fetch).
    pub fn get_server_key_id(&self) -> u32 {
        self.settings_repo
            .get::<u32>(keys::SENDER_CERTIFICATE_SERVER_KEY_ID)
            .ok()
            .flatten()
            .unwrap_or(1)
    }
}

/// Whether a cached sender certificate is due for refresh. `refresh_after_ns`
/// is the server's `RequestSenderCertificateResponse.refresh_after`, Unix
/// NANOSECONDS (auth/service/sealed_sender.go: `refreshAfter.UnixNano()`).
/// Comparing it against milliseconds made "now" ~10^6 times too small, so the
/// cache never refreshed early and was only replaced once recipients had
/// started rejecting the expired certificate.
fn sender_cert_refresh_due(now: std::time::SystemTime, refresh_after_ns: u64) -> bool {
    let now_ns = now
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
        .unwrap_or(0);
    now_ns >= refresh_after_ns
}

fn cert_not_expired(cert: &SenderCertificate) -> bool {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0);
    cert.expiration()
        .is_ok_and(|expiry| expiry.epoch_millis() >= now_ms)
}

#[cfg(test)]
mod sender_cert_refresh_tests {
    use super::sender_cert_refresh_due;
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn refresh_after_is_compared_in_nanoseconds() {
        let now = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let now_ns = 1_800_000_000u64 * 1_000_000_000;
        // A refresh point one hour in the past (as the server sends it, in ns)
        // is due; the old millisecond comparison called it 10^6 x in the future.
        assert!(sender_cert_refresh_due(now, now_ns - 3_600 * 1_000_000_000));
        assert!(sender_cert_refresh_due(now, now_ns));
        // One hour ahead is not due yet.
        assert!(!sender_cert_refresh_due(
            now,
            now_ns + 3_600 * 1_000_000_000
        ));
    }

    #[test]
    fn a_millisecond_scale_value_is_long_past() {
        // Guards the regression direction: a value on the ms scale for "now"
        // is ~1970 in nanoseconds, so it must read as due, never as fresh.
        let now = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        assert!(sender_cert_refresh_due(now, 1_800_000_000_000));
    }
}

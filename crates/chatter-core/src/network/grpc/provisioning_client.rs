//! Device provisioning gRPC client.
//!
//! Client for device linking operations via QR code provisioning.
//! Supports both new device (linking) and primary device (approving) flows.

use crate::network::grpc::transport::GrpcTransport;
use tonic::service::interceptor::InterceptedService;

use crate::error::NetworkError;
use crate::network::grpc::interceptors::AuthInterceptor;
use crate::proto::provisioning_v1::{
    CancelProvisioningRequest, CompleteDeviceLinkingRequest, CompleteDeviceLinkingResponse,
    GetProvisioningMessageRequest, GetProvisioningMessageResponse, GetTransferArchiveRequest,
    ListProvisioningSessionsRequest, ListProvisioningSessionsResponse, ProvisionDeviceRequest,
    ProvisionEnvelope, SetTransferArchiveRequest, StartProvisioningRequest,
    StartProvisioningResponse, device_provisioning_service_client::DeviceProvisioningServiceClient,
    get_provisioning_message_response, get_transfer_archive_response,
};
use crate::proto::{SignalPreKeyBundle, signal_pre_key_bundle::PreKeyRecord};

/// Result of polling for provisioning message.
#[derive(Debug, Clone)]
pub enum ProvisioningPollResult {
    /// Primary has sent encrypted keys - proceed with ECDH decryption
    Envelope(ProvisionEnvelope),
    /// Not ready yet - keep polling after the specified delay
    Pending { retry_after_ms: u32 },
    /// Session expired - need to restart
    Expired { message: String },
    /// Session was cancelled by primary
    Cancelled { message: String },
}

/// Result of polling for the Link'n'Sync transfer archive (§30).
#[derive(Debug, Clone)]
pub enum TransferArchivePoll {
    /// Ciphertext is uploaded — download it from `download_url`, decrypt, import.
    Ready { download_url: String, size: u64 },
    /// Primary has not finished exporting yet — keep polling.
    Pending { retry_after_ms: u32 },
    /// Primary cancelled, its export failed, or the session ended — stop
    /// polling. The device links without history rather than hanging.
    Failed { message: String },
}

/// Device provisioning client wrapper.
///
/// Wraps the generated DeviceProvisioningServiceClient with ergonomic APIs.
///
/// # Flows
///
/// ## New Device (Linking)
/// 1. `start_provisioning()` - Create session, get UUID for QR code
/// 2. `poll_provisioning_message()` - Poll until primary sends keys
/// 3. Decrypt envelope with ECDH
/// 4. `complete_device_linking()` - Finish with provisioning code
///
/// ## Primary Device (Approving)
/// 1. Scan QR code to get UUID and ephemeral public key
/// 2. `provision_device()` - Send ECDH-encrypted keys
/// 3. Optionally `cancel_provisioning()` to abort
pub struct ProvisioningClient {
    inner: DeviceProvisioningServiceClient<InterceptedService<GrpcTransport, AuthInterceptor>>,
}

impl ProvisioningClient {
    /// Create a new ProvisioningClient from pool components.
    pub fn new(channel: GrpcTransport, interceptor: AuthInterceptor) -> Self {
        Self {
            inner: DeviceProvisioningServiceClient::with_interceptor(channel, interceptor),
        }
    }

    /// Start a new provisioning session (new device initiates).
    ///
    /// Returns session UUID to embed in QR code along with ephemeral public key.
    /// QR format: `chatter://link?uuid={uuid}&pub_key={base64_pubkey}`
    ///
    /// # Arguments
    /// * `device_info` - Platform/model info (e.g., "Linux x86_64")
    ///
    /// No device name: the new device cannot encrypt one until the
    /// ProvisionMessage hands it the account identity key, so the name is
    /// sent (encrypted) at `complete_device_linking`.
    pub async fn start_provisioning(
        &mut self,
        device_info: &str,
    ) -> Result<StartProvisioningResponse, NetworkError> {
        let request = StartProvisioningRequest {
            device_info: device_info.to_string(),
        };

        self.inner
            .start_provisioning(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Poll for provisioning message from primary device.
    ///
    /// New device calls this repeatedly until it receives the encrypted envelope
    /// or the session expires/is cancelled.
    ///
    /// # Arguments
    /// * `provisioning_uuid` - Session UUID from start_provisioning
    pub async fn poll_provisioning_message(
        &mut self,
        provisioning_uuid: &str,
    ) -> Result<ProvisioningPollResult, NetworkError> {
        let request = GetProvisioningMessageRequest {
            provisioning_uuid: provisioning_uuid.to_string(),
        };

        let response: GetProvisioningMessageResponse = self
            .inner
            .get_provisioning_message(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))?;

        match response.result {
            Some(get_provisioning_message_response::Result::Envelope(env)) => {
                Ok(ProvisioningPollResult::Envelope(env))
            }
            Some(get_provisioning_message_response::Result::Pending(p)) => {
                Ok(ProvisioningPollResult::Pending {
                    retry_after_ms: p.retry_after_ms,
                })
            }
            Some(get_provisioning_message_response::Result::Expired(e)) => {
                Ok(ProvisioningPollResult::Expired { message: e.message })
            }
            Some(get_provisioning_message_response::Result::Cancelled(c)) => {
                Ok(ProvisioningPollResult::Cancelled { message: c.message })
            }
            None => Ok(ProvisioningPollResult::Pending {
                retry_after_ms: 2000,
            }),
        }
    }

    /// Send encrypted provisioning data to new device (primary device calls).
    ///
    /// Called by authenticated primary device after scanning QR code.
    /// The envelope contains ECDH-encrypted identity keys, profile key, etc.
    ///
    /// # Arguments
    /// * `provisioning_uuid` - Session UUID from QR code
    /// * `envelope` - ECDH-encrypted ProvisionMessage
    /// * `provisioning_code` - The same code primary embedded inside the
    ///   encrypted envelope. Server stores it cleartext; new device echoes
    ///   back the decrypted copy in `complete_device_linking` to prove it
    ///   successfully decrypted. Server compares strict equality.
    pub async fn provision_device(
        &mut self,
        provisioning_uuid: &str,
        envelope: ProvisionEnvelope,
        provisioning_code: &str,
    ) -> Result<(), NetworkError> {
        let request = ProvisionDeviceRequest {
            provisioning_uuid: provisioning_uuid.to_string(),
            envelope: Some(envelope),
            request_id: uuid::Uuid::new_v4().to_string(),
            provisioning_code: provisioning_code.to_string(),
        };

        self.inner
            .provision_device(request)
            .await
            .map(|_| ())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Complete device linking (new device calls after decrypting envelope).
    ///
    /// New device decrypts the ProvisionEnvelope, extracts the provisioning code,
    /// and calls this with its prekey bundle to complete linking.
    ///
    /// # Arguments
    /// * `provisioning_code` - Code from decrypted ProvisionMessage
    /// * `device_auth_token` - 32-byte client-generated auth token
    /// * `device_info` - Platform/model info
    /// * `encrypted_name` - Serialized `DeviceName`, encrypted to the account
    ///   identity key from the ProvisionMessage (opaque to the server)
    /// * `prekey_bundle` - Signal Protocol prekey bundle for this device
    /// * `one_time_prekeys` - Initial batch of one-time prekeys
    #[allow(clippy::too_many_arguments)]
    pub async fn complete_device_linking(
        &mut self,
        provisioning_code: &str,
        device_auth_token: &[u8; 32],
        device_info: &str,
        encrypted_name: Vec<u8>,
        prekey_bundle: SignalPreKeyBundle,
        one_time_prekeys: Vec<PreKeyRecord>,
    ) -> Result<CompleteDeviceLinkingResponse, NetworkError> {
        let request = CompleteDeviceLinkingRequest {
            provisioning_code: provisioning_code.to_string(),
            device_auth_token: device_auth_token.to_vec(),
            device_info: device_info.to_string(),
            encrypted_name,
            prekey_bundle: Some(prekey_bundle),
            initial_one_time_prekeys: one_time_prekeys,
            request_id: uuid::Uuid::new_v4().to_string(),
        };

        self.inner
            .complete_device_linking(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Cancel an in-progress provisioning session (primary device calls).
    ///
    /// # Arguments
    /// * `provisioning_uuid` - Session UUID to cancel
    pub async fn cancel_provisioning(
        &mut self,
        provisioning_uuid: &str,
    ) -> Result<(), NetworkError> {
        let request = CancelProvisioningRequest {
            provisioning_uuid: provisioning_uuid.to_string(),
            request_id: uuid::Uuid::new_v4().to_string(),
        };

        self.inner
            .cancel_provisioning(request)
            .await
            .map(|_| ())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// List active provisioning sessions for the authenticated user.
    pub async fn list_provisioning_sessions(
        &mut self,
    ) -> Result<ListProvisioningSessionsResponse, NetworkError> {
        let request = ListProvisioningSessionsRequest {};

        self.inner
            .list_provisioning_sessions(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Announce the Link'n'Sync transfer archive for a session (primary calls,
    /// authenticated). On success returns a presigned PUT URL to upload the
    /// ciphertext to; the server stores only the blob pointer + declared size,
    /// never the archive key. §30.
    ///
    /// # Arguments
    /// * `provisioning_uuid` - Session UUID (the archive's address)
    /// * `size` - Byte length of the encrypted archive about to be uploaded
    /// * `export_failed` - When true, no size/URL: signals the new device to
    ///   stop polling and link without history rather than hang.
    pub async fn set_transfer_archive(
        &mut self,
        provisioning_uuid: &str,
        size: u64,
        export_failed: bool,
    ) -> Result<String, NetworkError> {
        let request = SetTransferArchiveRequest {
            provisioning_uuid: provisioning_uuid.to_string(),
            size,
            export_failed,
        };

        self.inner
            .set_transfer_archive(request)
            .await
            .map(|r| r.into_inner().upload_url)
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Poll for the transfer archive location (new device calls, unauthenticated
    /// — authorized only by holding the session UUID, like
    /// `poll_provisioning_message`). §30.
    pub async fn get_transfer_archive(
        &mut self,
        provisioning_uuid: &str,
    ) -> Result<TransferArchivePoll, NetworkError> {
        let request = GetTransferArchiveRequest {
            provisioning_uuid: provisioning_uuid.to_string(),
        };

        let response = self
            .inner
            .get_transfer_archive(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))?;

        match response.result {
            Some(get_transfer_archive_response::Result::Ready(ready)) => {
                Ok(TransferArchivePoll::Ready {
                    download_url: ready.download_url,
                    size: ready.size,
                })
            }
            Some(get_transfer_archive_response::Result::Pending(p)) => {
                Ok(TransferArchivePoll::Pending {
                    retry_after_ms: p.retry_after_ms,
                })
            }
            Some(get_transfer_archive_response::Result::Error(e)) => {
                Ok(TransferArchivePoll::Failed { message: e.message })
            }
            // A server that answered without a oneof arm is treated as
            // still-pending; the caller's overall deadline bounds the loop.
            None => Ok(TransferArchivePoll::Pending {
                retry_after_ms: 2000,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_poll_result_variants() {
        // Test that all variants can be created
        let _envelope = ProvisioningPollResult::Envelope(ProvisionEnvelope {
            public_key: vec![0u8; 32],
            body: vec![0u8; 80],
        });
        let _pending = ProvisioningPollResult::Pending {
            retry_after_ms: 2000,
        };
        let _expired = ProvisioningPollResult::Expired {
            message: "Session expired".to_string(),
        };
        let _cancelled = ProvisioningPollResult::Cancelled {
            message: "Cancelled by primary".to_string(),
        };
    }
}

//! Session management and encryption/decryption operations.
//!
//! Handles establishing Signal Protocol sessions and encrypting/decrypting
//! messages using both sealed sender and identified sender modes.
//! All operations are delegated to the SignalActor for thread-safety.

use libsignal_protocol::{
    CiphertextMessageType, DeviceId, ProtocolAddress, PublicKey, SenderCertificate,
};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use super::{SignalService, kt_anchor};
use crate::error::{CryptoError, ServiceError};
use crate::network::grpc::auth_client::AuthClient;
use crate::storage::repositories::settings_repo::keys;

mod certificate;
mod devices;
mod encrypt;
mod establish;
pub(super) mod kt;
mod receive;

pub(crate) enum PreKeySource<'a> {
    Identified(&'a mut AuthClient),
    Anonymous {
        pool: &'a crate::network::grpc::client_pool::GrpcClientPool,
        tokens: &'a crate::mailbox::token_manager::TokenManager,
    },
}

impl PreKeySource<'_> {
    async fn fetch(
        &mut self,
        peer: &str,
        device: &str,
    ) -> Result<chatter_proto::chat_v1::GetPreKeyBundleResponse, crate::error::NetworkError> {
        match self {
            Self::Identified(client) => client.get_pre_key_bundle(peer, device).await,
            Self::Anonymous { pool, tokens } => {
                // Minting immediately before discovery would link the authenticated
                // mint to this request. Wait for the independent token refill.
                let token = tokens
                    .claim_prefetched_token()
                    .map_err(|_| crate::error::NetworkError::Unavailable)?;
                pool.anonymous_auth_client()
                    .get_pre_key_bundle_anonymous(peer, device, &token.public_key, &token.signature)
                    .await
            }
        }
    }
}


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
        route: AnonymousPreKeyRoute<'a>,
        tokens: &'a crate::mailbox::token_manager::TokenManager,
        claim: fn(&crate::mailbox::token_manager::TokenManager)
            -> Result<crate::mailbox::token_manager::MailboxToken, crate::error::NetworkError>,
    },
}

pub(crate) enum AnonymousPreKeyRoute<'a> {
    Client(crate::network::grpc::AnonymousAuthClient),
    Secret(&'a crate::network::grpc::GrpcClientPool),
}

impl PreKeySource<'_> {
    async fn fetch(
        &mut self,
        peer: &str,
        device: &str,
    ) -> Result<chatter_proto::chat_v1::GetPreKeyBundleResponse, crate::error::NetworkError> {
        match self {
            Self::Identified(client) => client.get_pre_key_bundle(peer, device).await,
            Self::Anonymous {
                route,
                tokens,
                claim,
            } => {
                let mut secret_client;
                let client = match route {
                    AnonymousPreKeyRoute::Client(client) => client,
                    AnonymousPreKeyRoute::Secret(pool) => {
                        secret_client = pool.secret_auth_client()?;
                        &mut secret_client
                    }
                };
                // Minting immediately before discovery would link the authenticated
                // mint to this request. Wait for the independent token refill.
                let token = claim(tokens)?;
                client
                    .get_pre_key_bundle_anonymous(peer, device, &token.public_key, &token.signature)
                    .await
            }
        }
    }
}


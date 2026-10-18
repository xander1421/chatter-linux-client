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
#[cfg(feature = "dev-trust-root")]
use crate::crypto::keytransparency::KtError;
use crate::error::{CryptoError, ServiceError};
use crate::network::grpc::auth_client::AuthClient;
use crate::storage::repositories::settings_repo::keys;

mod certificate;
mod devices;
mod encrypt;
mod establish;
mod kt;
mod receive;

pub use encrypt::DeviceCiphertext;

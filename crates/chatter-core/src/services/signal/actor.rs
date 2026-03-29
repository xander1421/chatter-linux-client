//! Signal Actor - Dedicated thread for libsignal-protocol operations.
//!
//! Encapsulates all non-Send/Sync libsignal operations into a single-threaded
//! actor, providing an async handle for the rest of the application.

use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use libsignal_protocol::{
    ProtocolAddress, DeviceId,
    PublicKey, PreKeyBundle,
    CiphertextMessageType,
    SenderCertificate, SessionRecord,
};
use uuid::Uuid;
use rand::SeedableRng;

use crate::domain::ids::{ChannelId, UserId};
use crate::error::{ServiceError, CryptoError};
use crate::crypto::{
    SignalStore, SessionManager, SealedSenderManager,
    signal::sender_key::SenderKeyManager,
    signal::sesame::ConvergenceResult,
};
use crate::storage::KeyStore;
use crate::storage::repositories::settings_repo::SettingsRepository;

/// Commands sent to the Signal Actor.
pub enum SignalCommand {
    GetIdentityFingerprint { response: oneshot::Sender<Result<String, ServiceError>> },
    GetIdentityPublicKey { response: oneshot::Sender<Result<[u8; 33], ServiceError>> },
    /// Get the identity private key (32 bytes X25519 scalar) for ECIES decryption.
    GetIdentityPrivateKey { response: oneshot::Sender<Result<[u8; 32], ServiceError>> },
    RefreshPrekeys { count: u32, response: oneshot::Sender<Result<Vec<crate::proto::chat_v1::signal_pre_key_bundle::PreKeyRecord>, ServiceError>> },
    EstablishSession { 
        address: ProtocolAddress, 
        bundle: PreKeyBundle, 
        response: oneshot::Sender<Result<(), ServiceError>> 
    },
    EncryptSealed {
        plaintext: Vec<u8>,
        sender_cert: SenderCertificate,
        recipient_address: ProtocolAddress,
        response: oneshot::Sender<Result<Vec<u8>, ServiceError>>,
    },
    EncryptIdentified {
        plaintext: Vec<u8>,
        recipient_address: ProtocolAddress,
        response: oneshot::Sender<Result<(Vec<u8>, u8), ServiceError>>,
    },
    DecryptSealed {
        ciphertext: Vec<u8>,
        trust_root: PublicKey,
        local_uuid: String,
        local_device_id: DeviceId,
        response: oneshot::Sender<Result<(ProtocolAddress, Vec<u8>), ServiceError>>,
    },
    DecryptIdentified {
        ciphertext: Vec<u8>,
        sender_address: ProtocolAddress,
        response: oneshot::Sender<Result<Vec<u8>, ServiceError>>,
    },
    DecryptWithConvergence {
        envelope: Vec<u8>,
        sender_id: String,
        sender_device_id: u32,
        sessions: Vec<(Vec<u8>, SessionRecord)>,
        response: oneshot::Sender<Result<ConvergenceResult, ServiceError>>,
    },
    CreateSenderKey { channel_id: ChannelId, our_address: ProtocolAddress, response: oneshot::Sender<Result<Vec<u8>, ServiceError>> },
    ProcessSenderKey { channel_id: ChannelId, sender_address: ProtocolAddress, skdm_bytes: Vec<u8>, response: oneshot::Sender<Result<(), ServiceError>> },
    EncryptForGroup { channel_id: ChannelId, our_address: ProtocolAddress, plaintext: Vec<u8>, response: oneshot::Sender<Result<Vec<u8>, ServiceError>> },
    DecryptFromGroup { channel_id: ChannelId, sender_address: ProtocolAddress, ciphertext: Vec<u8>, response: oneshot::Sender<Result<Vec<u8>, ServiceError>> },
    DeriveCallEncryptionKey { peer_id: UserId, peer_device_id: u32, response: oneshot::Sender<Result<[u8; 32], ServiceError>> },
    /// Derive an ephemeral call key with identity binding (for DM calls with PFS).
    ///
    /// Completes ephemeral call key exchange: ECDH + identity lookup + HKDF.
    /// All crypto stays in the service — UI just passes raw key bytes.
    CompleteCallKeyExchange {
        our_ephemeral_private: [u8; 32],
        peer_ephemeral_public: [u8; 32],
        peer_id: UserId,
        call_id: String,
        we_are_caller: bool,
        response: oneshot::Sender<Result<[u8; 32], ServiceError>>,
    },
    HasSenderKey { channel_id: ChannelId, our_address: ProtocolAddress, response: oneshot::Sender<Result<bool, ServiceError>> },
    HasSession { address: ProtocolAddress, response: oneshot::Sender<Result<bool, ServiceError>> },
    ClearChannelSenderKeys { channel_id: ChannelId, response: oneshot::Sender<Result<(), ServiceError>> },
    MarkChannelForRotation { channel_id: ChannelId, response: oneshot::Sender<Result<(), ServiceError>> },
    /// Generate a Signal safety number for a peer (60-digit fingerprint).
    GenerateSafetyNumber {
        local_user_id: String,
        peer_user_id: String,
        response: oneshot::Sender<Result<String, ServiceError>>,
    },
    /// Generate scannable fingerprint bytes for QR code.
    GenerateScannableFingerprint {
        local_user_id: String,
        peer_user_id: String,
        response: oneshot::Sender<Result<Vec<u8>, ServiceError>>,
    },
}

/// Handle to the Signal Actor.
#[derive(Clone)]
pub struct SignalActorHandle {
    sender: mpsc::Sender<SignalCommand>,
}

impl SignalActorHandle {
    pub fn spawn(key_store: Arc<KeyStore>, settings_repo: Arc<SettingsRepository>) -> Self {
        let (sender, mut receiver) = mpsc::channel(100);
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            let mut store = SignalStore::new(key_store);
            runtime.block_on(async {
                while let Some(cmd) = receiver.recv().await {
                    match cmd {
                        SignalCommand::GetIdentityFingerprint { response } => {
                            use libsignal_protocol::IdentityKeyStore;
                            let res = store.get_identity_key_pair().await
                                .map_err(|e| ServiceError::Crypto(CryptoError::Protocol(e.to_string())))
                                .map(|identity| {
                                    let bytes = identity.public_key().serialize();
                                    bytes.chunks(4)
                                        .map(|chunk| chunk.iter().map(|b| format!("{:02X}", b)).collect::<String>())
                                        .collect::<Vec<_>>()
                                        .join(" ")
                                });
                            let _ = response.send(res);
                        }

                        SignalCommand::GetIdentityPublicKey { response } => {
                            use libsignal_protocol::IdentityKeyStore;
                            let res = store.get_identity_key_pair().await
                                .map_err(|e| ServiceError::Crypto(CryptoError::Protocol(e.to_string())))
                                .and_then(|identity| {
                                    let serialized = identity.public_key().serialize();
                                    let bytes: [u8; 33] = serialized.as_ref().try_into()
                                        .map_err(|_| ServiceError::Crypto(CryptoError::Protocol(
                                            "identity key should be 33 bytes".into()
                                        )))?;
                                    Ok(bytes)
                                });
                            let _ = response.send(res);
                        }

                        SignalCommand::GetIdentityPrivateKey { response } => {
                            use libsignal_protocol::IdentityKeyStore;
                            let res = store.get_identity_key_pair().await
                                .map_err(|e| ServiceError::Crypto(CryptoError::Protocol(e.to_string())))
                                .map(|identity| {
                                    // Get the 32-byte private scalar for X25519
                                    let serialized = identity.private_key().serialize();
                                    let mut bytes = [0u8; 32];
                                    bytes.copy_from_slice(&serialized);
                                    bytes
                                });
                            let _ = response.send(res);
                        }

                        SignalCommand::RefreshPrekeys { count, response } => {
                            use crate::crypto::signal::PreKeyManager;
                            let prekey_manager = PreKeyManager::new(store.key_store());
                            let mut rng = rand::rngs::StdRng::from_os_rng();
                            
                            let start_id = prekey_manager.get_next_prekey_id().map_err(ServiceError::Crypto);
                            if let Err(e) = start_id { let _ = response.send(Err(e)); continue; }
                            let start_id = start_id.unwrap();

                            let prekeys = prekey_manager.generate_prekeys(&mut rng, start_id, count).map_err(ServiceError::Crypto);
                            if let Err(e) = prekeys { let _ = response.send(Err(e)); continue; }
                            let prekeys = prekeys.unwrap();

                            let proto_prekeys = prekeys.iter().filter_map(|pk| {
                                Some(crate::proto::chat_v1::signal_pre_key_bundle::PreKeyRecord {
                                    id: u32::from(pk.id().ok()?),
                                    public_key: pk.public_key().ok()?.serialize().to_vec(),
                                })
                            }).collect();
                            let _ = response.send(Ok(proto_prekeys));
                        }

                        SignalCommand::EstablishSession { address, bundle, response } => {
                            let mut rng = rand::rngs::StdRng::from_os_rng();
                            let mut store2 = store.clone();
                            let res = SessionManager::establish_session(&address, &mut store, &mut store2, &bundle, &mut rng)
                                .await
                                .map_err(ServiceError::Crypto);
                            let _ = response.send(res);
                        }

                        SignalCommand::EncryptSealed { plaintext, sender_cert, recipient_address, response } => {
                            let mut store2 = store.clone();
                            let res = SealedSenderManager::encrypt(&plaintext, &sender_cert, &recipient_address, &mut store, &mut store2)
                                .await
                                .map_err(ServiceError::Crypto);
                            let _ = response.send(res);
                        }

                        SignalCommand::EncryptIdentified { plaintext, recipient_address, response } => {
                            let mut rng = rand::rngs::StdRng::from_os_rng();
                            let mut store2 = store.clone();
                            let res = match libsignal_protocol::message_encrypt(&plaintext, &recipient_address, &mut store, &mut store2, std::time::SystemTime::now(), &mut rng).await {
                                Ok(ciphertext) => {
                                    let msg_type = match ciphertext.message_type() {
                                        CiphertextMessageType::PreKey => 2u8,
                                        CiphertextMessageType::Whisper => 3u8,
                                        CiphertextMessageType::SenderKey => 4u8,
                                        CiphertextMessageType::Plaintext => 1u8,
                                    };
                                    Ok((ciphertext.serialize().to_vec(), msg_type))
                                }
                                Err(e) => Err(ServiceError::Crypto(CryptoError::Protocol(e.to_string()))),
                            };
                            let _ = response.send(res);
                        }

                        SignalCommand::DecryptSealed { ciphertext, trust_root, local_uuid, local_device_id, response } => {
                            let mut store2 = store.clone();
                            let mut store3 = store.clone();
                            let mut store4 = store.clone();
                            let store6 = store.clone();
                            let res = match SealedSenderManager::decrypt(
                                &ciphertext, &trust_root,
                                libsignal_protocol::Timestamp::from_epoch_millis(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64),
                                None, local_uuid, local_device_id,
                                &mut store, &mut store2, &mut store3, &store6, &mut store4,
                            ).await {
                                Ok(result) => Ok((ProtocolAddress::new(result.sender_uuid, result.device_id), result.message)),
                                Err(e) => Err(ServiceError::Crypto(CryptoError::Protocol(e.to_string()))),
                            };
                            let _ = response.send(res);
                        }

                        SignalCommand::DecryptIdentified { ciphertext, sender_address, response } => {
                            let mut rng = rand::rngs::StdRng::from_os_rng();
                            let mut store2 = store.clone();
                            let mut store3 = store.clone();
                            let mut store4 = store.clone();
                            let store6 = store.clone();
                            
                            let res = if let Ok(prekey_msg) = libsignal_protocol::PreKeySignalMessage::try_from(ciphertext.as_slice()) {
                                libsignal_protocol::message_decrypt_prekey(&prekey_msg, &sender_address, &mut store, &mut store2, &mut store3, &store6, &mut store4, &mut rng).await
                                    .map_err(|e| ServiceError::Crypto(CryptoError::Protocol(e.to_string())))
                            } else if let Ok(signal_msg) = libsignal_protocol::SignalMessage::try_from(ciphertext.as_slice()) {
                                libsignal_protocol::message_decrypt_signal(&signal_msg, &sender_address, &mut store, &mut store2, &mut rng).await
                                    .map_err(|e| ServiceError::Crypto(CryptoError::Protocol(e.to_string())))
                            } else {
                                Err(ServiceError::Crypto(CryptoError::Protocol("Unknown ciphertext format".into())))
                            };
                            let _ = response.send(res);
                        }

                        SignalCommand::DecryptWithConvergence { envelope, sender_id, sender_device_id, sessions, response } => {
                            let device_id_struct = match DeviceId::try_from(sender_device_id).map_err(|e| ServiceError::Validation(e.to_string())) {
                                Ok(id) => id,
                                Err(e) => { let _ = response.send(Err(e)); continue; }
                            };
                            let sender_address = ProtocolAddress::new(sender_id, device_id_struct);
                            
                            let mut store2 = store.clone();
                            let mut store3 = store.clone();
                            let mut store4 = store.clone();
                            let store6 = store.clone();

                            let mut final_res = Ok(ConvergenceResult::AllFailed { errors: vec!["No sessions to try".into()] });
                            
                            for (session_id, record) in sessions {
                                let mut rng = rand::rngs::StdRng::from_os_rng();
                                if let Ok(serialized) = record.serialize() {
                                    if store2.key_store().store_session(&format!("{}:{}", sender_address.name(), sender_address.device_id()), &serialized).is_ok() {
                                        if let Ok(prekey_msg) = libsignal_protocol::PreKeySignalMessage::try_from(envelope.as_slice()) {
                                            if let Ok(plaintext) = libsignal_protocol::message_decrypt_prekey(&prekey_msg, &sender_address, &mut store, &mut store2, &mut store3, &store6, &mut store4, &mut rng).await {
                                                final_res = Ok(ConvergenceResult::ActiveSession { plaintext, session_id });
                                                break;
                                            }
                                        } else if let Ok(signal_msg) = libsignal_protocol::SignalMessage::try_from(envelope.as_slice()) {
                                            if let Ok(plaintext) = libsignal_protocol::message_decrypt_signal(&signal_msg, &sender_address, &mut store, &mut store2, &mut rng).await {
                                                final_res = Ok(ConvergenceResult::ActiveSession { plaintext, session_id });
                                                break;
                                            }
                                        }
                                    }
                                }
                            }
                            let _ = response.send(final_res);
                        }

                        SignalCommand::CreateSenderKey { channel_id, our_address, response } => {
                            let distribution_id = Uuid::parse_str(&channel_id.to_string()).unwrap();
                            let res = SenderKeyManager::create_distribution_message(&our_address, distribution_id, &mut store).await
                                .map(|skdm| skdm.serialized().to_vec())
                                .map_err(ServiceError::Crypto);
                            let _ = response.send(res);
                        }

                        SignalCommand::ProcessSenderKey { channel_id: _, sender_address, skdm_bytes, response } => {
                            let res = match libsignal_protocol::SenderKeyDistributionMessage::try_from(skdm_bytes.as_slice()) {
                                Ok(skdm) => {
                                    SenderKeyManager::process_distribution_message(&sender_address, &skdm, &mut store).await
                                        .map_err(ServiceError::Crypto)
                                }
                                Err(e) => Err(ServiceError::Crypto(CryptoError::Protocol(e.to_string()))),
                            };
                            let _ = response.send(res);
                        }

                        SignalCommand::EncryptForGroup { channel_id, our_address, plaintext, response } => {
                            let distribution_id = Uuid::parse_str(&channel_id.to_string()).unwrap();
                            let res = SenderKeyManager::encrypt(&our_address, distribution_id, &plaintext, &mut store).await
                                .map(|c| c.serialized().to_vec())
                                .map_err(ServiceError::Crypto);
                            let _ = response.send(res);
                        }

                        SignalCommand::DecryptFromGroup { channel_id: _, sender_address, ciphertext, response } => {
                            let res = SenderKeyManager::decrypt(&ciphertext, &sender_address, &mut store).await
                                .map_err(ServiceError::Crypto);
                            let _ = response.send(res);
                        }

                        SignalCommand::DeriveCallEncryptionKey { peer_id, peer_device_id, response } => {
                            use libsignal_protocol::IdentityKeyStore;
                            let peer_address = ProtocolAddress::new(peer_id.to_string(), DeviceId::try_from(peer_device_id).unwrap());
                            
                            let our_identity = match store.get_identity_key_pair().await.map_err(|e| ServiceError::Crypto(CryptoError::Protocol(e.to_string()))) {
                                Ok(id) => id,
                                Err(e) => { let _ = response.send(Err(e)); continue; }
                            };
                            let peer_identity = match store.get_identity(&peer_address).await.map_err(|e| ServiceError::Crypto(CryptoError::Protocol(e.to_string()))) {
                                Ok(Some(id)) => id,
                                Ok(None) => { let _ = response.send(Err(ServiceError::Crypto(CryptoError::SessionNotFound(peer_address.to_string())))); continue; }
                                Err(e) => { let _ = response.send(Err(e)); continue; }
                            };

                            use x25519_dalek::{PublicKey as X25519PublicKey, StaticSecret as X25519PrivateKey};
                            let mut our_scalar = [0u8; 32];
                            our_scalar.copy_from_slice(&our_identity.private_key().serialize());
                            let our_x25519_private = X25519PrivateKey::from(our_scalar);

                            let mut peer_point = [0u8; 32];
                            peer_point.copy_from_slice(&peer_identity.public_key().serialize()[1..33]);
                            let peer_x25519_public = X25519PublicKey::from(peer_point);

                            let shared_secret = our_x25519_private.diffie_hellman(&peer_x25519_public);
                            let _ = response.send(Ok(*shared_secret.as_bytes()));
                        }

                        SignalCommand::CompleteCallKeyExchange { our_ephemeral_private, peer_ephemeral_public, peer_id, call_id, we_are_caller, response } => {
                            use libsignal_protocol::IdentityKeyStore;
                            use crate::crypto::{EphemeralCallKey, derive_call_key_ephemeral};

                            // ECDH: compute shared secret from ephemeral keys
                            let ephemeral = EphemeralCallKey::from_private_bytes(our_ephemeral_private);
                            let ephemeral_shared = ephemeral.diffie_hellman(&peer_ephemeral_public);

                            let our_identity = match store.get_identity_key_pair().await
                                .map_err(|e| ServiceError::Crypto(CryptoError::Protocol(e.to_string()))) {
                                Ok(id) => id,
                                Err(e) => { let _ = response.send(Err(e)); continue; }
                            };

                            let peer_address = ProtocolAddress::new(peer_id.to_string(), DeviceId::try_from(1u32).unwrap());
                            let peer_identity = match store.get_identity(&peer_address).await
                                .map_err(|e| ServiceError::Crypto(CryptoError::Protocol(e.to_string()))) {
                                Ok(Some(id)) => id,
                                Ok(None) => {
                                    let _ = response.send(Err(ServiceError::Crypto(
                                        CryptoError::SessionNotFound(format!("No identity key for peer {}", peer_id))
                                    )));
                                    continue;
                                }
                                Err(e) => { let _ = response.send(Err(e)); continue; }
                            };

                            let res = derive_call_key_ephemeral(
                                &ephemeral_shared,
                                &our_identity,
                                &peer_identity,
                                &call_id,
                                we_are_caller,
                            ).map_err(ServiceError::Crypto);
                            let _ = response.send(res);
                        }

                        SignalCommand::HasSenderKey { channel_id, our_address, response } => {
                            let distribution_id = Uuid::parse_str(&channel_id.to_string()).unwrap();
                            let sender_str = format!("{}:{}", our_address.name(), our_address.device_id());
                            let res = store.key_store().load_sender_key(&sender_str, &distribution_id.to_string())
                                .map_err(ServiceError::Storage)
                                .map(|k| k.is_some());
                            let _ = response.send(res);
                        }

                        SignalCommand::ClearChannelSenderKeys { channel_id, response } => {
                            let distribution_id = Uuid::parse_str(&channel_id.to_string()).unwrap();
                            let res = store.key_store().delete_sender_keys_for_distribution(&distribution_id.to_string())
                                .map_err(ServiceError::Storage);
                            let _ = response.send(res);
                        }

                        SignalCommand::MarkChannelForRotation { channel_id, response } => {
                            let key = format!("sender_key_rotation_needed:{}", channel_id);
                            let res = settings_repo.set_string(&key, "1").map_err(ServiceError::Storage);
                            let _ = response.send(res);
                        }

                        SignalCommand::HasSession { address, response } => {
                            let session_key = format!("{}:{}", address.name(), address.device_id());
                            let res = store.key_store().load_session(&session_key)
                                .map_err(ServiceError::Storage)
                                .map(|s| {
                                    match s {
                                        Some(bytes) => {
                                            // Validate session is structurally intact
                                            match SessionRecord::deserialize(&bytes) {
                                                Ok(_) => true,
                                                Err(e) => {
                                                    tracing::warn!(
                                                        address = %address,
                                                        error = %e,
                                                        "Session exists but is corrupted"
                                                    );
                                                    false
                                                }
                                            }
                                        }
                                        None => false,
                                    }
                                });
                            let _ = response.send(res);
                        }

                        SignalCommand::GenerateSafetyNumber { local_user_id, peer_user_id, response } => {
                            use libsignal_protocol::{IdentityKey, IdentityKeyStore};
                            let res = (|| async {
                                let local_pair = store.get_identity_key_pair().await
                                    .map_err(|e| ServiceError::Crypto(CryptoError::Protocol(e.to_string())))?;
                                let local_key = local_pair.identity_key();

                                let peer_bytes = store.key_store().get_identity_by_uuid(&peer_user_id)
                                    .map_err(ServiceError::Storage)?
                                    .ok_or_else(|| ServiceError::Crypto(CryptoError::SessionNotFound(
                                        format!("No identity key for peer {}", peer_user_id),
                                    )))?;
                                let peer_key = IdentityKey::decode(&peer_bytes)
                                    .map_err(|e| ServiceError::Crypto(CryptoError::Protocol(e.to_string())))?;

                                SessionManager::generate_fingerprint(
                                    &local_user_id, local_key,
                                    &peer_user_id, &peer_key,
                                ).map_err(ServiceError::Crypto)
                            })().await;
                            let _ = response.send(res);
                        }

                        SignalCommand::GenerateScannableFingerprint { local_user_id, peer_user_id, response } => {
                            use libsignal_protocol::{IdentityKey, IdentityKeyStore};
                            let res = (|| async {
                                let local_pair = store.get_identity_key_pair().await
                                    .map_err(|e| ServiceError::Crypto(CryptoError::Protocol(e.to_string())))?;
                                let local_key = local_pair.identity_key();

                                let peer_bytes = store.key_store().get_identity_by_uuid(&peer_user_id)
                                    .map_err(ServiceError::Storage)?
                                    .ok_or_else(|| ServiceError::Crypto(CryptoError::SessionNotFound(
                                        format!("No identity key for peer {}", peer_user_id),
                                    )))?;
                                let peer_key = IdentityKey::decode(&peer_bytes)
                                    .map_err(|e| ServiceError::Crypto(CryptoError::Protocol(e.to_string())))?;

                                SessionManager::generate_scannable_fingerprint(
                                    &local_user_id, local_key,
                                    &peer_user_id, &peer_key,
                                ).map_err(ServiceError::Crypto)
                            })().await;
                            let _ = response.send(res);
                        }
                    }
                }
            });
        });
        Self { sender }
    }

    async fn send<T>(&self, cmd_fn: impl FnOnce(oneshot::Sender<T>) -> SignalCommand) -> Result<T, ServiceError> {
        let (tx, rx) = oneshot::channel();
        self.sender.send(cmd_fn(tx)).await.map_err(|_| ServiceError::Internal("Actor dropped".into()))?;
        rx.await.map_err(|_| ServiceError::Internal("Response dropped".into()))
    }

    pub async fn get_identity_fingerprint(&self) -> Result<String, ServiceError> {
        self.send(|tx| SignalCommand::GetIdentityFingerprint { response: tx }).await?
    }

    pub async fn get_identity_public_key(&self) -> Result<[u8; 33], ServiceError> {
        self.send(|tx| SignalCommand::GetIdentityPublicKey { response: tx }).await?
    }

    /// Get the identity private key (32 bytes X25519 scalar) for ECIES decryption.
    pub async fn get_identity_private_key(&self) -> Result<[u8; 32], ServiceError> {
        self.send(|tx| SignalCommand::GetIdentityPrivateKey { response: tx }).await?
    }

    pub async fn refresh_prekeys(&self, count: u32) -> Result<Vec<crate::proto::chat_v1::signal_pre_key_bundle::PreKeyRecord>, ServiceError> {
        self.send(|tx| SignalCommand::RefreshPrekeys { count, response: tx }).await?
    }

    pub async fn establish_session(&self, address: ProtocolAddress, bundle: libsignal_protocol::PreKeyBundle) -> Result<(), ServiceError> {
        self.send(|tx| SignalCommand::EstablishSession { address, bundle, response: tx }).await?
    }

    pub async fn encrypt_sealed(&self, plaintext: Vec<u8>, sender_cert: SenderCertificate, recipient_address: ProtocolAddress) -> Result<Vec<u8>, ServiceError> {
        self.send(|tx| SignalCommand::EncryptSealed { plaintext, sender_cert, recipient_address, response: tx }).await?
    }

    pub async fn encrypt_identified(&self, plaintext: Vec<u8>, recipient_address: ProtocolAddress) -> Result<(Vec<u8>, u8), ServiceError> {
        self.send(|tx| SignalCommand::EncryptIdentified { plaintext, recipient_address, response: tx }).await?
    }

    pub async fn decrypt_sealed(&self, ciphertext: Vec<u8>, trust_root: libsignal_protocol::PublicKey, local_uuid: String, local_device_id: DeviceId) -> Result<(ProtocolAddress, Vec<u8>), ServiceError> {
        self.send(|tx| SignalCommand::DecryptSealed { ciphertext, trust_root, local_uuid, local_device_id, response: tx }).await?
    }

    pub async fn decrypt_identified(&self, ciphertext: Vec<u8>, sender_address: ProtocolAddress) -> Result<Vec<u8>, ServiceError> {
        self.send(|tx| SignalCommand::DecryptIdentified { ciphertext, sender_address, response: tx }).await?
    }

    pub async fn decrypt_with_convergence(&self, envelope: Vec<u8>, sender_id: String, sender_device_id: u32, sessions: Vec<(Vec<u8>, SessionRecord)>) -> Result<ConvergenceResult, ServiceError> {
        self.send(|tx| SignalCommand::DecryptWithConvergence { envelope, sender_id, sender_device_id, sessions, response: tx }).await?
    }

    pub async fn create_sender_key(&self, channel_id: ChannelId, our_address: ProtocolAddress) -> Result<Vec<u8>, ServiceError> {
        self.send(|tx| SignalCommand::CreateSenderKey { channel_id, our_address, response: tx }).await?
    }

    pub async fn process_sender_key(&self, channel_id: ChannelId, sender_address: ProtocolAddress, skdm_bytes: Vec<u8>) -> Result<(), ServiceError> {
        self.send(|tx| SignalCommand::ProcessSenderKey { channel_id, sender_address, skdm_bytes, response: tx }).await?
    }

    pub async fn encrypt_for_group(&self, channel_id: ChannelId, our_address: ProtocolAddress, plaintext: Vec<u8>) -> Result<Vec<u8>, ServiceError> {
        self.send(|tx| SignalCommand::EncryptForGroup { channel_id, our_address, plaintext, response: tx }).await?
    }

    pub async fn decrypt_from_group(&self, channel_id: ChannelId, sender_address: ProtocolAddress, ciphertext: Vec<u8>) -> Result<Vec<u8>, ServiceError> {
        self.send(|tx| SignalCommand::DecryptFromGroup { channel_id, sender_address, ciphertext, response: tx }).await?
    }

    pub async fn derive_call_encryption_key(&self, peer_id: UserId, peer_device_id: u32) -> Result<[u8; 32], ServiceError> {
        self.send(|tx| SignalCommand::DeriveCallEncryptionKey { peer_id, peer_device_id, response: tx }).await?
    }

    /// Derive an ephemeral call key with identity binding (for DM calls with PFS).
    ///
    /// Completes ephemeral call key exchange: ECDH + identity lookup + HKDF.
    ///
    /// Takes raw ephemeral key bytes so callers never perform crypto operations.
    /// The actor handles ECDH(our_ephemeral, peer_ephemeral), looks up both
    /// identity keys, and derives the final call encryption key via HKDF.
    pub async fn complete_call_key_exchange(
        &self,
        our_ephemeral_private: [u8; 32],
        peer_ephemeral_public: [u8; 32],
        peer_id: UserId,
        call_id: String,
        we_are_caller: bool,
    ) -> Result<[u8; 32], ServiceError> {
        self.send(|tx| SignalCommand::CompleteCallKeyExchange {
            our_ephemeral_private,
            peer_ephemeral_public,
            peer_id,
            call_id,
            we_are_caller,
            response: tx,
        }).await?
    }

    pub async fn has_sender_key(&self, channel_id: ChannelId, our_address: ProtocolAddress) -> Result<bool, ServiceError> {
        self.send(|tx| SignalCommand::HasSenderKey { channel_id, our_address, response: tx }).await?
    }

    pub async fn clear_channel_sender_keys(&self, channel_id: ChannelId) -> Result<(), ServiceError> {
        self.send(|tx| SignalCommand::ClearChannelSenderKeys { channel_id, response: tx }).await?
    }

    pub async fn mark_channel_for_rotation(&self, channel_id: ChannelId) -> Result<(), ServiceError> {
        self.send(|tx| SignalCommand::MarkChannelForRotation { channel_id, response: tx }).await?
    }

    pub async fn has_session(&self, address: ProtocolAddress) -> Result<bool, ServiceError> {
        self.send(|tx| SignalCommand::HasSession { address, response: tx }).await?
    }

    /// Generate a 60-digit safety number for verifying a peer's identity.
    pub async fn generate_safety_number(&self, local_user_id: String, peer_user_id: String) -> Result<String, ServiceError> {
        self.send(|tx| SignalCommand::GenerateSafetyNumber { local_user_id, peer_user_id, response: tx }).await?
    }

    /// Generate scannable fingerprint bytes for QR code verification.
    pub async fn generate_scannable_fingerprint(&self, local_user_id: String, peer_user_id: String) -> Result<Vec<u8>, ServiceError> {
        self.send(|tx| SignalCommand::GenerateScannableFingerprint { local_user_id, peer_user_id, response: tx }).await?
    }
}

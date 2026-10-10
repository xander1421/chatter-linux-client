//! Signal Actor - Dedicated thread for libsignal-protocol operations.
//!
//! Encapsulates all non-Send/Sync libsignal operations into a single-threaded
//! actor, providing an async handle for the rest of the application.

use libsignal_protocol::{
    CiphertextMessageType, DeviceId, PreKeyBundle, ProtocolAddress, PublicKey, SenderCertificate,
    SessionRecord,
};
use rand::TryRngCore;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use crate::crypto::{
    SealedSenderManager, SessionManager, SignalStore, signal::sender_key::SenderKeyManager,
};
use crate::domain::ids::{ChannelId, UserId};
use crate::error::{CryptoError, ServiceError};
use crate::storage::KeyStore;
use crate::storage::repositories::settings_repo::SettingsRepository;

/// Result is returned only after receive state is durably committed.
#[derive(Debug)]
pub struct DurableReceive {
    pub address: ProtocolAddress,
    pub content: Vec<u8>,
    pub msg_type: CiphertextMessageType,
    pub artifact: Option<crate::storage::receive_artifact::ReceiveArtifact>,
}

/// Typed rejection prevents callers guessing crypto commit safety from text.
#[derive(Debug, thiserror::Error)]
pub enum DurableReceiveError {
    #[error("unratcheted anonymous receive plaintext rejected")]
    PlaintextRejected,
    #[error(transparent)]
    Failure(#[from] ServiceError),
}
impl From<crate::error::StorageError> for DurableReceiveError {
    fn from(error: crate::error::StorageError) -> Self {
        Self::Failure(ServiceError::Storage(error))
    }
}

/// Map an identified-sender decrypt failure.
///
/// `UntrustedIdentity` must survive as its own error — collapsed into
/// `Protocol` it is indistinguishable from a parse failure and the accept path
/// can never be offered. The sender here is server-supplied envelope metadata,
/// so that is the provenance recorded.
fn map_identified_decrypt_err(
    e: libsignal_protocol::SignalProtocolError,
    new_identity_key: Option<Vec<u8>>,
) -> ServiceError {
    if let libsignal_protocol::SignalProtocolError::UntrustedIdentity(ref address) = e {
        return ServiceError::Crypto(crate::crypto::signal::untrusted_identity(
            address,
            new_identity_key,
            crate::error::IdentityChangeSource::ServerClaimed,
        ));
    }
    ServiceError::Crypto(CryptoError::Protocol(e.to_string()))
}

/// Commands sent to the Signal Actor.
pub(crate) enum SignalCommand {
    GetIdentityFingerprint {
        response: oneshot::Sender<Result<String, ServiceError>>,
    },
    GetIdentityPublicKey {
        response: oneshot::Sender<Result<[u8; 33], ServiceError>>,
    },
    /// Get the identity private key (32 bytes X25519 scalar) for the key
    /// backups. HPKE opens use `OpenIdentityHpke` instead.
    GetIdentityPrivateKey {
        response: oneshot::Sender<Result<[u8; 32], ServiceError>>,
    },
    /// Open an HPKE message sealed to our identity key (`crypto::ecies`)
    /// here, so the identity secret never leaves the actor for it.
    OpenIdentityHpke {
        purpose: crate::crypto::ecies::IdentityHpkeUse,
        ciphertext: Vec<u8>,
        response: oneshot::Sender<Result<Vec<u8>, ServiceError>>,
    },
    /// Sign a channel membership change (`crypto::signal::identity::sign_channel_change`):
    /// the only identity signature besides prekeys and the registration
    /// challenge. Takes typed `Actions`, never bytes. Returns
    /// `(actions_bytes, signature)`.
    SignChannelChange {
        actions: crate::proto::chat_v1::channel_change::Actions,
        response: oneshot::Sender<Result<(Vec<u8>, Vec<u8>), ServiceError>>,
    },
    RefreshPrekeys {
        count: u32,
        response: oneshot::Sender<
            Result<Vec<crate::proto::chat_v1::signal_pre_key_bundle::PreKeyRecord>, ServiceError>,
        >,
    },
    /// Generate `count` one-time Kyber prekeys (PQOPK) signed by the IK and
    /// return their proto-encoded form for upload via RefreshPreKeys. PQOPKs
    /// are spec-mandated alongside classical OPKs (PQXDH §3.2) and consumed
    /// 1:1 with them, so the client refills both in lockstep.
    RefreshKyberPrekeys {
        count: u32,
        response: oneshot::Sender<
            Result<
                Vec<crate::proto::chat_v1::signal_pre_key_bundle::KyberPreKeyRecord>,
                ServiceError,
            >,
        >,
    },
    EstablishSession {
        address: ProtocolAddress,
        bundle: PreKeyBundle,
        response: oneshot::Sender<Result<(), ServiceError>>,
    },
    EncryptSealed {
        plaintext: Vec<u8>,
        sender_cert: SenderCertificate,
        recipient_address: ProtocolAddress,
        response: oneshot::Sender<Result<Vec<u8>, ServiceError>>,
    },
    EncryptSealedBoundedBatch {
        plaintext: Vec<u8>,
        sender_cert: SenderCertificate,
        destinations: Vec<ProtocolAddress>,
        permit: crate::services::gateway::submission::SubmissionPermit,
        response: oneshot::Sender<Result<Vec<(ProtocolAddress, Vec<u8>, u32)>, ServiceError>>,
    },
    EncryptIdentified {
        plaintext: Vec<u8>,
        recipient_address: ProtocolAddress,
        response: oneshot::Sender<Result<(Vec<u8>, u8), ServiceError>>,
    },
    /// The `CiphertextMessageType` rides back with the plaintext on purpose:
    /// `Plaintext` means the bytes bypassed the Double Ratchet entirely and
    /// only the DecryptionErrorMessage handler may accept them.
    ///
    /// `validation_time` is the envelope's SERVER timestamp: the sender
    /// certificate is checked against when the server accepted the message,
    /// not when this device got round to reading it (Signal-Desktop
    /// `MessageReceiver.preload.ts` passes `envelope.serverTimestamp`).
    DecryptSealed {
        ciphertext: Vec<u8>,
        trust_root: PublicKey,
        validation_time: libsignal_protocol::Timestamp,
        local_uuid: String,
        local_device_id: DeviceId,
        response: oneshot::Sender<
            Result<(ProtocolAddress, Vec<u8>, CiphertextMessageType), ServiceError>,
        >,
    },
    DecryptReactionDurable {
        ciphertext: Vec<u8>,
        trust_root: PublicKey,
        key: crate::storage::receive_artifact::ReceiveKey,
        response: oneshot::Sender<Result<DurableReceive, DurableReceiveError>>,
    },
    DecryptIdentified {
        ciphertext: Vec<u8>,
        sender_address: ProtocolAddress,
        response: oneshot::Sender<Result<Vec<u8>, ServiceError>>,
    },
    CreateSenderKey {
        channel_id: ChannelId,
        our_address: ProtocolAddress,
        response: oneshot::Sender<Result<Vec<u8>, ServiceError>>,
    },
    ProcessSenderKey {
        sender_address: ProtocolAddress,
        skdm_bytes: Vec<u8>,
        response: oneshot::Sender<Result<(), ServiceError>>,
    },
    EncryptForGroup {
        channel_id: ChannelId,
        our_address: ProtocolAddress,
        plaintext: Vec<u8>,
        response: oneshot::Sender<Result<Vec<u8>, ServiceError>>,
    },
    DecryptFromGroup {
        sender_address: ProtocolAddress,
        ciphertext: Vec<u8>,
        response: oneshot::Sender<Result<Vec<u8>, ServiceError>>,
    },
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
    HasSenderKey {
        channel_id: ChannelId,
        our_address: ProtocolAddress,
        response: oneshot::Sender<Result<bool, ServiceError>>,
    },
    HasSession {
        address: ProtocolAddress,
        response: oneshot::Sender<Result<bool, ServiceError>>,
    },
    /// Delete every Signal session row whose address matches `{peer_user_id}:*`.
    /// Used by the PEER_DEVICES_CHANGED handler so a peer's device change
    /// forces fresh prekey fetch + session rebuild on the next send to them.
    /// Returns the number of session rows deleted.
    InvalidatePeerSessions {
        peer_user_id: String,
        response: oneshot::Sender<Result<u32, ServiceError>>,
    },
    /// Archive (not delete) the session for `(peer_user_id, device_id)` in
    /// response to a peer's DecryptionErrorMessage, so the next send
    /// re-establishes via a fresh PreKeySignalMessage while in-flight
    /// messages from the peer under the old ratchet still decrypt.
    ///
    /// If `dem_ratchet_key` is present it must match the session's CURRENT
    /// sender ratchet key — a stale DEM about an already-rotated session is
    /// ignored (Signal's guard against archiving a healthy new session).
    /// Returns true if the session was archived.
    ArchiveSessionForResend {
        peer_user_id: String,
        device_id: u32,
        dem_ratchet_key: Option<Vec<u8>>,
        response: oneshot::Sender<Result<bool, ServiceError>>,
    },
    /// Wrap a serialized DecryptionErrorMessage in `PlaintextContent` and
    /// seal it for `recipient_address`. Deliberately session-LESS: the
    /// ratchet with the recipient may be exactly what is broken, so the DEM
    /// rides as a `CiphertextMessageType::Plaintext` USMC, which sealed
    /// sender can encrypt with only the recipient's identity key.
    EncryptDecryptionErrorSealed {
        dem_serialized: Vec<u8>,
        sender_cert: SenderCertificate,
        recipient_address: ProtocolAddress,
        response: oneshot::Sender<Result<Vec<u8>, ServiceError>>,
    },
    ClearChannelSenderKeys {
        channel_id: ChannelId,
        response: oneshot::Sender<Result<(), ServiceError>>,
    },
    MarkChannelForRotation {
        channel_id: ChannelId,
        response: oneshot::Sender<Result<(), ServiceError>>,
    },
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
    /// Rotate signed prekey + Kyber prekey (PQXDH pairing).
    /// Returns (signed_prekey_proto, kyber_prekey_proto) for upload.
    RotateSignedPreKey {
        response: oneshot::Sender<
            Result<
                (
                    crate::proto::chat_v1::signal_pre_key_bundle::SignedPreKeyRecord,
                    crate::proto::chat_v1::signal_pre_key_bundle::KyberPreKeyRecord,
                ),
                ServiceError,
            >,
        >,
    },
}

/// Our own `ProtocolAddress`, read from the authenticated session.
///
/// libsignal >= 0.99 takes the LOCAL address alongside the remote one on every
/// encrypt/decrypt/process-bundle call: a session is keyed by both ends, so the
/// caller has to say which identity of ours is involved. Guessing is not an
/// option here — a wrong local address silently addresses a different session —
/// so this fails loudly when the device is not logged in.
fn local_protocol_address(settings: &SettingsRepository) -> Result<ProtocolAddress, ServiceError> {
    let uuid = settings
        .get_string(crate::storage::settings_keys::CURRENT_USER_ID)
        .map_err(ServiceError::Storage)?
        .ok_or_else(|| {
            ServiceError::Crypto(CryptoError::Protocol(
                "no local user id: cannot build the local ProtocolAddress libsignal now requires"
                    .into(),
            ))
        })?;
    // Stored as an INTEGER, not a string: `get_string` on this key fails with
    // "invalid type: integer `1`, expected a string". libsignal >= 0.99 narrowed
    // DeviceId to u8, so the value is range-checked on the way in.
    let device_id: u32 = settings
        .get(crate::storage::settings_keys::CURRENT_DEVICE_ID)
        .map_err(ServiceError::Storage)?
        .ok_or_else(|| ServiceError::Crypto(CryptoError::Protocol("no local device id".into())))?;
    let device_id = u8::try_from(device_id).map_err(|_| {
        ServiceError::Crypto(CryptoError::Protocol(format!(
            "local device id {device_id} does not fit libsignal's u8 DeviceId"
        )))
    })?;
    let device_id = DeviceId::new(device_id)
        .map_err(|e| ServiceError::Crypto(CryptoError::Protocol(e.to_string())))?;
    Ok(ProtocolAddress::new(uuid, device_id))
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
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
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

                        SignalCommand::SignChannelChange { actions, response } => {
                            use libsignal_protocol::IdentityKeyStore;
                            let res = store.get_identity_key_pair().await
                                .map_err(|e| ServiceError::Crypto(CryptoError::Protocol(e.to_string())))
                                .and_then(|identity| {
                                    crate::crypto::signal::identity::sign_channel_change(&identity, &actions)
                                        .map_err(ServiceError::Crypto)
                                });
                            let _ = response.send(res);
                        }

                        SignalCommand::OpenIdentityHpke { purpose, ciphertext, response } => {
                            use libsignal_protocol::IdentityKeyStore;
                            let res = store.get_identity_key_pair().await
                                .map_err(|e| ServiceError::Crypto(CryptoError::Protocol(e.to_string())))
                                .and_then(|identity| {
                                    crate::crypto::ecies::open_with_identity(&identity, purpose, &ciphertext)
                                        .map_err(ServiceError::Crypto)
                                });
                            let _ = response.send(res);
                        }

                        SignalCommand::RefreshPrekeys { count, response } => {
                            use crate::crypto::signal::PreKeyManager;
                            let prekey_manager = PreKeyManager::new(store.key_store());
                            let mut rng = rand::rngs::OsRng.unwrap_err();

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

                        SignalCommand::RefreshKyberPrekeys { count, response } => {
                            use libsignal_protocol::{IdentityKeyStore, GenericSignedPreKey};
                            use crate::crypto::signal::PreKeyManager;
                            let prekey_manager = PreKeyManager::new(store.key_store());

                            let res = (|| {
                                let identity = futures::executor::block_on(store.get_identity_key_pair())
                                    .map_err(|e| ServiceError::Crypto(CryptoError::Protocol(e.to_string())))?;

                                let mut out = Vec::with_capacity(count as usize);
                                for _ in 0..count {
                                    let id = prekey_manager.get_next_kyber_prekey_id().map_err(ServiceError::Crypto)?;
                                    let kpk = prekey_manager.generate_kyber_prekey(id, &identity)
                                        .map_err(ServiceError::Crypto)?;

                                    let proto = crate::proto::chat_v1::signal_pre_key_bundle::KyberPreKeyRecord {
                                        id: u32::from(kpk.id().map_err(|e| ServiceError::Crypto(CryptoError::Protocol(e.to_string())))?),
                                        public_key: kpk.public_key().map_err(|e| ServiceError::Crypto(CryptoError::Protocol(e.to_string())))?.serialize().to_vec(),
                                        signature: kpk.signature().map_err(|e| ServiceError::Crypto(CryptoError::Protocol(e.to_string())))?.to_vec(),
                                        // Chatter-owned wire field: nanoseconds. libsignal stores ms,
                                        // so multiply when emitting on the wire.
                                        timestamp: kpk.timestamp().map_err(|e| ServiceError::Crypto(CryptoError::Protocol(e.to_string())))?.epoch_millis() * 1_000_000,
                                    };
                                    out.push(proto);
                                }
                                Ok(out)
                            })();
                            let _ = response.send(res);
                        }

                        SignalCommand::EstablishSession { address, bundle, response } => {
                            let local_address = match local_protocol_address(&settings_repo) {
                                Ok(a) => a,
                                Err(e) => { let _ = response.send(Err(e)); continue; }
                            };
                            let mut rng = rand::rngs::OsRng.unwrap_err();
                            let mut store2 = store.clone();
                            let res = SessionManager::establish_session(&address, &local_address, &mut store, &mut store2, &bundle, &mut rng)
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

                        SignalCommand::EncryptSealedBoundedBatch { plaintext, sender_cert, destinations, permit, response } => {
                            let res = SealedSenderManager::encrypt_bounded_batch(&plaintext, &sender_cert, &destinations, &store, &permit).await;
                            let _ = response.send(res);
                        }

                        SignalCommand::EncryptIdentified { plaintext, recipient_address, response } => {
                            let local_address = match local_protocol_address(&settings_repo) {
                                Ok(a) => a,
                                Err(e) => { let _ = response.send(Err(e)); continue; }
                            };
                            let mut rng = rand::rngs::OsRng.unwrap_err();
                            let mut store2 = store.clone();
                            let res = match libsignal_protocol::message_encrypt(&plaintext, &recipient_address, &local_address, &mut store, &mut store2, std::time::SystemTime::now(), &mut rng).await {
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

                        SignalCommand::DecryptSealed { ciphertext, trust_root, validation_time, local_uuid, local_device_id, response } => {
                            let mut store2 = store.clone();
                            let mut store3 = store.clone();
                            let mut store4 = store.clone();
                            let store6 = store.clone();
                            let res = match SealedSenderManager::decrypt(
                                &ciphertext, &trust_root, validation_time,
                                None, local_uuid, local_device_id,
                                &mut store, &mut store2, &mut store3, &store6, &mut store4,
                            ).await {
                                Ok((result, msg_type)) => Ok((ProtocolAddress::new(result.sender_uuid, result.device_id), result.message, msg_type)),
                                // Preserve the STRUCTURED CryptoError (notably
                                // SessionDesync, which carries the failed inner
                                // ciphertext for the DecryptionErrorMessage
                                // self-heal). Stringifying here would destroy it.
                                Err(e) => Err(ServiceError::Crypto(e)),
                            };
                            let _ = response.send(res);
                        }

                        SignalCommand::DecryptReactionDurable {ciphertext,trust_root,key,response} => {
                            let res=crate::crypto::signal::store::receive::decrypt_durable(store.clone(),&ciphertext,&trust_root,key).await;
                            let _=response.send(res);
                        }

                        SignalCommand::DecryptIdentified { ciphertext, sender_address, response } => {
                            let local_address = match local_protocol_address(&settings_repo) {
                                Ok(a) => a,
                                Err(e) => { let _ = response.send(Err(e)); continue; }
                            };
                            let mut rng = rand::rngs::OsRng.unwrap_err();
                            let mut store2 = store.clone();
                            let mut store3 = store.clone();
                            let mut store4 = store.clone();
                            let store6 = store.clone();

                            let res = if let Ok(prekey_msg) = libsignal_protocol::PreKeySignalMessage::try_from(ciphertext.as_slice()) {
                                let new_key = prekey_msg.identity_key().serialize().to_vec();
                                libsignal_protocol::message_decrypt_prekey(&prekey_msg, &sender_address, &local_address, &mut store, &mut store2, &mut store3, &store6, &mut store4, &mut rng).await
                                    .map_err(|e| map_identified_decrypt_err(e, Some(new_key)))
                            } else if let Ok(signal_msg) = libsignal_protocol::SignalMessage::try_from(ciphertext.as_slice()) {
                                libsignal_protocol::message_decrypt_signal(&signal_msg, &sender_address, &local_address, &mut store, &mut store2, &mut rng).await
                                    .map_err(|e| map_identified_decrypt_err(e, None))
                            } else {
                                Err(ServiceError::Crypto(CryptoError::Protocol("Unknown ciphertext format".into())))
                            };
                            let _ = response.send(res);
                        }

                        SignalCommand::CreateSenderKey { channel_id, our_address, response } => {
                            let distribution_id = Uuid::parse_str(&channel_id.to_string()).unwrap();
                            let res = SenderKeyManager::create_distribution_message(&our_address, distribution_id, &mut store).await
                                .map(|skdm| skdm.serialized().to_vec())
                                .map_err(ServiceError::Crypto);
                            let _ = response.send(res);
                        }

                        SignalCommand::ProcessSenderKey { sender_address, skdm_bytes, response } => {
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

                        SignalCommand::DecryptFromGroup { sender_address, ciphertext, response } => {
                            let res = SenderKeyManager::decrypt(&ciphertext, &sender_address, &mut store).await
                                .map_err(ServiceError::Crypto);
                            let _ = response.send(res);
                        }

                        SignalCommand::CompleteCallKeyExchange { our_ephemeral_private, peer_ephemeral_public, peer_id, call_id, we_are_caller, response } => {
                            use libsignal_protocol::IdentityKeyStore;
                            use crate::crypto::{EphemeralCallKey, derive_call_key_ephemeral};

                            // ECDH: compute shared secret from ephemeral keys
                            let ephemeral = EphemeralCallKey::from_private_bytes(our_ephemeral_private);
                            let ephemeral_shared = match ephemeral.diffie_hellman(&peer_ephemeral_public) {
                                Ok(s) => s,
                                Err(e) => { let _ = response.send(Err(ServiceError::Crypto(e))); continue; }
                            };

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

                        SignalCommand::ArchiveSessionForResend { peer_user_id, device_id, dem_ratchet_key, response } => {
                            // Address layout matches load_session format:
                            // "{user_id}:{device_id}".
                            let session_key = format!("{}:{}", peer_user_id, device_id);
                            let res = (|| -> Result<bool, ServiceError> {
                                let Some(bytes) = store.key_store()
                                    .load_session(&session_key)
                                    .map_err(ServiceError::Storage)?
                                else {
                                    return Ok(false);
                                };
                                let mut record = SessionRecord::deserialize(&bytes)
                                    .map_err(|e| ServiceError::Crypto(CryptoError::Protocol(e.to_string())))?;

                                // Stale-DEM guard: only archive if the DEM's ratchet
                                // key names OUR current sender ratchet with this
                                // device. A DEM about an old message must not nuke a
                                // session that already rotated past the problem.
                                if let Some(rk_bytes) = dem_ratchet_key {
                                    let rk = PublicKey::deserialize(&rk_bytes)
                                        .map_err(|e| ServiceError::Crypto(CryptoError::Protocol(e.to_string())))?;
                                    let matches = record.current_ratchet_key_matches(&rk)
                                        .map_err(|e| ServiceError::Crypto(CryptoError::Protocol(e.to_string())))?;
                                    if !matches {
                                        tracing::info!(
                                            peer_user_id = %peer_user_id,
                                            device_id,
                                            "DEM ratchet key does not match current session — stale report, not archiving"
                                        );
                                        return Ok(false);
                                    }
                                }

                                record.archive_current_state()
                                    .map_err(|e| ServiceError::Crypto(CryptoError::Protocol(e.to_string())))?;
                                let serialized = record.serialize()
                                    .map_err(|e| ServiceError::Crypto(CryptoError::Protocol(e.to_string())))?;
                                store.key_store()
                                    .store_session(&session_key, &serialized)
                                    .map_err(ServiceError::Storage)?;
                                tracing::info!(
                                    peer_user_id = %peer_user_id,
                                    device_id,
                                    "Archived Signal session after peer's DecryptionErrorMessage — next send re-establishes via PreKeySignalMessage"
                                );
                                Ok(true)
                            })();
                            let _ = response.send(res);
                        }

                        SignalCommand::EncryptDecryptionErrorSealed { dem_serialized, sender_cert, recipient_address, response } => {
                            use libsignal_protocol::{ContentHint, DecryptionErrorMessage, PlaintextContent, UnidentifiedSenderMessageContent};
                            let res = async {
                                let dem = DecryptionErrorMessage::try_from(dem_serialized.as_slice())
                                    .map_err(|e| ServiceError::Crypto(CryptoError::Protocol(e.to_string())))?;
                                let plaintext_content = PlaintextContent::from(dem);
                                // Implicit hint: a DEM is a control ride-along — the
                                // sender has nothing to "resend" if it is lost.
                                let usmc = UnidentifiedSenderMessageContent::new(
                                    CiphertextMessageType::Plaintext,
                                    sender_cert,
                                    plaintext_content.serialized().to_vec(),
                                    ContentHint::Implicit,
                                    None,
                                )
                                .map_err(|e| ServiceError::Crypto(CryptoError::Protocol(e.to_string())))?;
                                SealedSenderManager::encrypt_from_content(&usmc, &recipient_address, &store)
                                    .await
                                    .map_err(ServiceError::Crypto)
                            }.await;
                            let _ = response.send(res);
                        }

                        SignalCommand::InvalidatePeerSessions { peer_user_id, response } => {
                            // ARCHIVE (not delete) every session whose address is
                            // "peer_user_id:%". The next send to this peer triggers
                            // ensure_session, which fetches the peer's (now updated)
                            // prekey bundle and establishes a fresh session per
                            // current device — while the archived states inside each
                            // SessionRecord keep decrypting messages already in
                            // transit under the old ratchet. This used to be a plain
                            // DELETE, which silently lost those in-flight messages on
                            // every peer device-roster change (OPEN-ITEMS §12); the
                            // identity-change path (crypto/signal/store.rs) has
                            // archived this way all along, and libsignal keeps the
                            // previous states inside the record, so no schema change
                            // is involved. For groups, the caller is responsible for
                            // also marking affected channels for sender-key rotation.
                            let res = (|| -> Result<u32, ServiceError> {
                                let mut archived: u32 = 0;
                                for (addr, record_bytes) in store.key_store()
                                    .list_sessions_for_user(&peer_user_id)
                                    .map_err(ServiceError::Storage)?
                                {
                                    let mut record = SessionRecord::deserialize(&record_bytes)
                                        .map_err(|e| ServiceError::Crypto(CryptoError::Protocol(e.to_string())))?;
                                    record.archive_current_state()
                                        .map_err(|e| ServiceError::Crypto(CryptoError::Protocol(e.to_string())))?;
                                    let serialized = record.serialize()
                                        .map_err(|e| ServiceError::Crypto(CryptoError::Protocol(e.to_string())))?;
                                    store.key_store()
                                        .store_session(&addr, &serialized)
                                        .map_err(ServiceError::Storage)?;
                                    archived += 1;
                                }
                                Ok(archived)
                            })();
                            if let Ok(n) = &res {
                                tracing::info!(
                                    peer_user_id = %peer_user_id,
                                    archived = *n,
                                    "Invalidated peer Signal sessions (archived, not deleted)"
                                );
                            }
                            let _ = response.send(res);
                        }

                        SignalCommand::HasSession { address, response } => {
                            let session_key = format!("{}:{}", address.name(), address.device_id());
                            let res = store.key_store().load_session(&session_key)
                                .map_err(ServiceError::Storage)
                                .map(|s| {
                                    match s {
                                        Some(bytes) => {
                                            // Validate session is structurally intact AND
                                            // usable for SENDING. An archived session (no
                                            // current state — e.g. after a peer's
                                            // DecryptionErrorMessage forced a reset)
                                            // deserializes fine but cannot encrypt; it must
                                            // read as "no session" here so ensure_session
                                            // fetches a fresh prekey bundle and the next
                                            // send goes out as a PreKeySignalMessage.
                                            match SessionRecord::deserialize(&bytes) {
                                                Ok(record) => record
                                                    .has_usable_sender_chain(
                                                        std::time::SystemTime::now(),
                                                        libsignal_protocol::SessionUsabilityRequirements::empty(),
                                                    )
                                                    .unwrap_or(false),
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

                        SignalCommand::RotateSignedPreKey { response } => {
                            use libsignal_protocol::{IdentityKeyStore, GenericSignedPreKey};
                            use crate::crypto::signal::PreKeyManager;
                            let prekey_manager = PreKeyManager::new(store.key_store());
                            let mut rng = rand::rngs::OsRng.unwrap_err();

                            let res = (|| {
                                let identity = futures::executor::block_on(store.get_identity_key_pair())
                                    .map_err(|e| ServiceError::Crypto(CryptoError::Protocol(e.to_string())))?;

                                // Generate new signed prekey
                                let spk_id = prekey_manager.get_next_signed_prekey_id().map_err(ServiceError::Crypto)?;
                                let spk = prekey_manager.generate_signed_prekey(&mut rng, spk_id, &identity)
                                    .map_err(ServiceError::Crypto)?;

                                let spk_proto = crate::proto::chat_v1::signal_pre_key_bundle::SignedPreKeyRecord {
                                    id: u32::from(spk.id().map_err(|e| ServiceError::Crypto(CryptoError::Protocol(e.to_string())))?),
                                    public_key: spk.public_key().map_err(|e| ServiceError::Crypto(CryptoError::Protocol(e.to_string())))?.serialize().to_vec(),
                                    signature: spk.signature().map_err(|e| ServiceError::Crypto(CryptoError::Protocol(e.to_string())))?.to_vec(),
                                    // Chatter-owned wire field: nanoseconds. libsignal stores ms,
                                    // so multiply when emitting on the wire.
                                    timestamp: spk.timestamp().map_err(|e| ServiceError::Crypto(CryptoError::Protocol(e.to_string())))?.epoch_millis() * 1_000_000,
                                };

                                // Generate new Kyber prekey (PQXDH pairing — must rotate together)
                                let kpk_id = prekey_manager.get_next_kyber_prekey_id().map_err(ServiceError::Crypto)?;
                                let kpk = prekey_manager.generate_kyber_prekey(kpk_id, &identity)
                                    .map_err(ServiceError::Crypto)?;

                                let kpk_proto = crate::proto::chat_v1::signal_pre_key_bundle::KyberPreKeyRecord {
                                    id: u32::from(kpk.id().map_err(|e| ServiceError::Crypto(CryptoError::Protocol(e.to_string())))?),
                                    public_key: kpk.public_key().map_err(|e| ServiceError::Crypto(CryptoError::Protocol(e.to_string())))?.serialize().to_vec(),
                                    signature: kpk.signature().map_err(|e| ServiceError::Crypto(CryptoError::Protocol(e.to_string())))?.to_vec(),
                                    // Chatter-owned wire field: nanoseconds. libsignal stores ms,
                                    // so multiply when emitting on the wire.
                                    timestamp: kpk.timestamp().map_err(|e| ServiceError::Crypto(CryptoError::Protocol(e.to_string())))?.epoch_millis() * 1_000_000,
                                };

                                Ok((spk_proto, kpk_proto))
                            })();
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

    async fn send<T>(
        &self,
        cmd_fn: impl FnOnce(oneshot::Sender<T>) -> SignalCommand,
    ) -> Result<T, ServiceError> {
        let (tx, rx) = oneshot::channel();
        self.sender
            .send(cmd_fn(tx))
            .await
            .map_err(|_| ServiceError::Internal("Actor dropped".into()))?;
        rx.await
            .map_err(|_| ServiceError::Internal("Response dropped".into()))
    }

    pub async fn get_identity_fingerprint(&self) -> Result<String, ServiceError> {
        self.send(|tx| SignalCommand::GetIdentityFingerprint { response: tx })
            .await?
    }

    pub async fn get_identity_public_key(&self) -> Result<[u8; 33], ServiceError> {
        self.send(|tx| SignalCommand::GetIdentityPublicKey { response: tx })
            .await?
    }

    /// Get the identity private key (32 bytes X25519 scalar) for the key
    /// backups. HPKE opens use [`Self::open_identity_hpke`] instead.
    pub async fn get_identity_private_key(&self) -> Result<[u8; 32], ServiceError> {
        self.send(|tx| SignalCommand::GetIdentityPrivateKey { response: tx })
            .await?
    }

    /// Sign a channel membership change with the identity key, inside the
    /// actor. Returns `(actions_bytes, signature)`.
    pub async fn sign_channel_change(
        &self,
        actions: crate::proto::chat_v1::channel_change::Actions,
    ) -> Result<(Vec<u8>, Vec<u8>), ServiceError> {
        self.send(|tx| SignalCommand::SignChannelChange {
            actions,
            response: tx,
        })
        .await?
    }

    /// Open an HPKE message sealed to our identity key, inside the actor.
    pub async fn open_identity_hpke(
        &self,
        purpose: crate::crypto::ecies::IdentityHpkeUse,
        ciphertext: Vec<u8>,
    ) -> Result<Vec<u8>, ServiceError> {
        self.send(|tx| SignalCommand::OpenIdentityHpke {
            purpose,
            ciphertext,
            response: tx,
        })
        .await?
    }

    pub async fn refresh_prekeys(
        &self,
        count: u32,
    ) -> Result<Vec<crate::proto::chat_v1::signal_pre_key_bundle::PreKeyRecord>, ServiceError> {
        self.send(|tx| SignalCommand::RefreshPrekeys {
            count,
            response: tx,
        })
        .await?
    }

    /// Generate a batch of one-time Kyber prekeys (PQOPK) for upload.
    pub async fn refresh_kyber_prekeys(
        &self,
        count: u32,
    ) -> Result<Vec<crate::proto::chat_v1::signal_pre_key_bundle::KyberPreKeyRecord>, ServiceError>
    {
        self.send(|tx| SignalCommand::RefreshKyberPrekeys {
            count,
            response: tx,
        })
        .await?
    }

    pub async fn establish_session(
        &self,
        address: ProtocolAddress,
        bundle: libsignal_protocol::PreKeyBundle,
    ) -> Result<(), ServiceError> {
        self.send(|tx| SignalCommand::EstablishSession {
            address,
            bundle,
            response: tx,
        })
        .await?
    }

    pub async fn encrypt_sealed(
        &self,
        plaintext: Vec<u8>,
        sender_cert: SenderCertificate,
        recipient_address: ProtocolAddress,
    ) -> Result<Vec<u8>, ServiceError> {
        self.send(|tx| SignalCommand::EncryptSealed {
            plaintext,
            sender_cert,
            recipient_address,
            response: tx,
        })
        .await?
    }

    pub(crate) async fn encrypt_sealed_bounded_batch(
        &self,
        plaintext: Vec<u8>,
        sender_cert: SenderCertificate,
        destinations: Vec<ProtocolAddress>,
    ) -> Result<Vec<(ProtocolAddress, Vec<u8>, u32)>, ServiceError> {
        let permit = crate::services::gateway::submission::SubmissionPermit::capture();
        self.send(|tx| SignalCommand::EncryptSealedBoundedBatch {
            plaintext,
            sender_cert,
            destinations,
            permit,
            response: tx,
        })
        .await?
    }

    pub async fn encrypt_identified(
        &self,
        plaintext: Vec<u8>,
        recipient_address: ProtocolAddress,
    ) -> Result<(Vec<u8>, u8), ServiceError> {
        self.send(|tx| SignalCommand::EncryptIdentified {
            plaintext,
            recipient_address,
            response: tx,
        })
        .await?
    }

    pub async fn decrypt_sealed(
        &self,
        ciphertext: Vec<u8>,
        trust_root: libsignal_protocol::PublicKey,
        validation_time: libsignal_protocol::Timestamp,
        local_uuid: String,
        local_device_id: DeviceId,
    ) -> Result<(ProtocolAddress, Vec<u8>, CiphertextMessageType), ServiceError> {
        self.send(|tx| SignalCommand::DecryptSealed {
            ciphertext,
            trust_root,
            validation_time,
            local_uuid,
            local_device_id,
            response: tx,
        })
        .await?
    }

    pub async fn decrypt_reaction_durable(
        &self,
        ciphertext: Vec<u8>,
        trust_root: PublicKey,
        key: crate::storage::receive_artifact::ReceiveKey,
    ) -> Result<DurableReceive, DurableReceiveError> {
        self.send(|response| SignalCommand::DecryptReactionDurable {
            ciphertext,
            trust_root,
            key,
            response,
        })
        .await?
    }

    pub async fn decrypt_identified(
        &self,
        ciphertext: Vec<u8>,
        sender_address: ProtocolAddress,
    ) -> Result<Vec<u8>, ServiceError> {
        self.send(|tx| SignalCommand::DecryptIdentified {
            ciphertext,
            sender_address,
            response: tx,
        })
        .await?
    }

    pub async fn create_sender_key(
        &self,
        channel_id: ChannelId,
        our_address: ProtocolAddress,
    ) -> Result<Vec<u8>, ServiceError> {
        self.send(|tx| SignalCommand::CreateSenderKey {
            channel_id,
            our_address,
            response: tx,
        })
        .await?
    }

    pub async fn process_sender_key(
        &self,
        sender_address: ProtocolAddress,
        skdm_bytes: Vec<u8>,
    ) -> Result<(), ServiceError> {
        self.send(|tx| SignalCommand::ProcessSenderKey {
            sender_address,
            skdm_bytes,
            response: tx,
        })
        .await?
    }

    pub async fn encrypt_for_group(
        &self,
        channel_id: ChannelId,
        our_address: ProtocolAddress,
        plaintext: Vec<u8>,
    ) -> Result<Vec<u8>, ServiceError> {
        self.send(|tx| SignalCommand::EncryptForGroup {
            channel_id,
            our_address,
            plaintext,
            response: tx,
        })
        .await?
    }

    pub async fn decrypt_from_group(
        &self,
        sender_address: ProtocolAddress,
        ciphertext: Vec<u8>,
    ) -> Result<Vec<u8>, ServiceError> {
        self.send(|tx| SignalCommand::DecryptFromGroup {
            sender_address,
            ciphertext,
            response: tx,
        })
        .await?
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
        })
        .await?
    }

    pub async fn has_sender_key(
        &self,
        channel_id: ChannelId,
        our_address: ProtocolAddress,
    ) -> Result<bool, ServiceError> {
        self.send(|tx| SignalCommand::HasSenderKey {
            channel_id,
            our_address,
            response: tx,
        })
        .await?
    }

    pub async fn clear_channel_sender_keys(
        &self,
        channel_id: ChannelId,
    ) -> Result<(), ServiceError> {
        self.send(|tx| SignalCommand::ClearChannelSenderKeys {
            channel_id,
            response: tx,
        })
        .await?
    }

    pub async fn mark_channel_for_rotation(
        &self,
        channel_id: ChannelId,
    ) -> Result<(), ServiceError> {
        self.send(|tx| SignalCommand::MarkChannelForRotation {
            channel_id,
            response: tx,
        })
        .await?
    }

    pub async fn has_session(&self, address: ProtocolAddress) -> Result<bool, ServiceError> {
        self.send(|tx| SignalCommand::HasSession {
            address,
            response: tx,
        })
        .await?
    }

    /// Delete every Signal session for `peer_user_id` across all devices.
    /// Returns the number of sessions dropped. Caller must separately mark
    /// affected group channels for sender-key rotation (see
    /// `mark_channel_for_rotation`) when the peer is a group member.
    pub async fn invalidate_peer_sessions(
        &self,
        peer_user_id: String,
    ) -> Result<u32, ServiceError> {
        self.send(|tx| SignalCommand::InvalidatePeerSessions {
            peer_user_id,
            response: tx,
        })
        .await?
    }

    /// Archive the session for `(peer_user_id, device_id)` after receiving the
    /// peer's DecryptionErrorMessage. See [`SignalCommand::ArchiveSessionForResend`].
    pub async fn archive_session_for_resend(
        &self,
        peer_user_id: String,
        device_id: u32,
        dem_ratchet_key: Option<Vec<u8>>,
    ) -> Result<bool, ServiceError> {
        self.send(|tx| SignalCommand::ArchiveSessionForResend {
            peer_user_id,
            device_id,
            dem_ratchet_key,
            response: tx,
        })
        .await?
    }

    /// Seal a serialized DecryptionErrorMessage (as `PlaintextContent`) for a
    /// recipient device without touching the (possibly broken) session.
    pub async fn encrypt_decryption_error_sealed(
        &self,
        dem_serialized: Vec<u8>,
        sender_cert: SenderCertificate,
        recipient_address: ProtocolAddress,
    ) -> Result<Vec<u8>, ServiceError> {
        self.send(|tx| SignalCommand::EncryptDecryptionErrorSealed {
            dem_serialized,
            sender_cert,
            recipient_address,
            response: tx,
        })
        .await?
    }

    /// Rotate signed prekey + Kyber prekey (PQXDH pairing).
    /// Returns proto types ready for upload via RefreshPreKeysRequest.
    pub async fn rotate_signed_prekey(
        &self,
    ) -> Result<
        (
            crate::proto::chat_v1::signal_pre_key_bundle::SignedPreKeyRecord,
            crate::proto::chat_v1::signal_pre_key_bundle::KyberPreKeyRecord,
        ),
        ServiceError,
    > {
        self.send(|tx| SignalCommand::RotateSignedPreKey { response: tx })
            .await?
    }

    /// Generate a 60-digit safety number for verifying a peer's identity.
    pub async fn generate_safety_number(
        &self,
        local_user_id: String,
        peer_user_id: String,
    ) -> Result<String, ServiceError> {
        self.send(|tx| SignalCommand::GenerateSafetyNumber {
            local_user_id,
            peer_user_id,
            response: tx,
        })
        .await?
    }

    /// Generate scannable fingerprint bytes for QR code verification.
    pub async fn generate_scannable_fingerprint(
        &self,
        local_user_id: String,
        peer_user_id: String,
    ) -> Result<Vec<u8>, ServiceError> {
        self.send(|tx| SignalCommand::GenerateScannableFingerprint {
            local_user_id,
            peer_user_id,
            response: tx,
        })
        .await?
    }
}

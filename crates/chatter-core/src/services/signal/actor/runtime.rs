use super::*;

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

                        SignalCommand::EncryptSealedBoundedBatch { plaintext, sender_cert, destinations, group_id, permit, response } => {
                            let res = SealedSenderManager::encrypt_bounded_batch(&plaintext, &sender_cert, &destinations, group_id.as_deref(), &store, &permit).await;
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
}

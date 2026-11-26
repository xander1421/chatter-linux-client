use super::*;
use sha2_10::{Digest, Sha256};

pub(super) const PAIRING_RECEIPT_MAGIC: &[u8; 8] = b"CHTRPQR1";

pub(super) struct PairingOffer {
    pub token: [u8; 32],
    pub transcript: [u8; 32],
    pub expires_at: u64,
}

pub(super) fn pairing_token(qr: &BindingQr) -> [u8; 32] {
    Sha256::digest(qr.to_text().as_bytes()).into()
}

pub(super) fn pairing_receipt(
    identity: &MeshIdentity,
    qr: &BindingQr,
    transcript: &[u8; 32],
) -> Vec<u8> {
    let token = pairing_token(qr);
    let public = identity.signing_public();
    let preimage = [
        PAIRING_RECEIPT_MAGIC.as_slice(),
        &token,
        transcript,
        &public,
    ]
    .concat();
    let signature = chatter_mesh_noise::sign::sign_bytes(identity, &preimage);
    [
        PAIRING_RECEIPT_MAGIC.as_slice(),
        &token,
        &public,
        &signature,
    ]
    .concat()
}

pub(super) fn store_paired_pin(
    peers: &mut chatter_mesh_link::registry::PeerRegistry,
    storage: &dyn Storage,
    sealer: &dyn Sealer,
    peer: &PeerId,
    noise: &[u8; 32],
    signing: &[u8; 32],
) -> bool {
    let mut candidate = peers.clone();
    if !candidate.pair_noise_identity(peer, noise, signing, now_millis()) {
        return false;
    }
    if let Ok(sealed) = sealer.seal(&candidate.pins_to_bytes())
        && storage
            .save(chatter_mesh_store::storage::StorageKey::PeerPins, &sealed)
            .is_ok()
    {
        *peers = candidate;
        return true;
    }
    false
}

impl MeshService {
    /// The account identity is disclosed only to this selected, live Noise peer.
    /// Presence alone never calls this method.
    pub fn share_identity(
        &self,
        peer: &PeerId,
        account_id: UserId,
        device_id: u32,
    ) -> Result<(), MeshError> {
        use libsignal_protocol::IdentityKeyPair;
        let peer = &self.state.route(peer);
        let identity = self.identity()?;
        let (mesh_static, transcript, sender_nonce, recipient_nonce) = self.state.with(|s| {
            (
                s.sessions.remote_static(peer),
                s.sessions.handshake_hash(peer),
                s.chatter_hello.local_nonce(peer),
                s.chatter_hello.get(peer).map(|h| h.nonce),
            )
        });
        let (Some(_), Some(handshake_hash), Some(sender_nonce), Some(recipient_nonce)) =
            (mesh_static, transcript, sender_nonce, recipient_nonce)
        else {
            return Err(self.needs_session(peer));
        };
        if device_id == 0 {
            return Err(MeshError::Binding("no account device is active".into()));
        }
        let (_, bytes) = self
            .key_store
            .get_local_identity()?
            .ok_or_else(|| MeshError::Binding("no account identity is available".into()))?;
        let bytes = zeroize::Zeroizing::new(bytes);
        let keypair = IdentityKeyPair::try_from(bytes.as_slice())
            .map_err(|_| MeshError::Binding("invalid local account identity".into()))?;
        let now = now_secs();
        let proof = BindingV1 {
            account_id: *account_id.0.as_bytes(),
            device_id,
            signal_identity: keypair
                .identity_key()
                .serialize()
                .as_ref()
                .try_into()
                .map_err(|_| MeshError::Binding("invalid Signal public key length".into()))?,
            mesh_static: identity.static_public(),
            mesh_signing: identity.signing_public(),
            handshake_hash,
            sender_nonce,
            recipient_nonce,
            created_at: now,
            expires_at: now + PROOF_TTL_SECS,
            generation: 1,
            account_signature: [0; 64],
            mesh_signature: [0; 64],
        }
        .sign(&keypair, &identity)
        .ok_or_else(|| MeshError::Binding("could not sign account binding".into()))?;
        self.ble
            .send(BleCommand::BindingProof {
                peer: peer.clone(),
                bytes: proof.encode(),
            })
            .map_err(|_| MeshError::NotRunning)?;
        self.binding_store
            .offer(&account_id.to_string(), &self.state.contact(peer), now)?;
        self.state.with(|s| {
            s.binding_offers.insert(peer.clone());
        });
        Ok(())
    }

    /// QR contains a signed identity attestation and invite nonce. It is useful
    /// only alongside a fresh live Noise proof from the same peer.
    pub fn identity_qr(
        &self,
        peer: &PeerId,
        account_id: UserId,
        device_id: u32,
    ) -> Result<String, MeshError> {
        use libsignal_protocol::IdentityKeyPair;
        let peer = &self.state.route(peer);
        let identity = self.identity()?;
        if device_id == 0 {
            return Err(MeshError::Binding("no account device is active".into()));
        }
        let (_, bytes) = self
            .key_store
            .get_local_identity()?
            .ok_or_else(|| MeshError::Binding("no account identity is available".into()))?;
        let bytes = zeroize::Zeroizing::new(bytes);
        let keypair = IdentityKeyPair::try_from(bytes.as_slice())
            .map_err(|_| MeshError::Binding("invalid local account identity".into()))?;
        let Some((nonce, transcript)) = self.state.with(|s| {
            Some((
                s.chatter_hello.local_nonce(peer)?,
                s.sessions.handshake_hash(peer)?,
            ))
        }) else {
            return Err(self.needs_session(peer));
        };
        let qr = BindingQr {
            account_id: *account_id.0.as_bytes(),
            device_id,
            signal_identity: keypair
                .identity_key()
                .serialize()
                .as_ref()
                .try_into()
                .map_err(|_| MeshError::Binding("invalid Signal public key length".into()))?,
            mesh_static: identity.static_public(),
            mesh_signing: identity.signing_public(),
            nonce,
            expires_at: now_secs() + PROOF_TTL_SECS,
            signature: [0; 64],
        }
        .sign(&keypair, &identity)
        .ok_or_else(|| MeshError::Binding("could not sign QR identity".into()))?;
        self.state.with(|s| {
            s.pairing_offers
                .retain(|_, offer| offer.expires_at > now_secs());
            s.pairing_offers.insert(
                peer.clone(),
                PairingOffer {
                    token: pairing_token(&qr),
                    transcript,
                    expires_at: qr.expires_at,
                },
            );
        });
        Ok(qr.to_text())
    }

    pub fn identity_qr_png(
        &self,
        peer: &PeerId,
        account_id: UserId,
        device_id: u32,
    ) -> Result<Vec<u8>, MeshError> {
        use image::{ImageEncoder, Luma};
        let text = self.identity_qr(peer, account_id, device_id)?;
        let qr = qrcode::QrCode::new(text.as_bytes())
            .map_err(|e| MeshError::Binding(format!("could not encode QR: {e}")))?;
        let image = qr.render::<Luma<u8>>().quiet_zone(true).build();
        let scaled =
            image::imageops::resize(&image, 512, 512, image::imageops::FilterType::Nearest);
        let mut bytes = Vec::new();
        image::codecs::png::PngEncoder::new(&mut bytes)
            .write_image(scaled.as_raw(), 512, 512, image::ExtendedColorType::L8)
            .map_err(|e| MeshError::Binding(format!("could not render QR: {e}")))?;
        Ok(bytes)
    }

    /// Human comparison code for the current Noise transcript and both static keys.
    pub fn comparison_code(&self, peer: &PeerId) -> Option<String> {
        let peer = &self.state.route(peer);
        let (remote, transcript) = self.state.with(|s| {
            (
                s.sessions.remote_static(peer),
                s.sessions.handshake_hash(peer),
            )
        });
        let (Some(remote), Some(transcript)) = (remote, transcript) else {
            return None;
        };
        let own = self.identity().ok()?.static_public();
        let (first, second) = if own <= remote {
            (own, remote)
        } else {
            (remote, own)
        };
        let mut hash = Sha256::new();
        hash.update(b"chatter.mesh.comparison.v1\0");
        hash.update(transcript);
        hash.update(first);
        hash.update(second);
        let digest = hash.finalize();
        let code = u32::from_be_bytes(digest[..4].try_into().ok()?) % 1_000_000;
        Some(format!("{code:06}"))
    }

    pub fn binding_state(&self, owner: UserId, peer: &PeerId) -> Result<PeerBinding, MeshError> {
        let peer = &self.state.contact(peer);
        let route = self.state.route(peer);
        if let Some(key) = self.state.with(|s| s.sessions.remote_static(&route)) {
            if let Some((current, _)) = self.state.rotated_keys(&route) {
                if key != current {
                    return Ok(PeerBinding::KeyChanged);
                }
            } else {
                self.binding_store
                    .observe_live_key(&owner.to_string(), peer, &key)?;
            }
        }
        if let Some((account_id, old_key)) = self
            .binding_store
            .pinned_account_key(&owner.to_string(), peer)?
        {
            let account = uuid::Uuid::from_bytes(account_id).to_string();
            if self
                .key_store
                .get_identity(&account)?
                .is_some_and(|current| current != old_key)
            {
                self.binding_store
                    .mark_key_changed(&owner.to_string(), peer)?;
            }
        }
        let stored = self.binding_store.state(&owner.to_string(), peer)?;
        if stored == PeerBinding::Unverified
            && self.binding_store.has_offer(&owner.to_string(), peer)?
        {
            return Ok(PeerBinding::BindingOffered);
        }
        Ok(stored)
    }

    /// Resolve a Signal target only through the live, persisted verification gate.
    /// Every later ratchet operation repeats this check inside `SignalStore`.
    pub fn verified_signal_target(
        &self,
        owner: UserId,
        peer: &PeerId,
    ) -> Result<(SignalStore, libsignal_protocol::ProtocolAddress), MeshError> {
        use libsignal_protocol::{DeviceId, ProtocolAddress};
        let peer = &self.state.contact(peer);
        let PeerBinding::Verified {
            account_id,
            device_id,
            ..
        } = self.binding_state(owner, peer)?
        else {
            return Err(MeshError::Binding(
                "verify this Chatter contact before Signal mesh messaging".into(),
            ));
        };
        let address = ProtocolAddress::new(
            uuid::Uuid::from_bytes(account_id).to_string(),
            DeviceId::try_from(device_id)
                .map_err(|_| MeshError::Binding("invalid verified device ID".into()))?,
        );
        Ok((
            SignalStore::for_mesh(
                self.key_store.clone(),
                self.binding_store.clone(),
                owner.to_string(),
                peer.clone(),
            ),
            address,
        ))
    }

    pub fn signal_opted_in(&self, owner: UserId, peer: &PeerId) -> Result<bool, MeshError> {
        Ok(self
            .binding_store
            .signal_opted_in(&owner.to_string(), &self.state.contact(peer))?)
    }

    pub fn signal_protection_label(
        &self,
        owner: UserId,
        peer: &PeerId,
    ) -> Result<&'static str, MeshError> {
        if !self.signal_opted_in(owner, peer)? {
            return Ok("Noise (bitchat)");
        }
        if matches!(
            self.binding_state(owner, peer)?,
            PeerBinding::Verified { .. }
        ) {
            Ok("Signal-protected")
        } else {
            Ok("Signal paused · verify identity")
        }
    }

    /// Sessions open lazily; ask the radio to open one and tell the caller
    /// to retry once the encrypted link is up.
    pub(super) fn needs_session(&self, peer: &PeerId) -> MeshError {
        self.open_session(peer);
        MeshError::Binding("connecting securely to this peer, try again in a moment".into())
    }

    /// Start a Noise handshake with `peer` if no session exists, so actions
    /// that need one (binding, invites) are ready by the time they are tapped.
    pub fn open_session(&self, peer: &PeerId) {
        let _ = self.ble.send(BleCommand::OpenSession {
            peer: self.state.route(peer),
        });
    }

    pub fn set_signal_opt_in(
        &self,
        owner: UserId,
        peer: &PeerId,
        enabled: bool,
    ) -> Result<(), MeshError> {
        let _signal_guard = self.signal_lock.lock();
        let peer = &self.state.contact(peer);
        if enabled
            && !matches!(
                self.binding_state(owner, peer)?,
                PeerBinding::Verified { .. }
            )
        {
            return Err(MeshError::Binding(
                "verify this Chatter contact first".into(),
            ));
        }
        self.binding_store
            .set_signal_opt_in(&owner.to_string(), peer, enabled)?;
        // Offer our bundle now; the peer replies with theirs. Otherwise the
        // exchange waits for the next Chatter Hello on a fresh Noise session.
        if enabled && let Ok(Some((_, device))) = self.key_store.mesh_local_account_device() {
            match signal_mesh::bundle_for_peer(
                &self.key_store,
                &self.binding_store,
                &owner.to_string(),
                peer,
                device,
                now_secs(),
            ) {
                Ok(bytes) => {
                    let _ = self.ble.send(BleCommand::SignalBundle {
                        peer: peer.clone(),
                        bytes,
                    });
                }
                Err(error) => {
                    tracing::warn!(target: "mesh", %error, "mesh Signal bundle generation failed")
                }
            }
        }
        Ok(())
    }

    pub fn remove_account_bindings(&self, owner: UserId) -> Result<(), crate::error::StorageError> {
        let _signal_guard = self.signal_lock.lock();
        self.binding_store.wipe_owner(&owner.to_string())?;
        self.key_store
            .delete_mesh_sessions(Some(&owner.to_string()))?;
        self.state.with(|s| {
            s.pending_bindings.clear();
            s.binding_offers.clear();
            s.pairing_offers.clear();
        });
        Ok(())
    }

    /// Finish verification only when the current transcript-bound proof agrees
    /// with a previously pinned Signal contact or the in-person scanned QR.
    pub fn verify_binding(
        &self,
        owner: UserId,
        local_device_id: u32,
        peer: &PeerId,
        scanned_qr: Option<&str>,
    ) -> Result<PeerBinding, MeshError> {
        let identity = self.identity()?;
        if scanned_qr.is_none() && !self.is_paired(peer) {
            return Err(MeshError::Binding(
                "scan this contact's QR code to pair".into(),
            ));
        }
        let peer = &self.state.route(peer);
        let contact = self.state.contact(peer);
        let proof = self
            .state
            .with(|s| s.pending_bindings.get(peer).cloned())
            .ok_or_else(|| MeshError::Binding("wait for this peer's live account proof".into()))?;
        let account = uuid::Uuid::from_bytes(proof.account_id).to_string();
        let qr = scanned_qr
            .map(|text| {
                BindingQr::from_text(text, now_secs())
                    .ok_or_else(|| MeshError::Binding("QR invite is invalid or expired".into()))
            })
            .transpose()?;
        let method = if let Some(qr) = qr.as_ref() {
            if !qr.matches(&proof) {
                return Err(MeshError::Binding(
                    "QR identity does not match the live peer".into(),
                ));
            }
            BindingMethod::InPersonQr
        } else {
            let pinned = self.key_store.get_identity(&account)?.ok_or_else(|| {
                MeshError::Binding("this account has no pinned Signal identity; scan its QR".into())
            })?;
            if pinned != proof.signal_identity {
                return Err(MeshError::Binding("pinned Signal identity changed".into()));
            }
            BindingMethod::PinnedContact
        };
        let live = self.state.with(|s| {
            (
                s.sessions.remote_static(peer),
                s.sessions.handshake_hash(peer),
                s.chatter_hello.local_nonce(peer),
                s.chatter_hello.get(peer).map(|h| h.nonce),
            )
        });
        let (Some(static_key), Some(hash), Some(local), Some(remote)) = live else {
            return Err(MeshError::Binding("the live Noise exchange ended".into()));
        };
        if !proof.verify_live(&static_key, &hash, &local, &remote, now_secs()) {
            return Err(MeshError::Binding(
                "live account proof is no longer valid".into(),
            ));
        }
        let _signal_guard = self.signal_lock.lock();
        if qr.is_some() {
            let paired = self.state.with(|s| {
                s.sessions.handshake_hash(peer) == Some(hash)
                    && store_paired_pin(
                        &mut s.peers,
                        self.storage.as_ref(),
                        self.sealer.as_ref(),
                        peer,
                        &proof.mesh_static,
                        &proof.mesh_signing,
                    )
            });
            if !paired {
                return Err(MeshError::Binding(
                    "could not persist the live mesh pairing".into(),
                ));
            }
        }
        let result = self.binding_store.accept(
            &owner.to_string(),
            &contact,
            &proof,
            method,
            now_secs(),
            qr.is_some(),
        )?;
        self.state.with(|s| {
            s.pending_bindings.remove(peer);
        });
        if matches!(result, PeerBinding::Verified { .. }) {
            if let Some(qr) = qr {
                let _ = self.ble.send(BleCommand::BindingProof {
                    peer: peer.clone(),
                    bytes: pairing_receipt(&identity, &qr, &hash),
                });
            }
            match signal_mesh::bundle_for_peer(
                &self.key_store,
                &self.binding_store,
                &owner.to_string(),
                &contact,
                local_device_id,
                now_secs(),
            ) {
                Ok(bytes) => {
                    let _ = self.ble.send(BleCommand::SignalBundle {
                        peer: peer.clone(),
                        bytes,
                    });
                }
                Err(error) => {
                    tracing::warn!(target: "mesh", %error, "mesh Signal bundle generation failed")
                }
            }
        }
        Ok(result)
    }
}

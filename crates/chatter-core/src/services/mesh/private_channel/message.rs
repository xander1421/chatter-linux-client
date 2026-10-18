use super::sender::route_tag;
use super::*;

impl PrivateChannel {
    pub fn record_message(&mut self, id: &str) -> bool {
        if self.replay.iter().any(|seen| seen == id) {
            return false;
        }
        self.replay.push(id.to_owned());
        if self.replay.len() > 512 {
            self.replay.remove(0);
        }
        true
    }

    pub fn encrypt(&self, sender: &PeerId, text: &str) -> Result<Vec<u8>, MeshError> {
        self.encrypt_with_id(sender, text, uuid::Uuid::new_v4())
    }

    pub fn encrypt_with_id(
        &self,
        sender: &PeerId,
        text: &str,
        message_id: uuid::Uuid,
    ) -> Result<Vec<u8>, MeshError> {
        if self.member_key(sender).is_none() || text.is_empty() || text.len() > MAX_TEXT {
            return Err(failure("invalid private channel sender or text"));
        }
        let sender_key = self
            .sender_keys
            .iter()
            .find(|key| key.peer == sender.as_str())
            .ok_or_else(|| failure("sender key missing"))?;
        let roster_hash = self.chain_hash()?;
        let mut nonce = [0; 24];
        OsRng
            .try_fill_bytes(&mut nonce)
            .map_err(|_| failure("randomness unavailable"))?;
        let mut header = FRAME_MAGIC.to_vec();
        header.extend(route_tag(
            &self.id,
            &roster_hash,
            sender.as_str(),
            sender_key.generation,
        ));
        header.extend(sender_key.generation.to_be_bytes());
        header.extend(roster_hash);
        header.extend(
            sender
                .to_routing_bytes()
                .ok_or_else(|| failure("invalid sender"))?,
        );
        header.extend(message_id.as_bytes());
        header.extend(nonce);
        let mut aad = header.clone();
        aad.extend(sender.as_str().as_bytes());
        let cipher = XChaCha20Poly1305::new_from_slice(&sender_key.key)
            .map_err(|_| failure("invalid sender key"))?;
        let ciphertext = cipher
            .encrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: text.as_bytes(),
                    aad: &aad,
                },
            )
            .map_err(|_| failure("private channel encryption failed"))?;
        header.extend(ciphertext);
        Ok(header)
    }

    pub fn decrypt(&self, sender: &PeerId, bytes: &[u8]) -> Option<(String, String)> {
        if bytes.len() < 112 + 16
            || !bytes.starts_with(FRAME_MAGIC)
            || bytes[64..72] != sender.to_routing_bytes()?
        {
            return None;
        }
        let generation = u64::from_be_bytes(bytes[24..32].try_into().ok()?);
        let roster_hash = &bytes[32..64];
        let current_hash = self.chain_hash().ok()?;
        let key = self
            .sender_keys
            .iter()
            .find(|key| {
                key.peer == sender.as_str()
                    && key.generation == generation
                    && roster_hash == current_hash
            })
            .map(|key| key.key.as_slice())
            .or_else(|| {
                self.prior_sender_keys
                    .iter()
                    .find(|prior| {
                        prior.sender.peer == sender.as_str()
                            && prior.sender.generation == generation
                            && prior.roster_hash == roster_hash
                            && prior.expires_at >= super::super::now_secs()
                    })
                    .map(|prior| prior.sender.key.as_slice())
            })?;
        if bytes[8..24] != route_tag(&self.id, roster_hash, sender.as_str(), generation) {
            return None;
        }
        let nonce = XNonce::from_slice(&bytes[88..112]);
        let mut aad = bytes[..112].to_vec();
        aad.extend(sender.as_str().as_bytes());
        let plaintext = XChaCha20Poly1305::new_from_slice(key)
            .ok()?
            .decrypt(
                nonce,
                Payload {
                    msg: &bytes[112..],
                    aad: &aad,
                },
            )
            .ok()?;
        let text = String::from_utf8(plaintext).ok()?;
        if text.is_empty() {
            return None;
        }
        let id = uuid::Uuid::from_bytes(bytes[72..88].try_into().ok()?).to_string();
        Some((id, text))
    }
}

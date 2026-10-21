use super::protocol::{field, parse_tlv, put_tlv};
use super::*;

pub struct OpenedGroupMessage {
    pub id: String,
    pub content: String,
    pub sender: PeerId,
    pub nickname: String,
    pub timestamp_ms: u64,
}

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

    pub fn envelope_group_id(bytes: &[u8]) -> Option<[u8; 16]> {
        let fields = parse_tlv(bytes)?;
        field(&fields, 1)?.try_into().ok()
    }

    pub fn encrypt(
        &self,
        identity: &MeshIdentity,
        text: &str,
        nickname: &str,
    ) -> Result<(String, Vec<u8>), MeshError> {
        let id = uuid::Uuid::new_v4().to_string();
        let bytes =
            self.encrypt_with_id(identity, text, nickname, &id, super::super::now_millis())?;
        Ok((id, bytes))
    }

    pub fn encrypt_with_id(
        &self,
        identity: &MeshIdentity,
        text: &str,
        nickname: &str,
        message_id: &str,
        timestamp_ms: u64,
    ) -> Result<Vec<u8>, MeshError> {
        if self.stale
            || self.member_key(&identity.peer_id()).is_none()
            || text.is_empty()
            || text.len() > MAX_TEXT
            || message_id.is_empty()
        {
            return Err(failure("invalid private group sender or text"));
        }
        let group_id = self.group_id().ok_or_else(|| failure("invalid group id"))?;
        let mut signed = b"bitchat-group-msg-v1".to_vec();
        signed.extend(group_id);
        signed.extend(self.epoch.to_be_bytes());
        signed.extend(message_id.as_bytes());
        signed.extend(timestamp_ms.to_be_bytes());
        signed.extend(text.as_bytes());
        let signature = sign_bytes(identity, &signed);
        let mut inner = Vec::new();
        put_tlv(1, message_id.as_bytes(), &mut inner)?;
        put_tlv(2, &identity.signing_public(), &mut inner)?;
        put_tlv(3, nickname.as_bytes(), &mut inner)?;
        put_tlv(4, &timestamp_ms.to_be_bytes(), &mut inner)?;
        put_tlv(5, text.as_bytes(), &mut inner)?;
        put_tlv(6, &signature, &mut inner)?;
        let mut nonce = [0u8; 12];
        OsRng
            .try_fill_bytes(&mut nonce)
            .map_err(|_| failure("randomness unavailable"))?;
        let mut aad = group_id.to_vec();
        aad.extend(self.epoch.to_be_bytes());
        let cipher = ChaCha20Poly1305::new_from_slice(&self.key)
            .map_err(|_| failure("invalid group key"))?;
        let ciphertext = cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: &inner,
                    aad: &aad,
                },
            )
            .map_err(|_| failure("group encryption failed"))?;
        let mut out = Vec::new();
        put_tlv(1, &group_id, &mut out)?;
        put_tlv(2, &self.epoch.to_be_bytes(), &mut out)?;
        put_tlv(3, &nonce, &mut out)?;
        put_tlv(4, &ciphertext, &mut out)?;
        Ok(out)
    }

    pub fn decrypt(&self, bytes: &[u8]) -> Option<OpenedGroupMessage> {
        let fields = parse_tlv(bytes)?;
        let group_id = field(&fields, 1)?;
        if group_id != self.group_id()? {
            return None;
        }
        let epoch = u32::from_be_bytes(field(&fields, 2)?.try_into().ok()?);
        if epoch != self.epoch {
            return None;
        }
        let nonce = field(&fields, 3)?;
        if nonce.len() != 12 {
            return None;
        }
        let ciphertext = field(&fields, 4)?;
        if ciphertext.len() <= 16 {
            return None;
        }
        let mut aad = group_id.to_vec();
        aad.extend(epoch.to_be_bytes());
        let cipher = ChaCha20Poly1305::new_from_slice(&self.key).ok()?;
        let plaintext = Zeroizing::new(
            cipher
                .decrypt(
                    Nonce::from_slice(nonce),
                    Payload {
                        msg: ciphertext,
                        aad: &aad,
                    },
                )
                .ok()?,
        );
        let inner = parse_tlv(&plaintext)?;
        let message_id = String::from_utf8(field(&inner, 1)?.to_vec()).ok()?;
        if message_id.is_empty() {
            return None;
        }
        let signing_key: [u8; 32] = field(&inner, 2)?.try_into().ok()?;
        let nickname = String::from_utf8(field(&inner, 3)?.to_vec()).ok()?;
        let timestamp_ms = u64::from_be_bytes(field(&inner, 4)?.try_into().ok()?);
        let content = String::from_utf8(field(&inner, 5)?.to_vec()).ok()?;
        let signature: [u8; 64] = field(&inner, 6)?.try_into().ok()?;
        let member = self.members.iter().find(|m| m.signing_key == signing_key)?;
        if self.departed.iter().any(|n| n.peer == member.peer) {
            return None;
        }
        let mut signed = b"bitchat-group-msg-v1".to_vec();
        signed.extend(group_id);
        signed.extend(epoch.to_be_bytes());
        signed.extend(message_id.as_bytes());
        signed.extend(timestamp_ms.to_be_bytes());
        signed.extend(content.as_bytes());
        if !verify_bytes(&signature, &signed, &signing_key) {
            return None;
        }
        Some(OpenedGroupMessage {
            id: message_id,
            content,
            sender: PeerId::from_str(&member.peer),
            nickname,
            timestamp_ms,
        })
    }
}

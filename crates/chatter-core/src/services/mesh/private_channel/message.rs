use super::protocol::{field, parse_tlv, put_tlv};
use super::*;
use hmac::{Hmac, Mac};
use sha2::Sha256 as RouteHash;
use subtle::ConstantTimeEq;

const ROUTE_WINDOW_MS: u64 = 40_000;
const ROUTE_TAG_LEN: usize = 16;
const NONCE_LEN: usize = 12;
const TAG_FIELD: u8 = 1;
const NONCE_FIELD: u8 = 3;
const CIPHERTEXT_FIELD: u8 = 4;
const EPOCH_FIELD: u8 = 7;
const GROUP_MESSAGE_DOMAIN: &[u8] = b"chatter.mesh.group-message.v2\0";
pub(super) type GroupRoutingKey = Zeroizing<[u8; 32]>;

/// Canonical, borrowed view of the opaque group envelope. There is no clear
/// group ID or key epoch, including in envelopes relayed by nonmembers.
pub(super) struct GroupEnvelope<'a> {
    tag: &'a [u8; ROUTE_TAG_LEN],
    nonce: &'a [u8; NONCE_LEN],
    ciphertext: &'a [u8],
}

impl<'a> GroupEnvelope<'a> {
    pub(super) fn parse(bytes: &'a [u8]) -> Option<Self> {
        fn take<'a>(bytes: &mut &'a [u8], kind: u8) -> Option<&'a [u8]> {
            let (header, rest) = bytes.split_first_chunk::<3>()?;
            if header[0] != kind {
                return None;
            }
            let len = usize::from(u16::from_be_bytes([header[1], header[2]]));
            let (value, tail) = rest.split_at_checked(len)?;
            *bytes = tail;
            Some(value)
        }
        let mut remaining = bytes;
        let tag = take(&mut remaining, TAG_FIELD)?.try_into().ok()?;
        let nonce = take(&mut remaining, NONCE_FIELD)?.try_into().ok()?;
        let ciphertext = take(&mut remaining, CIPHERTEXT_FIELD)?;
        (remaining.is_empty() && ciphertext.len() > 16).then_some(Self {
            tag,
            nonce,
            ciphertext,
        })
    }

    pub(super) fn matches(&self, key: &GroupRoutingKey, timestamp_ms: u64) -> bool {
        bool::from(self.tag.ct_eq(&route_tag(key, timestamp_ms, self.nonce)))
    }
}

fn route_mac(key: &[u8]) -> Hmac<RouteHash> {
    <Hmac<RouteHash> as hmac::KeyInit>::new_from_slice(key).expect("HMAC accepts any key length")
}

fn route_tag(
    key: &GroupRoutingKey,
    timestamp_ms: u64,
    nonce: &[u8; NONCE_LEN],
) -> [u8; ROUTE_TAG_LEN] {
    let mut mac = route_mac(&key[..]);
    mac.update(&(timestamp_ms / ROUTE_WINDOW_MS).to_be_bytes());
    // Even distinct packets in the same window have independent tags. A
    // connection observer cannot use a tag to join them across an RPA change.
    mac.update(nonce);
    let digest = mac.finalize().into_bytes();
    let mut tag = [0; ROUTE_TAG_LEN];
    tag.copy_from_slice(&digest[..ROUTE_TAG_LEN]);
    tag
}

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

    pub(super) fn routing_key(&self) -> Option<GroupRoutingKey> {
        let mut mac = route_mac(&self.key);
        mac.update(b"chatter.mesh.group-routing.v1\0");
        mac.update(&self.group_id()?);
        Some(Zeroizing::new(mac.finalize().into_bytes().into()))
    }

    pub fn has_group_envelope(bytes: &[u8]) -> bool {
        GroupEnvelope::parse(bytes).is_some()
    }

    pub fn matches_group_envelope(&self, bytes: &[u8], timestamp_ms: u64) -> bool {
        GroupEnvelope::parse(bytes)
            .zip(self.routing_key())
            .is_some_and(|(envelope, key)| envelope.matches(&key, timestamp_ms))
    }

    pub fn encrypt(
        &self,
        identity: &MeshIdentity,
        text: &str,
        nickname: &str,
        timestamp_ms: u64,
    ) -> Result<(String, Vec<u8>), MeshError> {
        let id = uuid::Uuid::new_v4().to_string();
        let bytes = self.encrypt_with_id(identity, text, nickname, &id, timestamp_ms)?;
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
        let mut signed = GROUP_MESSAGE_DOMAIN.to_vec();
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
        put_tlv(EPOCH_FIELD, &self.epoch.to_be_bytes(), &mut inner)?;
        let mut nonce = [0u8; NONCE_LEN];
        OsRng
            .try_fill_bytes(&mut nonce)
            .map_err(|_| failure("randomness unavailable"))?;
        let key = self
            .routing_key()
            .ok_or_else(|| failure("invalid group routing key"))?;
        let tag = route_tag(&key, timestamp_ms, &nonce);
        let aad = [GROUP_MESSAGE_DOMAIN, &group_id, &tag].concat();
        let cipher = ChaCha20Poly1305::new_from_slice(&self.key)
            .map_err(|_| failure("invalid group key"))?;
        let ciphertext = cipher
            .encrypt(
                &Nonce::from(nonce),
                Payload {
                    msg: &inner,
                    aad: &aad,
                },
            )
            .map_err(|_| failure("group encryption failed"))?;
        let mut out = Vec::new();
        put_tlv(TAG_FIELD, &tag, &mut out)?;
        put_tlv(NONCE_FIELD, &nonce, &mut out)?;
        put_tlv(CIPHERTEXT_FIELD, &ciphertext, &mut out)?;
        Ok(out)
    }

    pub fn decrypt(&self, bytes: &[u8], sent_at_ms: u64) -> Option<OpenedGroupMessage> {
        let envelope = GroupEnvelope::parse(bytes)?;
        if !envelope.matches(&self.routing_key()?, sent_at_ms) {
            return None;
        }
        let group_id = self.group_id()?;
        let aad = [GROUP_MESSAGE_DOMAIN, &group_id, envelope.tag].concat();
        let cipher = ChaCha20Poly1305::new_from_slice(&self.key).ok()?;
        let plaintext = Zeroizing::new(
            cipher
                .decrypt(
                    &Nonce::from(*envelope.nonce),
                    Payload {
                        msg: envelope.ciphertext,
                        aad: &aad,
                    },
                )
                .ok()?,
        );
        let inner = parse_tlv(&plaintext)?;
        let epoch = u32::from_be_bytes(field(&inner, EPOCH_FIELD)?.try_into().ok()?);
        if epoch != self.epoch {
            return None;
        }
        let message_id = String::from_utf8(field(&inner, 1)?.to_vec()).ok()?;
        if message_id.is_empty() {
            return None;
        }
        let signing_key: [u8; 32] = field(&inner, 2)?.try_into().ok()?;
        let nickname = String::from_utf8(field(&inner, 3)?.to_vec()).ok()?;
        let timestamp_ms = u64::from_be_bytes(field(&inner, 4)?.try_into().ok()?);
        if timestamp_ms != sent_at_ms {
            return None;
        }
        let content = String::from_utf8(field(&inner, 5)?.to_vec()).ok()?;
        let signature: [u8; 64] = field(&inner, 6)?.try_into().ok()?;
        let member = self.members.iter().find(|m| m.signing_key == signing_key)?;
        if self.departed.iter().any(|n| n.peer == member.peer) {
            return None;
        }
        let mut signed = GROUP_MESSAGE_DOMAIN.to_vec();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_group_id_and_epoch_stay_inside_the_authenticated_envelope() {
        let identity = MeshIdentity::generate(&mut crate::crypto::os_rng());
        let channel = PrivateChannel::create("private", &identity, "sender").unwrap();
        let timestamp = ROUTE_WINDOW_MS * 20 + 10;
        let (_, first) = channel
            .encrypt(&identity, "first", "sender", timestamp)
            .unwrap();
        let (_, second) = channel
            .encrypt(&identity, "second", "sender", timestamp)
            .unwrap();
        let envelope = GroupEnvelope::parse(&first).unwrap();
        assert_ne!(envelope.tag, GroupEnvelope::parse(&second).unwrap().tag);
        let group_id = channel.group_id().unwrap();
        assert!(!first.windows(group_id.len()).any(|field| field == group_id));

        let key = channel.routing_key().unwrap();
        assert!(envelope.matches(&key, timestamp));
        assert!(!envelope.matches(&key, timestamp + ROUTE_WINDOW_MS));
        let aad = [GROUP_MESSAGE_DOMAIN, &group_id, envelope.tag].concat();
        let cipher = ChaCha20Poly1305::new_from_slice(&channel.key).unwrap();
        let plaintext = cipher
            .decrypt(
                &Nonce::from(*envelope.nonce),
                Payload {
                    msg: envelope.ciphertext,
                    aad: &aad,
                },
            )
            .unwrap();
        assert_eq!(
            field(&parse_tlv(&plaintext).unwrap(), EPOCH_FIELD),
            Some(channel.epoch.to_be_bytes().as_slice())
        );
        assert_eq!(channel.decrypt(&first, timestamp).unwrap().content, "first");
        assert!(channel.decrypt(&first, timestamp + 1).is_none());

        let unrelated = PrivateChannel::create("same sender", &identity, "sender").unwrap();
        assert!(!unrelated.matches_group_envelope(&first, timestamp));
        assert!(unrelated.decrypt(&first, timestamp).is_none());

        let mut old_shape = Vec::new();
        put_tlv(TAG_FIELD, &group_id, &mut old_shape).unwrap();
        put_tlv(2, &channel.epoch.to_be_bytes(), &mut old_shape).unwrap();
        put_tlv(NONCE_FIELD, envelope.nonce, &mut old_shape).unwrap();
        put_tlv(CIPHERTEXT_FIELD, envelope.ciphertext, &mut old_shape).unwrap();
        assert!(!PrivateChannel::has_group_envelope(&old_shape));
    }
}

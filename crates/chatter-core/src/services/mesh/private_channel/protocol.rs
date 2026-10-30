use super::*;

pub(super) fn put_tlv(kind: u8, value: &[u8], out: &mut Vec<u8>) -> Result<(), MeshError> {
    let len = u16::try_from(value.len()).map_err(|_| failure("group field too large"))?;
    out.push(kind);
    out.extend(len.to_be_bytes());
    out.extend(value);
    Ok(())
}

pub(super) fn parse_tlv(bytes: &[u8]) -> Option<Vec<(u8, Vec<u8>)>> {
    let mut fields = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        if bytes.len() - at < 3 {
            return None;
        }
        let kind = bytes[at];
        let len = u16::from_be_bytes(bytes[at + 1..at + 3].try_into().ok()?) as usize;
        at += 3;
        if bytes.len() - at < len {
            return None;
        }
        fields.push((kind, bytes[at..at + len].to_vec()));
        at += len;
    }
    Some(fields)
}
pub(super) fn field(fields: &[(u8, Vec<u8>)], kind: u8) -> Option<&[u8]> {
    fields
        .iter()
        .rev()
        .find(|(t, _)| *t == kind)
        .map(|(_, v)| v.as_slice())
}
pub(super) fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 15) as usize] as char);
    }
    out
}
pub(super) fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}
pub(super) fn truncate_utf8(s: &str, max: usize) -> Vec<u8> {
    let mut end = s.len().min(max);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s.as_bytes()[..end].to_vec()
}
pub(super) fn decode_roster(bytes: &[u8]) -> Option<Vec<Member>> {
    let count = *bytes.first()? as usize;
    if count == 0 || count > MAX_MEMBERS {
        return None;
    }
    let mut at = 1;
    let mut members = Vec::with_capacity(count);
    for _ in 0..count {
        if bytes.len().checked_sub(at)? < 65 {
            return None;
        }
        let fingerprint = hex_lower(&bytes[at..at + 32]);
        at += 32;
        let signing_key = bytes[at..at + 32].to_vec();
        at += 32;
        let nick_len = bytes[at] as usize;
        at += 1;
        if nick_len > 64 || bytes.len().checked_sub(at)? < nick_len {
            return None;
        }
        let nickname = String::from_utf8(bytes[at..at + nick_len].to_vec()).ok()?;
        at += nick_len;
        members.push(Member {
            peer: fingerprint[..16].to_owned(),
            fingerprint,
            signing_key,
            nickname,
        });
    }
    (at == bytes.len()).then_some(members)
}

impl LeaveNotice {
    pub(super) fn preimage(&self) -> Option<Vec<u8>> {
        serde_json::to_vec(&LeavePreimage {
            domain: "chatter.mesh.private.leave.v1",
            id: &self.id,
            epoch: self.epoch,
            peer: &self.peer,
        })
        .ok()
    }
    pub fn encode(&self) -> Result<String, MeshError> {
        let bytes = serde_json::to_vec(self).map_err(|_| failure("invalid leave notice"))?;
        Ok(format!("{LEAVE_MAGIC}{}", URL_SAFE_NO_PAD.encode(bytes)))
    }
    pub fn decode(text: &str) -> Option<Self> {
        let encoded = text.strip_prefix(LEAVE_MAGIC)?;
        if encoded.len() > 1024 {
            return None;
        }
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(encoded).ok()?).ok()
    }
}

impl EpochProof {
    pub fn channel_id(&self) -> &str {
        &self.id
    }
    pub fn epoch(&self) -> u32 {
        self.epoch
    }
    pub fn matches(&self, state: &PrivateChannel) -> bool {
        self.epoch == state.epoch && self.signature == state.hint_signature
    }
    pub fn encode(&self) -> Option<Vec<u8>> {
        let mut out = b"CHTRPCH4".to_vec();
        out.extend(serde_json::to_vec(self).ok()?);
        (out.len() <= 512).then_some(out)
    }
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        serde_json::from_slice(bytes.strip_prefix(b"CHTRPCH4")?).ok()
    }
    pub fn verified_for(&self, state: &PrivateChannel) -> bool {
        if self.id != state.id || self.epoch == 0 {
            return false;
        }
        let mut content = b"chatter.mesh.group.epoch-hint.v1".to_vec();
        let Some(id) = state.group_id() else {
            return false;
        };
        content.extend(id);
        content.extend(self.epoch.to_be_bytes());
        let (Ok(key), Ok(sig)) = (
            <[u8; 32]>::try_from(state.creator_key.as_slice()),
            <[u8; 64]>::try_from(self.signature.as_slice()),
        ) else {
            return false;
        };
        verify_bytes(&sig, &content, &key)
    }
}

impl Invite {
    pub(super) fn preimage(&self) -> Result<Vec<u8>, MeshError> {
        serde_json::to_vec(&(
            "chatter.mesh.private.invite.v1",
            &self.channel_id,
            &self.name,
            &self.creator,
            &self.creator_key,
            &self.creator_static,
            &self.target,
            &self.nonce,
            self.expires_at,
        ))
        .map_err(|_| failure("invalid invite"))
    }
    /// QR text: fixed-width binary fields, name last. The JSON form wrote each
    /// key byte as a decimal number, turning ~184 bytes into a 975-character
    /// QR that a phone camera could not read off another phone's screen.
    pub fn to_text(&self) -> Option<String> {
        let id = uuid::Uuid::parse_str(&self.channel_id).ok()?;
        let creator = PeerId::from_str(&self.creator).to_routing_bytes()?;
        let target = PeerId::from_str(&self.target).to_routing_bytes()?;
        // Only canonical forms round-trip; anything else would decode to a
        // different preimage and fail its signature.
        if id.to_string() != self.channel_id
            || PeerId::from_bytes(&creator).as_str() != self.creator
            || PeerId::from_bytes(&target).as_str() != self.target
            || self.creator_key.len() != 32
            || self.creator_static.len() != 32
            || self.nonce.len() != 16
            || self.signature.len() != 64
        {
            return None;
        }
        let mut bytes = Vec::with_capacity(INVITE_FIXED_LEN + self.name.len());
        bytes.extend_from_slice(id.as_bytes());
        bytes.extend_from_slice(&creator);
        bytes.extend_from_slice(&target);
        bytes.extend_from_slice(&self.creator_key);
        bytes.extend_from_slice(&self.creator_static);
        bytes.extend_from_slice(&self.nonce);
        bytes.extend_from_slice(&self.expires_at.to_be_bytes());
        bytes.extend_from_slice(&self.signature);
        bytes.extend_from_slice(self.name.as_bytes());
        Some(format!("{INVITE_PREFIX}{}", URL_SAFE_NO_PAD.encode(bytes)))
    }
    pub fn from_text(text: &str, now: u64) -> Option<Self> {
        let data = URL_SAFE_NO_PAD
            .decode(text.trim().strip_prefix(INVITE_PREFIX)?)
            .ok()?;
        if data.len() <= INVITE_FIXED_LEN || data.len() > INVITE_FIXED_LEN + 32 {
            return None;
        }
        let (fixed, name) = data.split_at(INVITE_FIXED_LEN);
        let take = |at: usize, len: usize| fixed[at..at + len].to_vec();
        let invite = Self {
            channel_id: uuid::Uuid::from_slice(&fixed[..16]).ok()?.to_string(),
            creator: PeerId::from_bytes(fixed[16..24].try_into().ok()?)
                .as_str()
                .to_owned(),
            target: PeerId::from_bytes(fixed[24..32].try_into().ok()?)
                .as_str()
                .to_owned(),
            creator_key: take(32, 32),
            creator_static: take(64, 32),
            nonce: take(96, 16),
            expires_at: u64::from_be_bytes(fixed[112..120].try_into().ok()?),
            signature: take(120, 64),
            name: String::from_utf8(name.to_vec()).ok()?,
        };
        invite.validated(now)
    }
    fn validated(self, now: u64) -> Option<Self> {
        let invite = self;
        if invite.creator_key.len() != 32
            || invite.creator_static.len() != 32
            || invite.signature.len() != 64
            || invite.nonce.len() != 16
            || uuid::Uuid::parse_str(&invite.channel_id).is_err()
            || PeerId::from_str(&invite.creator)
                .to_routing_bytes()
                .is_none()
            || PeerId::from_str(&invite.target)
                .to_routing_bytes()
                .is_none()
            || invite.expires_at < now
            || invite.expires_at > now.saturating_add(600)
            || valid_name(&invite.name).is_err()
        {
            return None;
        }
        let key = invite.creator_key.as_slice().try_into().ok()?;
        let sig = invite.signature.as_slice().try_into().ok()?;
        verify_bytes(&sig, &invite.preimage().ok()?, &key).then_some(invite)
    }
    pub fn request_bytes(&self) -> Option<Vec<u8>> {
        let mut bytes = b"CHTRPCR2".to_vec();
        bytes.extend(serde_json::to_vec(self).ok()?);
        Some(bytes)
    }
    pub fn decode_request(bytes: &[u8], now: u64) -> Option<Self> {
        if !bytes.starts_with(b"CHTRPCR2") || bytes.len() > 1032 {
            return None;
        }
        serde_json::from_slice::<Self>(&bytes[8..])
            .ok()?
            .validated(now)
    }
}

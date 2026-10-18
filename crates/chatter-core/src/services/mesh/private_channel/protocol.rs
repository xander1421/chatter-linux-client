use super::*;

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

    pub fn matches(&self, state: &PrivateChannel) -> bool {
        self.epoch == state.epoch && self.signature == state.signature
    }

    pub fn encode(&self) -> Option<Vec<u8>> {
        let mut bytes = b"CHTRPCH3".to_vec();
        bytes.extend(serde_json::to_vec(self).ok()?);
        (bytes.len() <= 4096).then_some(bytes)
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if !bytes.starts_with(b"CHTRPCH3") || bytes.len() > 4096 {
            return None;
        }
        serde_json::from_slice(&bytes[8..]).ok()
    }

    pub fn verified_for(&self, state: &PrivateChannel) -> bool {
        if self.domain != "chatter.mesh.private.roster.v2"
            || self.id != state.id
            || self.creator != state.creator
            || self.creator_static != state.creator_static
            || self.epoch == 0
            || self.previous_hash.len() != 32
            || self.signature.len() != 64
            || self.members.len() > MAX_MEMBERS
            || self.members.is_empty()
            || self.members[0].peer != state.creator
            || self.members[0].signing_key != state.creator_key
        {
            return false;
        }
        let Ok(key) = <[u8; 32]>::try_from(state.creator_key.as_slice()) else {
            return false;
        };
        let Ok(sig) = <[u8; 64]>::try_from(self.signature.as_slice()) else {
            return false;
        };
        let preimage = EpochPreimage {
            domain: "chatter.mesh.private.roster.v2",
            id: &self.id,
            name: &self.name,
            creator: &self.creator,
            creator_static: &self.creator_static,
            epoch: self.epoch,
            members: &self.members,
            previous_hash: &self.previous_hash,
        };
        serde_json::to_vec(&preimage).is_ok_and(|bytes| verify_bytes(&sig, &bytes, &key))
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }
}

impl SenderDistribution {
    pub(super) fn preimage(&self) -> Option<Vec<u8>> {
        serde_json::to_vec(&(
            self.domain.as_str(),
            &self.id,
            &self.roster_hash,
            &self.sender,
            self.generation,
            &self.key,
            &self.removal_hash,
        ))
        .ok()
    }
    pub fn encode(&self) -> Option<Zeroizing<Vec<u8>>> {
        let mut out = Zeroizing::new(DISTRIBUTION_MAGIC.to_vec());
        out.extend(serde_json::to_vec(self).ok()?);
        (out.len() <= 4096).then_some(out)
    }
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if !bytes.starts_with(DISTRIBUTION_MAGIC) || bytes.len() > 4096 {
            return None;
        }
        serde_json::from_slice(&bytes[8..]).ok()
    }
    pub fn verify(&self, state: &PrivateChannel, claimed: &PeerId) -> bool {
        if self.domain != "chatter.mesh.private.sender.distribution.v1"
            || self.id != state.id
            || self.sender != claimed.as_str()
            || self.roster_hash != state.chain_hash().unwrap_or_default()
            || self.key.len() != 32
            || self.generation == 0
            || self.signature.len() != 64
            || self.removal_hash != state.removal_hash()
            || state.member_key(claimed).is_none()
        {
            return false;
        }
        let Some(key) = state.member_key(claimed) else {
            return false;
        };
        let Ok(sig) = <[u8; 64]>::try_from(self.signature.as_slice()) else {
            return false;
        };
        self.preimage()
            .is_some_and(|bytes| verify_bytes(&sig, &bytes, &key))
    }
    pub fn ack(&self) -> Option<Vec<u8>> {
        let mut out = DISTRIBUTION_ACK_MAGIC.to_vec();
        out.extend(uuid::Uuid::parse_str(&self.id).ok()?.as_bytes());
        out.extend(&self.roster_hash);
        out.extend(&self.generation.to_be_bytes());
        Some(out)
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
    pub fn from_text(text: &str, now: u64) -> Option<Self> {
        let data = URL_SAFE_NO_PAD
            .decode(text.strip_prefix(INVITE_PREFIX)?)
            .ok()?;
        if data.len() > 1024 {
            return None;
        }
        let invite: Self = serde_json::from_slice(&data).ok()?;
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
        let text = format!("{INVITE_PREFIX}{}", URL_SAFE_NO_PAD.encode(&bytes[8..]));
        Self::from_text(&text, now)
    }
}

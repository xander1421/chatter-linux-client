use super::protocol::{
    decode_hex, decode_roster, field, hex_lower, parse_tlv, put_tlv, truncate_utf8,
};
use super::*;

impl PrivateChannel {
    pub fn create(name: &str, identity: &MeshIdentity, nickname: &str) -> Result<Self, MeshError> {
        if name.trim().len() > 32 {
            return Err(failure("QR group name must be at most 32 bytes"));
        }
        let mut key = Zeroizing::new(vec![0u8; 32]);
        OsRng
            .try_fill_bytes(&mut key)
            .map_err(|_| failure("randomness unavailable"))?;
        let mut group_id = [0u8; 16];
        OsRng
            .try_fill_bytes(&mut group_id)
            .map_err(|_| failure("randomness unavailable"))?;
        let mut state = Self {
            id: uuid::Uuid::from_bytes(group_id).to_string(),
            name: valid_name(name)?,
            creator: identity.peer_id().to_string(),
            creator_key: identity.signing_public().to_vec(),
            creator_static: identity.static_public().to_vec(),
            epoch: 1,
            members: vec![Member {
                peer: identity.peer_id().to_string(),
                fingerprint: identity.fingerprint(),
                signing_key: identity.signing_public().to_vec(),
                nickname: nickname.to_string(),
            }],
            key,
            signature: vec![],
            replay: vec![],
            pending_invites: vec![],
            awaiting_rekey: vec![],
            unanswered_direct_invites: Default::default(),
            unanswered_expired: vec![],
            stale: false,
            stale_epoch: 0,
            departed: vec![],
            hint_signature: vec![],
            removed_members: vec![],
        };
        state.sign(identity)?;
        Ok(state)
    }

    pub fn group_id(&self) -> Option<[u8; 16]> {
        Some(*uuid::Uuid::parse_str(&self.id).ok()?.as_bytes())
    }

    pub fn roster_bytes(&self) -> Option<Vec<u8>> {
        if self.members.len() > MAX_MEMBERS {
            return None;
        }
        let mut out = vec![self.members.len() as u8];
        for member in &self.members {
            let fingerprint = decode_hex(&member.fingerprint)?;
            if fingerprint.len() != 32 || member.signing_key.len() != 32 {
                return None;
            }
            let nickname = truncate_utf8(&member.nickname, 64);
            out.extend(fingerprint);
            out.extend(&member.signing_key);
            out.push(nickname.len() as u8);
            out.extend(nickname);
        }
        Some(out)
    }

    fn signing_content(&self) -> Option<Vec<u8>> {
        let mut out = b"bitchat-group-v1".to_vec();
        out.extend(self.group_id()?);
        out.extend(self.epoch.to_be_bytes());
        out.extend(Sha256::digest(self.key.as_slice()));
        out.extend(Sha256::digest(self.roster_bytes()?));
        out.extend(Sha256::digest(self.name.as_bytes()));
        Some(out)
    }

    fn hint_content(&self) -> Option<Vec<u8>> {
        let mut out = b"chatter.mesh.group.epoch-hint.v1".to_vec();
        out.extend(self.group_id()?);
        out.extend(self.epoch.to_be_bytes());
        Some(out)
    }

    fn sign(&mut self, identity: &MeshIdentity) -> Result<(), MeshError> {
        if self.creator != identity.peer_id().as_str() {
            return Err(failure("only the creator can change membership"));
        }
        self.signature = sign_bytes(
            identity,
            &self
                .signing_content()
                .ok_or_else(|| failure("invalid group state"))?,
        )
        .to_vec();
        self.hint_signature = sign_bytes(
            identity,
            &self
                .hint_content()
                .ok_or_else(|| failure("invalid epoch hint"))?,
        )
        .to_vec();
        Ok(())
    }

    pub fn verify(&self) -> bool {
        if self.epoch == 0
            || self.key.len() != 32
            || self.creator_key.len() != 32
            || self.signature.len() != 64
            || self.members.is_empty()
            || self.members.len() > MAX_MEMBERS
            || valid_name(&self.name).is_err()
            || self.replay.len() > 512
            || self.removed_members.len() > 64
            || self.members[0].peer != self.creator
            || self.members[0].signing_key != self.creator_key
            || self.members[0].fingerprint.get(..16) != Some(PeerId::from_str(&self.creator).bare())
            || self.members.iter().any(|m| {
                m.signing_key.len() != 32
                    || decode_hex(&m.fingerprint).is_none_or(|b| b.len() != 32)
                    || m.fingerprint.get(..16) != Some(PeerId::from_str(&m.peer).bare())
            })
            || self
                .members
                .iter()
                .enumerate()
                .any(|(i, m)| self.members[..i].iter().any(|x| x.peer == m.peer))
        {
            return false;
        }
        let (Some(content), Ok(key), Ok(sig)) = (
            self.signing_content(),
            <[u8; 32]>::try_from(self.creator_key.as_slice()),
            <[u8; 64]>::try_from(self.signature.as_slice()),
        ) else {
            return false;
        };
        verify_bytes(&sig, &content, &key)
    }

    pub fn member_key(&self, peer: &PeerId) -> Option<[u8; 32]> {
        self.members
            .iter()
            .find(|m| m.peer == peer.as_str())
            .and_then(|m| m.signing_key.as_slice().try_into().ok())
    }

    pub fn leave_notice(&self, identity: &MeshIdentity) -> Result<LeaveNotice, MeshError> {
        if self.member_key(&identity.peer_id()).is_none() {
            return Err(failure("not a group member"));
        }
        let mut notice = LeaveNotice {
            id: self.id.clone(),
            epoch: self.epoch,
            peer: identity.peer_id().to_string(),
            signature: vec![],
        };
        notice.signature = sign_bytes(
            identity,
            &notice
                .preimage()
                .ok_or_else(|| failure("invalid leave notice"))?,
        )
        .to_vec();
        Ok(notice)
    }

    pub fn verify_leave(&self, notice: &LeaveNotice) -> bool {
        if notice.id != self.id || notice.epoch != self.epoch {
            return false;
        }
        let (Some(key), Ok(sig), Some(content)) = (
            self.member_key(&PeerId::from_str(&notice.peer)),
            <[u8; 64]>::try_from(notice.signature.as_slice()),
            notice.preimage(),
        ) else {
            return false;
        };
        verify_bytes(&sig, &content, &key)
    }

    pub fn epoch_proof(&self) -> EpochProof {
        EpochProof {
            id: self.id.clone(),
            epoch: self.epoch,
            signature: self.hint_signature.clone(),
        }
    }

    pub fn invite(
        &mut self,
        identity: &MeshIdentity,
        target: &PeerId,
        creator_static: [u8; 32],
        now: u64,
    ) -> Result<String, MeshError> {
        if self.name.len() > 32 {
            return Err(failure("group name cannot fit compact QR"));
        }
        if self.creator != identity.peer_id().as_str() {
            return Err(failure("only the creator can invite"));
        }
        if self.creator_static != creator_static {
            return Err(failure("creator Noise identity changed"));
        }
        if self.members.len() >= MAX_MEMBERS {
            return Err(failure("private group is full (16 members)"));
        }
        if target.to_routing_bytes().is_none() || self.member_key(target).is_some() {
            return Err(failure("invalid or existing member"));
        }
        let mut nonce = vec![0; 16];
        OsRng
            .try_fill_bytes(&mut nonce)
            .map_err(|_| failure("randomness unavailable"))?;
        let mut invite = Invite {
            channel_id: self.id.clone(),
            name: self.name.clone(),
            creator: self.creator.clone(),
            creator_key: self.creator_key.clone(),
            creator_static: self.creator_static.clone(),
            target: target.to_string(),
            nonce,
            expires_at: now.saturating_add(600),
            signature: vec![],
        };
        invite.signature = sign_bytes(identity, &invite.preimage()?).to_vec();
        self.pending_invites
            .retain(|i| i.expires_at >= now && i.target != target.as_str());
        if self.pending_invites.len() >= 32 {
            return Err(failure("too many pending invites"));
        }
        let text = invite.to_text().ok_or_else(|| failure("invalid invite"))?;
        self.pending_invites.push(invite);
        Ok(text)
    }

    pub fn rotate(
        &mut self,
        identity: &MeshIdentity,
        member: Option<Member>,
        remove: Option<&PeerId>,
    ) -> Result<(), MeshError> {
        if self.creator != identity.peer_id().as_str() {
            return Err(failure("membership requires creator"));
        }
        if let Some(ref m) = member {
            if self.members.len() >= MAX_MEMBERS
                || m.signing_key.len() != 32
                || self.members.iter().any(|old| old.peer == m.peer)
            {
                return Err(failure("invalid or full roster"));
            }
        }
        if let Some(peer) = remove {
            if peer.as_str() == self.creator || self.member_key(peer).is_none() {
                return Err(failure("invalid removal"));
            }
        }
        if let Some(m) = member {
            self.removed_members.retain(|peer| peer != &m.peer);
            self.members.push(m);
        }
        if let Some(peer) = remove {
            self.unanswered_direct_invites.remove(peer.as_str());
            self.unanswered_expired.retain(|old| old != peer.as_str());
            self.members.retain(|m| m.peer != peer.as_str());
            if !self.removed_members.iter().any(|old| old == peer.as_str()) {
                self.removed_members.push(peer.to_string());
            }
        }
        self.epoch = self
            .epoch
            .checked_add(1)
            .ok_or_else(|| failure("epoch exhausted"))?;
        OsRng
            .try_fill_bytes(&mut self.key)
            .map_err(|_| failure("randomness unavailable"))?;
        self.awaiting_rekey = self
            .members
            .iter()
            .filter(|m| m.peer != self.creator)
            .map(|m| m.peer.clone())
            .collect();
        self.departed.clear();
        self.stale = false;
        self.stale_epoch = 0;
        self.sign(identity)
    }

    pub fn update_bytes(&self) -> Result<Zeroizing<Vec<u8>>, MeshError> {
        let mut out = Zeroizing::new(Vec::new());
        put_tlv(
            1,
            &self.group_id().ok_or_else(|| failure("invalid group id"))?,
            &mut out,
        )?;
        put_tlv(2, self.name.as_bytes(), &mut out)?;
        put_tlv(3, &self.key, &mut out)?;
        put_tlv(4, &self.epoch.to_be_bytes(), &mut out)?;
        put_tlv(
            5,
            &self
                .roster_bytes()
                .ok_or_else(|| failure("invalid roster"))?,
            &mut out,
        )?;
        put_tlv(
            6,
            &decode_hex(&self.members[0].fingerprint).ok_or_else(|| failure("invalid creator"))?,
            &mut out,
        )?;
        put_tlv(7, &self.signature, &mut out)?;
        Ok(out)
    }

    pub fn removed_update_bytes(
        &self,
        identity: &MeshIdentity,
    ) -> Result<Zeroizing<Vec<u8>>, MeshError> {
        let mut removed = self.clone();
        removed.key.fill(0);
        removed.sign(identity)?;
        removed.update_bytes()
    }

    pub fn decode_update(bytes: &[u8]) -> Option<Self> {
        let fields = parse_tlv(bytes)?;
        let id = uuid::Uuid::from_slice(field(&fields, 1)?).ok()?.to_string();
        let name = String::from_utf8(field(&fields, 2)?.to_vec()).ok()?;
        let key = field(&fields, 3)?;
        if key.len() != 32 {
            return None;
        }
        let epoch = u32::from_be_bytes(field(&fields, 4)?.try_into().ok()?);
        let members = decode_roster(field(&fields, 5)?)?;
        let creator_fingerprint = hex_lower(field(&fields, 6)?);
        if creator_fingerprint.len() != 64 || members.first()?.fingerprint != creator_fingerprint {
            return None;
        }
        let signature = field(&fields, 7)?.to_vec();
        if signature.len() != 64 {
            return None;
        }
        let creator = members.first()?.peer.clone();
        let creator_key = members.first()?.signing_key.clone();
        let state = Self {
            id,
            name,
            creator,
            creator_key,
            creator_static: vec![],
            epoch,
            members,
            key: Zeroizing::new(key.to_vec()),
            signature,
            replay: vec![],
            pending_invites: vec![],
            awaiting_rekey: vec![],
            unanswered_direct_invites: Default::default(),
            unanswered_expired: vec![],
            stale: false,
            stale_epoch: 0,
            departed: vec![],
            hint_signature: vec![],
            removed_members: vec![],
        };
        state.verify().then_some(state)
    }
}

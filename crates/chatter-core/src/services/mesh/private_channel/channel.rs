use super::*;

impl PrivateChannel {
    pub fn create(name: &str, identity: &MeshIdentity) -> Result<Self, MeshError> {
        let name = valid_name(name)?;
        let mut key = Zeroizing::new(vec![0; 32]);
        OsRng
            .try_fill_bytes(&mut key)
            .map_err(|_| failure("randomness unavailable"))?;
        let mut state = Self {
            id: uuid::Uuid::new_v4().to_string(),
            name,
            creator: identity.peer_id().to_string(),
            creator_key: identity.signing_public().to_vec(),
            creator_static: identity.static_public().to_vec(),
            epoch: 1,
            members: vec![Member {
                peer: identity.peer_id().to_string(),
                signing_key: identity.signing_public().to_vec(),
            }],
            sender_keys: vec![SenderKey {
                peer: identity.peer_id().to_string(),
                generation: 1,
                key,
            }],
            prior_sender_keys: vec![],
            pending_distributions: vec![],
            distribution_attempts: vec![],
            replay: vec![],
            previous_hash: vec![0; 32],
            signature: vec![],
            pending_invites: vec![],
            awaiting_rekey: vec![],
            stale: false,
            stale_epoch: 0,
            departed: vec![],
        };
        state.sign(identity)?;
        Ok(state)
    }

    fn preimage(&self) -> Result<Vec<u8>, MeshError> {
        serde_json::to_vec(&EpochPreimage {
            domain: "chatter.mesh.private.roster.v2",
            id: &self.id,
            name: &self.name,
            creator: &self.creator,
            creator_static: &self.creator_static,
            epoch: self.epoch,
            members: &self.members,
            previous_hash: &self.previous_hash,
        })
        .map_err(|_| failure("invalid channel roster"))
    }

    pub fn chain_hash(&self) -> Result<Vec<u8>, MeshError> {
        Ok(Sha256::digest(self.preimage()?).to_vec())
    }

    fn sign(&mut self, identity: &MeshIdentity) -> Result<(), MeshError> {
        if self.creator != identity.peer_id().as_str() {
            return Err(failure("only the creator can change membership"));
        }
        self.signature = sign_bytes(identity, &self.preimage()?).to_vec();
        Ok(())
    }

    pub fn verify(&self) -> bool {
        if self.epoch == 0
            || uuid::Uuid::parse_str(&self.id).is_err()
            || self.creator_key.len() != 32
            || self.creator_static.len() != 32
            || self.previous_hash.len() != 32
            || self.signature.len() != 64
            || self.members.is_empty()
            || self.members.len() > MAX_MEMBERS
            || self.members[0].peer != self.creator
            || self.members[0].signing_key != self.creator_key
            || self.members.iter().any(|m| {
                m.signing_key.len() != 32 || PeerId::from_str(&m.peer).to_routing_bytes().is_none()
            })
            || self.sender_keys.len() > MAX_MEMBERS
            || self.prior_sender_keys.len() > 32
            || self.replay.len() > 512
            || self.sender_keys.iter().any(|key| {
                key.key.len() != 32
                    || key.generation == 0
                    || !self.members.iter().any(|member| member.peer == key.peer)
            })
            || self
                .pending_distributions
                .iter()
                .any(|peer| !self.members.iter().any(|member| &member.peer == peer))
            || self.distribution_attempts.len() > MAX_MEMBERS
            || self.departed.len() > MAX_MEMBERS
            || self
                .departed
                .iter()
                .any(|notice| !self.verify_leave(notice))
            || self
                .members
                .iter()
                .enumerate()
                .any(|(i, m)| self.members[..i].iter().any(|before| before.peer == m.peer))
            || valid_name(&self.name).is_err()
        {
            return false;
        }
        let (Ok(key), Ok(sig), Ok(preimage)) = (
            <[u8; 32]>::try_from(self.creator_key.as_slice()),
            <[u8; 64]>::try_from(self.signature.as_slice()),
            self.preimage(),
        ) else {
            return false;
        };
        verify_bytes(&sig, &preimage, &key)
    }

    pub fn member_key(&self, peer: &PeerId) -> Option<[u8; 32]> {
        self.members
            .iter()
            .find(|m| m.peer == peer.as_str())
            .and_then(|m| m.signing_key.as_slice().try_into().ok())
    }

    pub fn leave_notice(&self, identity: &MeshIdentity) -> Result<LeaveNotice, MeshError> {
        if self.member_key(&identity.peer_id()).is_none() {
            return Err(failure("not a channel member"));
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
        if notice.id != self.id || notice.epoch != self.epoch || notice.signature.len() != 64 {
            return false;
        }
        let Some(key) = self.member_key(&PeerId::from_str(&notice.peer)) else {
            return false;
        };
        let Ok(sig) = <[u8; 64]>::try_from(notice.signature.as_slice()) else {
            return false;
        };
        notice
            .preimage()
            .is_some_and(|bytes| verify_bytes(&sig, &bytes, &key))
    }

    pub fn epoch_proof(&self) -> EpochProof {
        EpochProof {
            domain: "chatter.mesh.private.roster.v2".into(),
            id: self.id.clone(),
            name: self.name.clone(),
            creator: self.creator.clone(),
            creator_static: self.creator_static.clone(),
            epoch: self.epoch,
            members: self.members.clone(),
            previous_hash: self.previous_hash.clone(),
            signature: self.signature.clone(),
        }
    }

    pub fn invite(
        &mut self,
        identity: &MeshIdentity,
        target: &PeerId,
        creator_static: [u8; 32],
        now: u64,
    ) -> Result<String, MeshError> {
        if self.creator != identity.peer_id().as_str() {
            return Err(failure("only the creator can invite"));
        }
        if self.creator_static.as_slice() != creator_static.as_slice() {
            return Err(failure("creator Noise identity changed"));
        }
        if self.members.len() >= MAX_MEMBERS {
            return Err(failure("private channel is full (16 members)"));
        }
        if target.to_routing_bytes().is_none() {
            return Err(failure("invite target is not a mesh device"));
        }
        if self.member_key(target).is_some() {
            return Err(failure("peer is already a member"));
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
            creator_static: creator_static.to_vec(),
            target: target.to_string(),
            nonce,
            expires_at: now.saturating_add(600),
            signature: vec![],
        };
        invite.signature = sign_bytes(identity, &invite.preimage()?).to_vec();
        self.pending_invites
            .retain(|i| i.expires_at >= now && i.target != target.as_str());
        if self.pending_invites.len() >= 32 {
            return Err(failure("too many pending private channel invites"));
        }
        self.pending_invites.push(invite.clone());
        let bytes = serde_json::to_vec(&invite).map_err(|_| failure("invalid invite"))?;
        Ok(format!("{INVITE_PREFIX}{}", URL_SAFE_NO_PAD.encode(bytes)))
    }

    pub fn rotate(
        &mut self,
        identity: &MeshIdentity,
        member: Option<Member>,
        remove: Option<&PeerId>,
    ) -> Result<(), MeshError> {
        if self.creator != identity.peer_id().as_str() {
            return Err(failure("membership is frozen until the creator returns"));
        }
        if let Some(ref member) = member {
            if self.members.len() >= MAX_MEMBERS
                || member.signing_key.len() != 32
                || self.members.iter().any(|m| m.peer == member.peer)
            {
                return Err(failure("invalid or full roster"));
            }
        }
        if let Some(remove) = remove {
            if remove.as_str() == self.creator {
                return Err(failure("creator cannot leave"));
            }
            if !self.members.iter().any(|m| m.peer == remove.as_str()) {
                return Err(failure("peer is not a member"));
            }
        }
        self.previous_hash = Sha256::digest(self.preimage()?).to_vec();
        self.departed.clear();
        self.remember_current();
        if let Some(member) = member {
            self.members.push(member);
        }
        if let Some(remove) = remove {
            self.members.retain(|m| m.peer != remove.as_str());
        }
        self.awaiting_rekey = self
            .members
            .iter()
            .filter(|m| m.peer != self.creator)
            .map(|m| m.peer.clone())
            .collect();
        self.epoch = self
            .epoch
            .checked_add(1)
            .ok_or_else(|| failure("epoch exhausted"))?;
        self.sender_keys
            .retain(|key| self.members.iter().any(|m| m.peer == key.peer));
        self.invalidate_other_sender_keys(&identity.peer_id());
        self.pending_distributions = self
            .members
            .iter()
            .filter(|m| m.peer != identity.peer_id().as_str())
            .map(|m| m.peer.clone())
            .collect();
        self.distribution_attempts.clear();
        self.sign(identity)
    }

    pub fn update_bytes(&self) -> Result<Zeroizing<Vec<u8>>, MeshError> {
        let mut out = Zeroizing::new(UPDATE_MAGIC.to_vec());
        // Serialize only the signed current roster; local state never crosses Noise.
        let wire = CurrentEpoch {
            id: &self.id,
            name: &self.name,
            creator: &self.creator,
            creator_key: &self.creator_key,
            creator_static: &self.creator_static,
            epoch: self.epoch,
            members: &self.members,
            previous_hash: &self.previous_hash,
            signature: &self.signature,
        };
        out.extend(serde_json::to_vec(&wire).map_err(|_| failure("invalid roster"))?);
        Ok(out)
    }

    pub fn decode_update(bytes: &[u8]) -> Option<Self> {
        if !bytes.starts_with(UPDATE_MAGIC) || bytes.len() > 8192 {
            return None;
        }
        let state: Self = serde_json::from_slice(&bytes[8..]).ok()?;
        (state.pending_invites.is_empty()
            && state.awaiting_rekey.is_empty()
            && !state.stale
            && state.stale_epoch == 0
            && state.departed.is_empty()
            && state.sender_keys.is_empty()
            && state.prior_sender_keys.is_empty()
            && state.pending_distributions.is_empty()
            && state.distribution_attempts.is_empty()
            && state.replay.is_empty()
            && state.verify())
        .then_some(state)
    }
}

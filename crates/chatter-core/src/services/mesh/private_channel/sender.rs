use super::*;

impl PrivateChannel {
    pub fn member_key_for_packet(&self, peer: &PeerId, bytes: &[u8]) -> Option<[u8; 32]> {
        if self
            .departed
            .iter()
            .any(|notice| notice.peer == peer.as_str())
        {
            return None;
        }
        if !self.has_route_tag(bytes) {
            return None;
        }
        self.member_key(peer)
    }

    pub fn has_route_tag(&self, bytes: &[u8]) -> bool {
        if bytes.len() < 32 || !bytes.starts_with(FRAME_MAGIC) {
            return false;
        }
        let current = self.chain_hash().unwrap_or_default();
        self.sender_keys
            .iter()
            .any(|key| route_tag(&self.id, &current, &key.peer, key.generation) == bytes[8..24])
            || self.prior_sender_keys.iter().any(|prior| {
                route_tag(
                    &self.id,
                    &prior.roster_hash,
                    &prior.sender.peer,
                    prior.sender.generation,
                ) == bytes[8..24]
                    && prior.expires_at >= super::super::now_secs()
            })
    }

    pub fn missing_sender_keys(&self, own: &PeerId) -> usize {
        self.members
            .iter()
            .filter(|m| {
                m.peer != own.as_str()
                    && !self.departed.iter().any(|n| n.peer == m.peer)
                    && !self.sender_keys.iter().any(|key| key.peer == m.peer)
            })
            .count()
    }

    pub fn initialize_sender(&mut self, identity: &MeshIdentity) -> Result<(), MeshError> {
        let peer = identity.peer_id();
        if self.member_key(&peer).is_none() {
            return Err(failure("not a channel member"));
        }
        if self.sender_keys.iter().any(|key| key.peer == peer.as_str()) {
            return Ok(());
        }
        let mut key = Zeroizing::new(vec![0; 32]);
        OsRng
            .try_fill_bytes(&mut key)
            .map_err(|_| failure("randomness unavailable"))?;
        self.sender_keys.push(SenderKey {
            peer: peer.to_string(),
            generation: 1,
            key,
        });
        self.pending_distributions = self
            .members
            .iter()
            .filter(|m| m.peer != peer.as_str() && !self.departed.iter().any(|n| n.peer == m.peer))
            .map(|m| m.peer.clone())
            .collect();
        self.distribution_attempts.clear();
        Ok(())
    }

    pub fn rotate_sender(&mut self, identity: &MeshIdentity) -> Result<(), MeshError> {
        let peer = identity.peer_id();
        let pos = self
            .sender_keys
            .iter()
            .position(|key| key.peer == peer.as_str())
            .ok_or_else(|| failure("sender key missing"))?;
        let hash = self.chain_hash()?;
        let old = self.sender_keys[pos].clone();
        let generation = old
            .generation
            .checked_add(1)
            .ok_or_else(|| failure("sender generation exhausted"))?;
        self.prior_sender_keys.push(PriorSenderKey {
            sender: old,
            roster_hash: hash,
            expires_at: super::super::now_secs().saturating_add(300),
        });
        if self.prior_sender_keys.len() > 32 {
            self.prior_sender_keys.remove(0);
        }
        let mut key = Zeroizing::new(vec![0; 32]);
        OsRng
            .try_fill_bytes(&mut key)
            .map_err(|_| failure("randomness unavailable"))?;
        self.sender_keys[pos] = SenderKey {
            peer: peer.to_string(),
            generation,
            key,
        };
        self.pending_distributions = self
            .members
            .iter()
            .filter(|m| m.peer != peer.as_str() && !self.departed.iter().any(|n| n.peer == m.peer))
            .map(|m| m.peer.clone())
            .collect();
        self.distribution_attempts.clear();
        Ok(())
    }

    pub fn reset_distribution_attempt(&mut self, peer: &str) {
        self.distribution_attempts.retain(|(p, _)| p != peer);
    }

    pub fn mark_distribution_attempt(&mut self, peer: &str) -> bool {
        if !self.pending_distributions.iter().any(|p| p == peer) {
            return false;
        }
        if let Some((_, count)) = self
            .distribution_attempts
            .iter_mut()
            .find(|(p, _)| p == peer)
        {
            if *count >= 3 {
                return false;
            }
            *count += 1;
        } else {
            self.distribution_attempts.push((peer.to_owned(), 1));
        }
        true
    }

    pub fn invalidate_other_sender_keys(&mut self, own: &PeerId) {
        if let Ok(hash) = self.chain_hash() {
            for key in self
                .sender_keys
                .iter()
                .filter(|key| key.peer != own.as_str())
            {
                self.prior_sender_keys.push(PriorSenderKey {
                    sender: key.clone(),
                    roster_hash: hash.clone(),
                    expires_at: super::super::now_secs().saturating_add(300),
                });
            }
            if self.prior_sender_keys.len() > 32 {
                self.prior_sender_keys
                    .drain(..self.prior_sender_keys.len() - 32);
            }
        }
        self.sender_keys.retain(|key| key.peer == own.as_str());
    }

    pub fn distribution(&self, identity: &MeshIdentity) -> Option<SenderDistribution> {
        let peer = identity.peer_id();
        let sender = self
            .sender_keys
            .iter()
            .find(|key| key.peer == peer.as_str())?;
        let removal_hash = self.removal_hash();
        let mut distribution = SenderDistribution {
            domain: "chatter.mesh.private.sender.distribution.v1".into(),
            id: self.id.clone(),
            roster_hash: self.chain_hash().ok()?,
            sender: peer.to_string(),
            generation: sender.generation,
            key: sender.key.clone(),
            removal_hash,
            signature: vec![],
        };
        distribution.signature = sign_bytes(identity, &distribution.preimage()?).to_vec();
        Some(distribution)
    }

    fn removal_hash(&self) -> Vec<u8> {
        self.departed
            .iter()
            .filter_map(|notice| notice.preimage())
            .map(|bytes| Sha256::digest(bytes).to_vec())
            .max()
            .unwrap_or_else(|| self.previous_hash.clone())
    }

    pub fn accept_distribution(
        &mut self,
        distribution: &SenderDistribution,
        claimed: &PeerId,
    ) -> bool {
        if !distribution.verify(self, claimed) {
            return false;
        }
        if let Some(existing) = self
            .sender_keys
            .iter_mut()
            .find(|key| key.peer == distribution.sender)
        {
            if distribution.generation <= existing.generation {
                return false;
            }
            let old = existing.clone();
            self.prior_sender_keys.push(PriorSenderKey {
                sender: old,
                roster_hash: distribution.roster_hash.clone(),
                expires_at: super::super::now_secs().saturating_add(300),
            });
            if self.prior_sender_keys.len() > 32 {
                self.prior_sender_keys.remove(0);
            }
            *existing = SenderKey {
                peer: distribution.sender.clone(),
                generation: distribution.generation,
                key: distribution.key.clone(),
            };
        } else {
            self.sender_keys.push(SenderKey {
                peer: distribution.sender.clone(),
                generation: distribution.generation,
                key: distribution.key.clone(),
            });
        }
        true
    }

    pub fn remember_previous(&mut self, old: &Self) {
        self.sender_keys = old.sender_keys.clone();
        self.prior_sender_keys = old.prior_sender_keys.clone();
        if let Ok(hash) = old.chain_hash() {
            for key in &old.sender_keys {
                self.prior_sender_keys.push(PriorSenderKey {
                    sender: key.clone(),
                    roster_hash: hash.clone(),
                    expires_at: super::super::now_secs().saturating_add(300),
                });
            }
            if self.prior_sender_keys.len() > 32 {
                self.prior_sender_keys
                    .drain(..self.prior_sender_keys.len() - 32);
            }
        }
        self.replay = old.replay.clone();
        self.pending_distributions = old.pending_distributions.clone();
        self.distribution_attempts = old.distribution_attempts.clone();
    }

    pub(super) fn remember_current(&mut self) {
        if let Ok(hash) = self.chain_hash() {
            for key in &self.sender_keys {
                self.prior_sender_keys.push(PriorSenderKey {
                    sender: key.clone(),
                    roster_hash: hash.clone(),
                    expires_at: super::super::now_secs().saturating_add(300),
                });
            }
            if self.prior_sender_keys.len() > 32 {
                self.prior_sender_keys
                    .drain(..self.prior_sender_keys.len() - 32);
            }
        }
    }
}

pub(super) fn route_tag(id: &str, roster_hash: &[u8], sender: &str, generation: u64) -> [u8; 16] {
    let mut hash = Sha256::new();
    hash.update(b"chatter.mesh.private.sender.route.v1\0");
    hash.update(id.as_bytes());
    hash.update(roster_hash);
    hash.update(sender.as_bytes());
    hash.update(generation.to_be_bytes());
    hash.finalize()[..16].try_into().expect("digest length")
}

use super::*;

impl MeshTimeline {
    /// Note that a geohash cell is joined, so it shows up in the sidebar before
    /// anyone has said anything in it.
    pub fn note_geohash(&self, cell: &str) {
        self.register(&MeshConversation::Geohash(cell.to_string()), None);
    }

    /// Keep a newly chosen peer visible before either side has sent a message.
    pub fn open_dm(&self, peer: &PeerId, nickname: &str) {
        self.register(&MeshConversation::Dm(peer.clone()), Some(nickname));
    }

    /// Drop a geohash cell from the sidebar when it is left.
    ///
    /// Only the sidebar entry goes: the messages stay on disk, and rejoining the
    /// cell brings the conversation back with its history intact.
    pub fn forget_geohash(&self, cell: &str) {
        let conversation = MeshConversation::Geohash(cell.to_string());
        let id = conversation.conversation_id();
        self.open.lock().remove(&id);
        if let Err(error) = self.directory.remove(&id.to_string()) {
            tracing::warn!(%error, "mesh: could not forget a cell durably");
        }
    }

    pub fn join_channel(&self, name: &str) {
        self.nearby_channels.lock().remove(name);
        self.register(&MeshConversation::Channel(name.to_string()), None);
    }

    pub fn open_private_channel(&self, id: &str, name: &str) {
        self.register(&MeshConversation::PrivateChannel(id.to_owned()), Some(name));
    }

    pub fn close_private_channel(&self, id: &str) {
        let conversation = MeshConversation::PrivateChannel(id.to_owned());
        self.open.lock().remove(&conversation.conversation_id());
        if let Err(error) = self
            .directory
            .remove(&conversation.conversation_id().to_string())
        {
            tracing::warn!(%error, "mesh: could not close private channel");
        }
    }

    pub fn leave_channel(&self, name: &str) {
        let conversation = MeshConversation::Channel(name.to_string());
        let id = conversation.conversation_id();
        self.open.lock().remove(&id);
        if let Err(error) = self.directory.remove(&id.to_string()) {
            tracing::warn!(%error, "mesh: could not leave channel durably");
        }
    }

    pub fn has_channel(&self, name: &str) -> bool {
        self.open
            .lock()
            .contains_key(&MeshConversation::Channel(name.to_string()).conversation_id())
    }

    pub fn refresh_peer_nickname(&self, peer: &PeerId, nickname: &str) {
        let conversation = MeshConversation::Dm(peer.clone());
        let Some(label) = dm_label(&conversation, Some(nickname)) else {
            return;
        };
        let changed = {
            let mut open = self.open.lock();
            if let Some(entry) = open.get_mut(&conversation.conversation_id()) {
                if entry.label != label {
                    entry.label = label.clone();
                    true
                } else {
                    false
                }
            } else {
                false
            }
        };
        if changed {
            self.persist_entry(&conversation, &label);
        }
    }

    pub fn active_channels(&self) -> Vec<String> {
        self.open
            .lock()
            .values()
            .filter_map(|entry| {
                if let MeshConversation::Channel(name) = &entry.conversation {
                    Some(name.clone())
                } else {
                    None
                }
            })
            .collect()
    }

    pub fn nearby_channels(&self) -> Vec<String> {
        let mut nearby = self.nearby_channels.lock();
        nearby.retain(|_, seen| seen.elapsed() < std::time::Duration::from_secs(180));
        nearby.keys().cloned().collect()
    }

    pub fn observe_channel(&self, name: &str) {
        if !self.has_channel(name) {
            let mut nearby = self.nearby_channels.lock();
            nearby.retain(|_, seen| seen.elapsed() < std::time::Duration::from_secs(180));
            if nearby.len() >= 128 && !nearby.contains_key(name) {
                if let Some(oldest) = nearby
                    .iter()
                    .min_by_key(|(_, seen)| *seen)
                    .map(|(name, _)| name.clone())
                {
                    nearby.remove(&oldest);
                }
            }
            nearby.insert(name.to_owned(), std::time::Instant::now());
        }
    }
}

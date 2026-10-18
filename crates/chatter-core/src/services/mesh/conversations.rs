use super::*;

impl MeshService {
    /// Create a durable DM row so it can be opened before the first message.
    pub fn open_dm(&self, peer: &PeerId) -> Result<String, MeshError> {
        let known = self
            .state
            .peers()
            .into_iter()
            .find(|known| known.peer == *peer)
            .ok_or(MeshError::OutOfRange("unknown mesh peer"))?;
        self.timeline.open_dm(peer, &known.nickname);
        Ok(MeshConversation::Dm(peer.clone())
            .conversation_id()
            .to_string())
    }

    /// The nickname behind a mesh-derived `UserId`, if this device knows one.
    ///
    /// Mesh messages are stored with `mapping::peer_user_id(peer)` as their
    /// sender — a hash of the peer ID, so it lives in the same `UserId` space as
    /// every other message and needs no special case in storage or in the
    /// timeline. The human-readable nickname deliberately does not travel with
    /// it (`mapping.rs`), which is fine while a message is in flight — the live
    /// path carries the nickname alongside — and wrong the moment it is read
    /// back from the store, where all that survived is the hash. Re-opening a
    /// mesh conversation therefore listed every sender as a raw UUID.
    ///
    /// `peer_user_id` is one-way, so this recovers the name by recomputing it
    /// over the peers this device knows rather than by inverting anything. That
    /// is O(peers) per lookup, over a set bounded by Bluetooth range and by the
    /// geohash cells joined — tens, not thousands.
    ///
    /// A peer that is no longer known falls back to the nickname it last used,
    /// remembered when its message was stored. `None` means no peer we know
    /// hashes to this id and none ever named itself to us; callers must render
    /// that case rather than let a UUID through.
    ///
    /// Either way the name is self-declared — what the peer called itself, not
    /// a verified identity.
    #[must_use]
    pub fn sender_display_name(&self, sender: &UserId) -> Option<String> {
        self.state
            .peers()
            .into_iter()
            .find(|p| mapping::peer_user_id(&p.peer) == *sender)
            .map(|p| {
                if p.nickname.trim().is_empty() {
                    format!(
                        "Peer {}",
                        p.peer.as_str().chars().take(8).collect::<String>()
                    )
                } else {
                    p.nickname
                }
            })
            .or_else(|| self.timeline.remembered_sender_name(sender))
    }

    /// Transport status across both halves.
    #[must_use]
    pub fn status(&self) -> MeshStatus {
        self.state.status(self.nostr_state.connected_relay_count())
    }

    /// Whether [`start`](Self::start) succeeded and has not been stopped.
    ///
    /// Deliberately **not** a field on [`MeshStatus`]. That struct is "which
    /// transports are live", it is rebuilt from `MeshState` in several places
    /// that have no view of the service, and it travels as
    /// [`MeshEvent::Status`](crate::services::mesh::MeshEvent::Status) — so a
    /// service flag riding along inside it would have read `false` every time a
    /// relay flapped, and told the UI the mesh had died.
    ///
    /// Distinct from `MeshStatus::radio_running`, and the distinction is the
    /// point: a running service with no radio still carries geohash cells over
    /// relays and still retains what you write for a carrier that has not
    /// arrived yet, whereas a service that never started has nothing at all — no
    /// timeline, no identity loaded, no store to queue into.
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// The geohash cells currently joined.
    #[must_use]
    pub fn joined_geohashes(&self) -> Vec<String> {
        self.nostr_state.joined_cells()
    }

    /// The conversation a public mesh room message belongs to.
    #[must_use]
    pub fn room_conversation() -> MeshConversation {
        MeshConversation::Room
    }

    /// Every mesh conversation, shaped for the unified sidebar.
    ///
    /// The public room, every joined geohash cell, and every mesh DM — already
    /// ordered and counted. The caller renders this list; it filters nothing and
    /// derives nothing, and in particular it does not go looking for a server
    /// record of any of these conversations, because there is none.
    pub fn conversations(&self) -> Result<Vec<UnifiedConversation>, MeshError> {
        Ok(self.timeline.conversations()?)
    }

    /// Resolve and order sidebar rows before the UI maps them to its model.
    pub fn sidebar_rows(&self, owner: Option<UserId>) -> Result<Vec<MeshSidebarRow>, MeshError> {
        let peers = self.peers();
        Ok(self.conversations()?.into_iter().filter_map(|row| {
            let conversation = self.conversation_for(&row.id)?;
            let (kind, scope) = match &conversation {
                MeshConversation::Room => ("room", String::new()),
                MeshConversation::Geohash(cell) => ("geohash", cell.clone()),
                MeshConversation::Channel(name) => ("channel", name.clone()),
                MeshConversation::PrivateChannel(id) => ("private-channel", id.clone()),
                MeshConversation::Dm(peer) => ("dm", peer.as_str().to_owned()),
            };
            let (reachable, verified, binding_label) = match &conversation {
                MeshConversation::Dm(peer) => {
                    let (reachable, verified) = peers.iter().find(|known| known.peer == *peer)
                        .map(|known| (known.connected || known.reachable, known.account_verified))
                        .unwrap_or((false, false));
                    let label = owner.and_then(|id| self.binding_state(id, peer).ok())
                        .map(|state| state.label())
                        .unwrap_or("Mesh peer · self-asserted name");
                    (reachable, verified, label)
                }
                _ => (false, false, ""),
            };
            let muted = matches!(&conversation, MeshConversation::Channel(name) if self.channel_muted(name));
            let blocked = matches!(&conversation, MeshConversation::Dm(peer) if self.peer_blocked(peer));
            Some(MeshSidebarRow {
                conversation, id: row.id, kind, scope, title: row.name,
                last_message_at: row.last_message_at,
                unread_count: i32::try_from(row.unread_count).unwrap_or(i32::MAX),
                reachable, verified, muted, blocked, binding_label,
            })
        }).collect())
    }

    /// Resolve one row using the same visibility and scope rules as the sidebar.
    pub fn sidebar_row_for(
        &self,
        owner: Option<UserId>,
        id: &str,
    ) -> Result<Option<MeshSidebarRow>, MeshError> {
        Ok(self
            .sidebar_rows(owner)?
            .into_iter()
            .find(|row| row.id == id))
    }

    /// Resolve one nearby peer by its transport identity.
    pub fn peer_by_id(&self, peer_id: &str) -> Option<MeshPeer> {
        self.peers()
            .into_iter()
            .find(|peer| peer.peer.as_str() == peer_id)
    }

    /// The mesh conversation a sidebar row refers to, or `None` when the id is
    /// not one of ours.
    ///
    /// The UI holds conversation ids as strings and cannot re-derive a
    /// [`MeshConversation`] from one — the derivation is one-way — so opening a
    /// mesh row goes through here.
    #[must_use]
    pub fn conversation_for(&self, id: &str) -> Option<MeshConversation> {
        let id: ChannelId = id.parse().ok()?;
        self.timeline.conversation_for(&id)
    }

    /// The same title the mesh sidebar shows, without loading all conversations.
    #[must_use]
    pub fn conversation_name(&self, id: &ChannelId) -> Option<String> {
        self.timeline.conversation_name(id)
    }

    /// History for one mesh conversation, oldest first.
    ///
    /// `before_sent_at` pages backwards through it in the sender's nanosecond
    /// timestamps, the same cursor every other conversation type uses.
    pub fn history(
        &self,
        conversation: &MeshConversation,
        before_sent_at: Option<u64>,
        limit: u32,
    ) -> Result<Vec<Message>, MeshError> {
        Ok(self.timeline.history(conversation, before_sent_at, limit)?)
    }

    /// Mark a mesh conversation read on this device.
    ///
    /// Local only. Telling the peer costs airtime, so it is a separate act:
    /// [`mark_read`](Self::mark_read) puts a receipt on the wire.
    pub fn mark_conversation_read(&self, conversation: &MeshConversation) -> Result<(), MeshError> {
        Ok(self.timeline.mark_read(conversation)?)
    }
}

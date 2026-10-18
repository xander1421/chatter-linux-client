use super::*;

/// The mesh's event fan-out, with the message store in front of it.
///
/// Cloned into every runtime, which sends through it exactly as it would send
/// through the underlying [`broadcast::Sender`] — the persistence is not
/// something a call site has to remember.
#[derive(Clone)]
pub struct MeshEvents {
    events: broadcast::Sender<MeshEvent>,
    timeline: Arc<MeshTimeline>,
    muted_channels: Arc<Mutex<std::collections::HashSet<String>>>,
    muted_peers: Arc<Mutex<std::collections::HashSet<String>>>,
    blocked_peers: Arc<Mutex<std::collections::HashSet<String>>>,
}

impl MeshEvents {
    pub fn open_private_channel(&self, id: &str, name: &str) {
        self.timeline.open_private_channel(id, name);
    }
    /// Signal receive must know the row reached SQLCipher before committing its
    /// ratchet and dedup marker. A failed insert leaves the receive journal for
    /// rollback/replay and never emits a delivery receipt.
    pub fn persist_signal_received(&self, message: &MeshMessage) -> Result<(), StorageError> {
        if self.peer_blocked(&message.sender) {
            return Err(StorageError::InvalidData(
                "blocked mesh Signal sender".into(),
            ));
        }
        self.timeline.record(message)?;
        Ok(())
    }

    pub fn publish_signal_received(&self, message: MeshMessage) {
        let _ = self.events.send(MeshEvent::Message(Box::new(message)));
    }
    /// A fan-out buffering `capacity` events, storing through `timeline`.
    #[must_use]
    pub fn new(capacity: usize, timeline: Arc<MeshTimeline>) -> Self {
        let (events, _) = broadcast::channel(capacity);
        Self {
            events,
            timeline,
            muted_channels: Arc::new(Mutex::new(Default::default())),
            muted_peers: Arc::new(Mutex::new(Default::default())),
            blocked_peers: Arc::new(Mutex::new(Default::default())),
        }
    }

    pub fn set_channel_muted(&self, name: &str, muted: bool) {
        let mut names = self.muted_channels.lock();
        if muted {
            names.insert(name.to_owned());
        } else {
            names.remove(name);
        }
    }

    pub fn channel_muted(&self, name: &str) -> bool {
        self.muted_channels.lock().contains(name)
    }

    pub fn set_peer_muted(&self, peer: &PeerId, muted: bool) {
        let mut peers = self.muted_peers.lock();
        if muted {
            peers.insert(peer.as_str().to_owned());
        } else {
            peers.remove(peer.as_str());
        }
    }

    pub fn peer_muted(&self, peer: &PeerId) -> bool {
        self.muted_peers.lock().contains(peer.as_str())
    }

    pub fn set_peer_blocked(&self, peer: &PeerId, blocked: bool) {
        let mut peers = self.blocked_peers.lock();
        if blocked {
            peers.insert(peer.as_str().to_owned());
        } else {
            peers.remove(peer.as_str());
        }
    }

    pub fn peer_blocked(&self, peer: &PeerId) -> bool {
        self.blocked_peers.lock().contains(peer.as_str())
    }

    pub fn clear_moderation(&self) {
        self.muted_channels.lock().clear();
        self.muted_peers.lock().clear();
        self.blocked_peers.lock().clear();
    }

    pub fn channel_joined(&self, name: &str) -> bool {
        self.timeline.has_channel(name)
    }

    pub fn active_channels(&self) -> Vec<String> {
        self.timeline.active_channels()
    }

    pub fn nearby_channels(&self) -> Vec<String> {
        self.timeline.nearby_channels()
    }

    /// Store what needs storing, then tell everyone.
    ///
    /// The store runs first so that a subscriber woken by this event can read
    /// the row back immediately. A failed write is logged and the event still
    /// goes out: the message did arrive, and dropping it from the live timeline
    /// as well would turn a storage problem into a lost message.
    pub fn send(&self, event: MeshEvent) {
        if let MeshEvent::Message(message) = &event {
            if !message.is_local && self.peer_blocked(&message.sender) {
                return;
            }
            if let MeshConversation::Channel(name) = &message.conversation {
                if !message.is_local && self.channel_muted(name) {
                    return;
                }
            }
        }
        match &event {
            MeshEvent::Message(message) => match self.timeline.record(message) {
                Ok(false)
                    if matches!(&message.conversation, MeshConversation::PrivateChannel(_)) =>
                {
                    return;
                }
                Err(error) => tracing::error!(
                    target: "mesh", %error, "a mesh message could not be stored; it is live-only"
                ),
                _ => {}
            },
            MeshEvent::Delivery {
                message_id, state, ..
            } => self.timeline.record_delivery(message_id, *state),
            MeshEvent::GeohashJoined(cell) => self.timeline.note_geohash(cell),
            MeshEvent::GeohashLeft(cell) => self.timeline.forget_geohash(cell),
            MeshEvent::ChannelObserved(name) => self.timeline.observe_channel(name),
            MeshEvent::Peers(peers) => {
                for peer in peers {
                    self.timeline
                        .refresh_peer_nickname(&peer.peer, &peer.nickname);
                }
            }
            _ => {}
        }
        // No subscribers is the normal state before a UI attaches, not an error.
        let _ = self.events.send(event);
    }

    /// Store a mesh file and hand back the id a message can carry it by.
    ///
    /// Deliberately *not* part of [`send`](Self::send), which is where every
    /// other write in this module happens: the event a runtime sends already
    /// names the attachment by id, so the bytes have to be on disk before the
    /// event exists. There is no shape of [`MeshMessage`] that means "a file
    /// whose content has not landed yet".
    pub fn store_file(
        &self,
        file_name: Option<&str>,
        mime_type: Option<&str>,
        bytes: &[u8],
    ) -> Result<AttachmentId, MeshError> {
        self.timeline.store_file(file_name, mime_type, bytes)
    }

    /// Store a mesh file that is already on disk, without reading it into memory.
    ///
    /// See [`MeshTimeline::store_file_streamed`]. Used by both ends of a chunked
    /// transfer: the receiver's assembled spool file and the sender's own copy of
    /// the file it is streaming out.
    pub fn store_file_streamed(
        &self,
        file_name: Option<&str>,
        mime_type: Option<&str>,
        plaintext: &std::path::Path,
    ) -> Result<AttachmentId, MeshError> {
        self.timeline
            .store_file_streamed(file_name, mime_type, plaintext)
    }

    /// Whether a message with this wire ID is already on disk.
    ///
    /// See [`MeshTimeline::already_stored`] for why only the file path asks.
    pub fn already_stored(&self, wire_id: &str) -> Result<bool, StorageError> {
        self.timeline.already_stored(wire_id)
    }

    /// Subscribe to the fan-out.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<MeshEvent> {
        self.events.subscribe()
    }
}

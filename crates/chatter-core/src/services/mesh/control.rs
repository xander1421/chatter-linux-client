use super::*;

impl MeshService {
    /// Join a geohash channel.
    pub fn join_geohash(&self, cell: &str) -> Result<(), MeshError> {
        if !chatter_mesh_nostr::geohash::is_valid(cell) {
            return Err(MeshError::InvalidGeohash(cell.to_string()));
        }
        if self.relay_connector.is_none() {
            return Err(MeshError::NoRelayConnector);
        }
        self.nostr
            .send(NostrCommand::Join(cell.to_string()))
            .map_err(|_| MeshError::WorkerGone)
    }

    /// Leave a geohash channel.
    pub fn leave_geohash(&self, cell: &str) -> Result<(), MeshError> {
        self.nostr
            .send(NostrCommand::Leave(cell.to_string()))
            .map_err(|_| MeshError::WorkerGone)
    }

    /// Announce our identity now, outside the normal cadence.
    pub fn announce_now(&self) -> Result<(), MeshError> {
        if !self.has_radio {
            return Err(MeshError::NoRadio);
        }
        self.ble
            .send(BleCommand::Announce)
            .map_err(|_| MeshError::WorkerGone)
    }

    /// Tell the duty cycle whether the app is in the foreground.
    ///
    /// Scanning is the expensive half of BLE. In the foreground it runs
    /// continuously; in the background it duty-cycles.
    pub fn set_app_active(&self, active: bool) {
        self.state.with(|state| state.app_is_active = active);
    }

    /// Forget everything: retained mail, carried envelopes, gossip history, and
    /// the bridge's dedup sets.
    ///
    /// The identity itself is wiped separately, by
    /// [`MeshIdentityStore::wipe`](crate::storage::mesh_identity_store::MeshIdentityStore::wipe) —
    /// they are deliberately not the same operation, because clearing a device's
    /// mail is not the same act as destroying its identity.
    pub fn wipe(&self) -> Result<(), MeshError> {
        self.state.with(|state| {
            state.sessions.clear();
            state.chatter_hello.clear_live();
            state.pending_bindings.clear();
        });
        self.router.send(RouterCommand::Wipe)
    }

    /// Panic wipe: destroy everything this device holds about the mesh.
    ///
    /// Clears, in order: live sessions, peer pins, our one-time prekeys,
    /// retained mail, carried courier envelopes, gossip history, the bridge
    /// dedup sets, every mesh message, the conversation directory, and finally
    /// the identity keypair itself.
    ///
    /// # What this does not clear
    ///
    /// The chatter account, its Signal identity, and its messages. They are a
    /// separate identity with a separate threat model, and destroying an
    /// account is a larger, irreversible act that deserves its own deliberate
    /// confirmation rather than riding along with this one. Anyone relying on
    /// this to leave a device clean of *everything* needs to know that.
    ///
    /// # Why it continues after a failure
    ///
    /// Every step runs even if an earlier one fails, and the first error is
    /// returned at the end. Stopping at the first failure would leave the
    /// remainder on disk — the worst possible outcome for the one operation
    /// whose entire purpose is leaving nothing behind. A partial wipe that
    /// reports success would be worse still, so the error is not swallowed.
    ///
    /// The identity goes last. The steps before it log and emit events, and
    /// doing those under a destroyed identity would be the one moment this
    /// service could not describe itself.
    pub fn panic_wipe(&self) -> Result<(), MeshError> {
        let _signal_guard = self.signal_lock.lock();
        self.config.set_nickname("anon".to_owned());
        self.events.clear_moderation();
        let mut first_error: Option<MeshError> = None;
        let fail = |error: MeshError, slot: &mut Option<MeshError>| {
            if slot.is_none() {
                *slot = Some(error);
            }
        };

        self.state.with(|state| {
            state.sessions.clear();
            state.chatter_hello.clear_live();
            state.pending_bindings.clear();
            state.binding_offers.clear();
            state.peers.clear_pins();
            state.peers.remove_all();
            state.prekeys = prekeys::PrekeyState::default();
        });
        for key in [
            chatter_mesh_store::storage::StorageKey::PeerPins,
            chatter_mesh_store::storage::StorageKey::LocalPrekeys,
        ] {
            if let Err(error) = self.storage.wipe(key) {
                fail(error.into(), &mut first_error);
            }
        }

        if let Err(error) = self.router.send(RouterCommand::Wipe) {
            fail(error, &mut first_error);
        }
        if let Err(error) = self.timeline.wipe() {
            fail(error.into(), &mut first_error);
        }
        if let Err(error) = self.identity_store.wipe() {
            fail(error.into(), &mut first_error);
        }
        if let Err(error) = self.binding_store.wipe_all() {
            fail(error.into(), &mut first_error);
        }
        if let Err(error) = self.private_channels.wipe() {
            fail(error, &mut first_error);
        }
        if let Err(error) = self.key_store.delete_mesh_sessions(None) {
            fail(error.into(), &mut first_error);
        }

        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    // ----------------------------------------------------------------- helpers

    /// Whether any transport could carry a DM to this peer.
    pub(super) fn can_route_to(&self, peer: &PeerId) -> bool {
        if peer.is_empty() {
            return false;
        }
        // Nostr peers are addressed by their gift-wrap pubkey; mesh peers by
        // their routing ID. A peer that is neither cannot be reached at all, and
        // saying so up front beats queueing a message nothing will ever pick up.
        if self.nostr_state.knows_peer(peer) {
            return self.relay_connector.is_some();
        }
        self.has_radio && peer.to_routing_bytes().is_some()
    }

    pub(super) fn nickname_for(&self, peer: &PeerId) -> String {
        self.state
            .with(|state| state.peers.nickname(peer, false).map(str::to_string))
            .or_else(|| self.nostr_state.nickname(peer))
            .unwrap_or_else(|| peer.as_str().to_string())
    }

    /// Empty bodies never reach the wire: they cost airtime, render as nothing,
    /// and are indistinguishable from a decode failure on the far side.
    pub(super) fn require_content(text: &str) -> Result<String, MeshError> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Err(MeshError::EmptyContent);
        }
        Ok(trimmed.to_string())
    }
}

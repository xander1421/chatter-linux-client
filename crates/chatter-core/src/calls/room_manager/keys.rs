use super::*;

impl RoomManager {
    /// Ratchet the encryption key for forward secrecy.
    ///
    /// This advances the key to a new epoch, providing forward secrecy
    /// so that compromise of a later key doesn't reveal earlier messages.
    /// Should be called periodically during long calls.
    ///
    /// Returns the new key epoch, or None if not connected.
    pub fn ratchet_key(&self) -> Option<u32> {
        let mut e2ee = self.e2ee.write();
        if let Some(ref key_provider) = e2ee.provider {
            if let Some(_new_key) = key_provider.ratchet_shared_key(0) {
                e2ee.epoch += 1;
                tracing::info!(epoch = e2ee.epoch, "E2EE key ratcheted");
                Some(e2ee.epoch)
            } else {
                tracing::warn!("Key ratchet failed");
                None
            }
        } else {
            tracing::warn!("Cannot ratchet key: not connected");
            None
        }
    }

    /// Get the current key epoch.
    pub fn key_epoch(&self) -> u32 {
        self.e2ee.read().epoch
    }

    /// Test-only: the current epoch and every slot of the shared-key ring, as
    /// LiveKit's `KeyProvider` returns them. `None` without a key provider.
    #[cfg(feature = "testing")]
    pub fn installed_key_ring(&self) -> Option<(u32, Vec<Option<Vec<u8>>>)> {
        let e2ee = self.e2ee.read();
        let provider = e2ee.provider.as_ref()?;
        Some((
            e2ee.epoch,
            (0..KEY_RING_SIZE)
                .map(|i| provider.get_shared_key(i))
                .collect(),
        ))
    }

    /// Test-only: how many tracks we publish to the room. 0 when not connected.
    #[cfg(feature = "testing")]
    pub fn local_track_count(&self) -> usize {
        self.room
            .read()
            .as_ref()
            .map_or(0, |r| r.local_participant().track_publications().len())
    }

    /// Install an externally-ratcheted shared key for `epoch`, pre-installing
    /// `epoch + 1` so a peer whose 30 s ratchet timer fires slightly ahead can
    /// still decrypt immediately.
    ///
    /// Both sides advance the same deterministic [`CallKeyState`] chain at the
    /// same 30 s cadence, so they reach the same epoch (= key index). The two
    /// directions of small timing skew are absorbed without LiveKit's own
    /// ratchet (which we disabled): a peer slightly *ahead* is covered by the
    /// pre-installed `epoch + 1`; a peer slightly *behind* by the 16-slot key
    /// ring still holding older epochs. The SDK manager installs and selects
    /// the current sending epoch before preinstalling the future receive key,
    /// serialized with new sender creation. Receive preinstallation does not
    /// change the selected media or data sending key.
    pub fn set_ratcheted_shared_key(
        &self,
        epoch: u32,
        key_cur: Vec<u8>,
        key_next: Vec<u8>,
    ) -> bool {
        let mut e2ee = self.e2ee.write();
        match e2ee.provider.as_ref() {
            Some(_) => {}
            None => {
                tracing::warn!("Cannot ratchet key: no key provider");
                return false;
            }
        };
        let room = self.room.read();
        let Some(room) = room.as_ref() else {
            return false;
        };
        if !room.e2ee_manager().install_shared_media_key(
            key_ring_index(epoch),
            key_cur,
            Some((key_ring_index(epoch.wrapping_add(1)), key_next)),
        ) {
            return false;
        }
        e2ee.epoch = epoch;
        tracing::info!(
            epoch,
            sender_index = key_ring_index(epoch),
            "Media encryption epoch selected"
        );
        true
    }

    /// Install `key` as the shared key at index 0 and reset the epoch counter.
    ///
    /// Used on the DM caller side when the real ECDH-derived key arrives after
    /// a placeholder connect: the callee connected *with* this same key at
    /// index 0, so re-installing it at index 0 (and resetting the epoch) puts
    /// both peers on the same key and the same epoch baseline, so the periodic
    /// ratchet then advances them in lockstep.
    pub fn reset_shared_key(&self, key: Vec<u8>) -> bool {
        let key = zeroize::Zeroizing::new(key);
        let mut e2ee = self.e2ee.write();
        match e2ee.provider.as_ref() {
            Some(_) => {}
            None => {
                tracing::warn!("Cannot reset key: no key provider");
                return false;
            }
        };
        let room = self.room.read();
        let Some(room) = room.as_ref() else {
            return false;
        };
        if !room
            .e2ee_manager()
            .install_shared_media_key(0, key.to_vec(), None)
        {
            return false;
        }
        e2ee.epoch = 0;
        tracing::info!(
            epoch = 0u32,
            sender_index = 0i32,
            "Media encryption epoch selected"
        );
        tracing::info!("E2EE key re-rooted at index 0 (real DM key)");
        true
    }
}

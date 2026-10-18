use super::codec::{decode, encode};
use super::*;

impl OutboxStore {
    /// Read the durable outbox and merge it into memory.
    ///
    /// Safe and useful to call more than once. The first call is the cold load; a
    /// later one is the recovery pass after storage becomes readable, and it merges
    /// rather than replaces so nothing accepted in between is lost. A pending write
    /// that was deferred because the store had not loaded is flushed here.
    ///
    /// Returns the overflow drops the merge caused, which the caller must surface.
    pub fn load(&mut self) -> Result<Vec<DroppedMessage>, StoreError> {
        let sealed = self.storage.load(StorageKey::Outbox)?;

        let dropped = match sealed {
            // Nothing durable — a first run, or a run after a panic wipe.
            None => Vec::new(),
            Some(sealed) => match self.sealer.open(&sealed) {
                Ok(plaintext) => self.merge_recovered(decode(&plaintext)?),
                // Unrecoverable: the blob can never be read, so treat it as absent
                // and let the next write replace it.
                Err(StoreError::Malformed(_)) => Vec::new(),
                // Anything else means "not readable yet". Stay un-loaded so we
                // never overwrite mail we have not seen.
                Err(error) => return Err(error),
            },
        };

        self.loaded = true;
        if self.dirty {
            self.persist()?;
        }
        Ok(dropped)
    }

    /// Write the outbox, sealed.
    ///
    /// A no-op when nothing changed. Deliberately also a no-op while the store has
    /// not loaded: writing then would overwrite a durable queue that has never been
    /// read. The pending write happens on the next successful [`load`](Self::load).
    pub fn persist(&mut self) -> Result<(), StoreError> {
        if !self.dirty {
            return Ok(());
        }
        if !self.loaded {
            // Stay dirty; the write is owed, not lost.
            return Ok(());
        }

        if self.outbox.is_empty() {
            self.storage.wipe(StorageKey::Outbox)?;
        } else {
            let plaintext = encode(&self.outbox)?;
            let sealed = self.sealer.seal(&plaintext)?;
            self.storage.save(StorageKey::Outbox, &sealed)?;
        }
        self.dirty = false;
        Ok(())
    }

    /// Panic wipe: forget the queued mail in memory and on disk.
    ///
    /// The ack memory goes too — it exists only to protect the queue it describes,
    /// and keeping a list of recently acked message IDs after a wipe would be
    /// exactly the kind of residue the wipe is for.
    pub fn wipe(&mut self) -> Result<(), StoreError> {
        self.outbox.clear();
        self.acks.clear();
        self.dirty = false;
        // A wipe leaves nothing to recover, so the store is trivially "loaded".
        self.loaded = true;
        self.storage.wipe(StorageKey::Outbox)
    }
}

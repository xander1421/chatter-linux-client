use super::*;

/// Wire tag for the serialized outbox, so a blob from another store is rejected as
/// malformed rather than decoded into nonsense.
const MAGIC: [u8; 3] = *b"OBX";
const FORMAT_VERSION: u8 = 1;
fn encode_error(field: &'static str) -> StoreError {
    StoreError::Storage {
        operation: "encode",
        key: StorageKey::Outbox.as_str(),
        reason: format!("{field} exceeds the encodable length"),
    }
}

fn put_u16_len(out: &mut Vec<u8>, len: usize, field: &'static str) -> Result<(), StoreError> {
    let len = u16::try_from(len).map_err(|_| encode_error(field))?;
    out.extend_from_slice(&len.to_be_bytes());
    Ok(())
}

fn put_u32_len(out: &mut Vec<u8>, len: usize, field: &'static str) -> Result<(), StoreError> {
    let len = u32::try_from(len).map_err(|_| encode_error(field))?;
    out.extend_from_slice(&len.to_be_bytes());
    Ok(())
}

fn put_short_bytes(out: &mut Vec<u8>, bytes: &[u8], field: &'static str) -> Result<(), StoreError> {
    put_u16_len(out, bytes.len(), field)?;
    out.extend_from_slice(bytes);
    Ok(())
}

fn put_long_bytes(out: &mut Vec<u8>, bytes: &[u8], field: &'static str) -> Result<(), StoreError> {
    put_u32_len(out, bytes.len(), field)?;
    out.extend_from_slice(bytes);
    Ok(())
}

/// Serialize the outbox. The result is plaintext and must be sealed before it
/// reaches [`Storage`] — [`OutboxStore::persist`] is the only caller, and it does.
pub(super) fn encode(snapshot: &Snapshot) -> Result<Vec<u8>, StoreError> {
    let mut out = Vec::new();
    out.extend_from_slice(&MAGIC);
    out.push(FORMAT_VERSION);
    put_u32_len(&mut out, snapshot.len(), "peer count")?;

    for (peer, queue) in snapshot {
        put_short_bytes(&mut out, peer.as_str().as_bytes(), "peer id")?;
        put_u32_len(&mut out, queue.len(), "queue length")?;
        for message in queue {
            put_long_bytes(&mut out, message.content.as_bytes(), "content")?;
            put_short_bytes(&mut out, message.nickname.as_bytes(), "nickname")?;
            put_short_bytes(&mut out, message.message_id.as_bytes(), "message id")?;
            out.extend_from_slice(&message.timestamp.to_be_bytes());
            out.extend_from_slice(&message.send_attempts.to_be_bytes());
            put_u16_len(
                &mut out,
                message.deposited_courier_keys.len(),
                "courier key count",
            )?;
            for key in &message.deposited_courier_keys {
                out.extend_from_slice(key);
            }
        }
    }
    Ok(out)
}

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], StoreError> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or(StoreError::Malformed("outbox"))?;
        let slice = self
            .bytes
            .get(self.pos..end)
            .ok_or(StoreError::Malformed("outbox"))?;
        self.pos = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, StoreError> {
        self.take(1)?
            .first()
            .copied()
            .ok_or(StoreError::Malformed("outbox"))
    }

    fn u16(&mut self) -> Result<u16, StoreError> {
        let raw: [u8; 2] = self
            .take(2)?
            .try_into()
            .map_err(|_| StoreError::Malformed("outbox"))?;
        Ok(u16::from_be_bytes(raw))
    }

    fn u32(&mut self) -> Result<u32, StoreError> {
        let raw: [u8; 4] = self
            .take(4)?
            .try_into()
            .map_err(|_| StoreError::Malformed("outbox"))?;
        Ok(u32::from_be_bytes(raw))
    }

    fn u64(&mut self) -> Result<u64, StoreError> {
        let raw: [u8; 8] = self
            .take(8)?
            .try_into()
            .map_err(|_| StoreError::Malformed("outbox"))?;
        Ok(u64::from_be_bytes(raw))
    }

    fn key32(&mut self) -> Result<[u8; 32], StoreError> {
        self.take(32)?
            .try_into()
            .map_err(|_| StoreError::Malformed("outbox"))
    }

    fn short_string(&mut self) -> Result<String, StoreError> {
        let len = usize::from(self.u16()?);
        let bytes = self.take(len)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| StoreError::Malformed("outbox"))
    }

    fn long_string(&mut self) -> Result<String, StoreError> {
        let len = usize::try_from(self.u32()?).map_err(|_| StoreError::Malformed("outbox"))?;
        let bytes = self.take(len)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| StoreError::Malformed("outbox"))
    }
}

/// Parse bytes produced by [`encode`]. Counts are attacker-influenced in principle,
/// so nothing is pre-allocated from them: a truncated blob fails on the next read
/// instead of reserving gigabytes first.
pub(super) fn decode(bytes: &[u8]) -> Result<Snapshot, StoreError> {
    let mut reader = Reader::new(bytes);
    if reader.take(MAGIC.len())? != MAGIC.as_slice() {
        return Err(StoreError::Malformed("outbox"));
    }
    if reader.u8()? != FORMAT_VERSION {
        return Err(StoreError::Malformed("outbox"));
    }

    let peer_count = usize::try_from(reader.u32()?).map_err(|_| StoreError::Malformed("outbox"))?;
    let mut snapshot = Snapshot::new();

    for _ in 0..peer_count {
        let peer = PeerId::from_str(&reader.short_string()?);
        let queue_len =
            usize::try_from(reader.u32()?).map_err(|_| StoreError::Malformed("outbox"))?;
        let mut queue = Vec::new();

        for _ in 0..queue_len {
            let content = reader.long_string()?;
            let nickname = reader.short_string()?;
            let message_id = reader.short_string()?;
            let timestamp = reader.u64()?;
            let send_attempts = reader.u32()?;
            let key_count = usize::from(reader.u16()?);
            let mut deposited_courier_keys = BTreeSet::new();
            for _ in 0..key_count {
                deposited_courier_keys.insert(reader.key32()?);
            }
            queue.push(QueuedMessage {
                content,
                nickname,
                message_id,
                timestamp,
                send_attempts,
                deposited_courier_keys,
            });
        }

        if !queue.is_empty() {
            snapshot.insert(peer, queue);
        }
    }

    Ok(snapshot)
}

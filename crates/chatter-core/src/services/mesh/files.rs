use super::*;

impl MeshService {
    /// Put a file on the air for the public mesh room.
    ///
    /// Returns the wire message ID; the row itself arrives on the event stream as
    /// a [`MeshEvent::Message`] whose content is
    /// [`Mixed`](crate::domain::message::MessageContent::Mixed), one path for
    /// local and remote files just as [`send_room`](Self::send_room) is for text.
    ///
    /// # Room only, and chunked above one packet's worth
    ///
    /// A mesh file transfer is a cleartext broadcast, so there is no directed
    /// form to offer: this client's DMs are Noise-encrypted, and
    /// `on_file_transfer` refuses a directed file for that reason.
    ///
    /// The *size* decides the protocol, and it is decided here because this is
    /// where the file's length is first known:
    ///
    /// - At or below [`MAX_SINGLE_PACKET_FILE_BYTES`] the file rides one
    ///   `FileTransfer` packet, exactly as P8 shipped it and exactly as real
    ///   bitchat speaks. Interop must not regress for the common case.
    /// - Above it the file is *chunked* — see [`send_file_streamed`] — because a
    ///   single packet above that size cannot be reassembled inside
    ///   `FRAGMENT_LIFETIME_SECS` at any goodput this radio achieves. That is not
    ///   an optimisation: the single-packet path cannot deliver a large file at
    ///   all, whatever the sender does.
    /// - Above [`MAX_STREAMED_FILE_BYTES`] it is refused, and the ceiling is
    ///   *time* rather than memory now that nothing buffers the whole file.
    ///
    /// [`send_file_streamed`]: Self::send_file_streamed
    pub fn send_file(&self, path: &Path) -> Result<String, MeshError> {
        if !self.has_radio {
            return Err(MeshError::NoRadio);
        }
        // Against the file's own length first, so a multi-gigabyte pick is
        // refused without being read into memory.
        let declared = std::fs::metadata(path)
            .map_err(|source| MeshError::FileIo {
                path: path.display().to_string(),
                source,
            })?
            .len();
        if declared > MAX_STREAMED_FILE_BYTES as u64 {
            return Err(MeshError::FileTooLarge {
                size: declared,
                max: MAX_STREAMED_FILE_BYTES,
            });
        }
        if declared > MAX_SINGLE_PACKET_FILE_BYTES as u64 {
            return self.send_file_streamed(path, declared);
        }
        let content = std::fs::read(path).map_err(|source| MeshError::FileIo {
            path: path.display().to_string(),
            source,
        })?;
        // Re-checked against what was actually read: the file can grow between
        // the two calls, and the limit is the receiver's decode ceiling, so one
        // byte over it is a packet nobody will accept.
        if content.len() > MAX_SINGLE_PACKET_FILE_BYTES {
            return Err(MeshError::FileTooLarge {
                size: content.len() as u64,
                max: MAX_SINGLE_PACKET_FILE_BYTES,
            });
        }
        if content.is_empty() {
            // `BitchatFilePacket::decode` rejects an empty content TLV outright,
            // so an empty file is airtime spent on a packet no peer can accept.
            return Err(MeshError::EmptyContent);
        }

        // The name is the leaf, not the path: the rest is this device's directory
        // layout and is nobody else's business.
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_string);
        let mime = mime_guess::from_path(path)
            .first_or_octet_stream()
            .to_string();
        // `file_size: None` lets `encode` write the content length, so the
        // declared size can never disagree with the bytes that follow it.
        let file = BitchatFilePacket::new(file_name, None, Some(mime), content);

        let message_id = new_message_id();
        self.ble
            .send(BleCommand::FileTransfer {
                message_id: message_id.clone(),
                file,
            })
            .map_err(|_| MeshError::WorkerGone)?;
        // A broadcast, exactly as `send_room`: nobody acknowledges it, so this is
        // the whole of its delivery history. Leaving it unrecorded would make the
        // row render as an ordinary one-to-one "sent" — see `outbound_message`.
        self.timeline
            .record_delivery(&message_id, MeshDelivery::Broadcast);
        Ok(message_id)
    }

    /// Put a large file on the air as a chunked, resumable transfer.
    ///
    /// Reached from [`send_file`](Self::send_file) by size, never called
    /// directly by a UI: which protocol a file rides is a property of the file,
    /// not a choice a surface should be making.
    ///
    /// The file is not read into memory here either. It is hashed in windows —
    /// which is unavoidable, because the manifest's whole-file digest is the only
    /// thing that will tell the receiver its assembled file is the right one — and
    /// then the *path* is handed to the runtime, which reads one chunk at a time.
    ///
    /// # Why the transfer id is derived rather than random
    ///
    /// `SHA-256(local peer ‖ file digest)[..8]`. Deterministic, which buys two
    /// things a random id would not:
    ///
    /// - **Resume across a re-send.** A transfer interrupted at 80%, re-sent an
    ///   hour later, keys onto the same spool entry on the receiver and finishes
    ///   the remaining 20% instead of starting over.
    /// - **No cross-sender collision.** Two peers sending the same file get
    ///   different ids, so neither can write into the other's transfer — which the
    ///   per-chunk sender check would refuse, but only after wasting the airtime.
    fn send_file_streamed(&self, path: &Path, declared: u64) -> Result<String, MeshError> {
        use chatter_mesh::tlv::file_stream::{CHUNK_BYTES, FileManifest};
        use sha2::{Digest, Sha256};

        if declared == 0 {
            return Err(MeshError::EmptyContent);
        }

        // O(256 KiB) regardless of the file, like everything else on this path.
        let digest = file_stream::digest_file(path)?;

        let transfer_id: [u8; 8] = {
            let mut hasher = Sha256::new();
            hasher.update(self.identity.peer_id().as_str().as_bytes());
            hasher.update(digest);
            let full: [u8; 32] = hasher.finalize().into();
            let mut id = [0u8; 8];
            id.copy_from_slice(&full[..8]);
            id
        };

        // The leaf name only: the rest is this device's directory layout.
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_string);
        let mime = mime_guess::from_path(path)
            .first_or_octet_stream()
            .to_string();

        let manifest = FileManifest::new(
            transfer_id,
            file_name,
            Some(mime),
            declared,
            CHUNK_BYTES as u32,
            digest,
        )?;

        let message_id = new_message_id();
        self.ble
            .send(BleCommand::StreamedFile {
                message_id: message_id.clone(),
                manifest,
                path: path.to_path_buf(),
            })
            .map_err(|_| MeshError::WorkerGone)?;
        // A broadcast like any other room file: nobody acknowledges it. That is
        // true even though this one takes minutes — progress is not delivery, and
        // there is no acknowledgement to promote it with.
        self.timeline
            .record_delivery(&message_id, MeshDelivery::Broadcast);
        Ok(message_id)
    }

    /// How long a file would take to put on the air, in seconds, at the two ends
    /// of the goodput measured on this hardware.
    ///
    /// Exposed because the honest answer for a large file is "twenty minutes",
    /// and a surface that starts a twenty-minute transfer without saying so is
    /// promising something the radio cannot deliver quickly — the same class of
    /// claim P7 removed from the public room. The UI decides how to present it;
    /// this decides what the numbers are.
    #[must_use]
    pub fn file_transfer_estimate_secs(&self, size_bytes: u64) -> (u64, u64) {
        use chatter_mesh::tlv::file_stream::{
            CHUNK_BYTES, MAX_SINGLE_PACKET_FILE_BYTES, OPTIMISTIC_GOODPUT_BYTES_PER_SEC,
            PESSIMISTIC_GOODPUT_BYTES_PER_SEC, estimated_seconds,
        };
        // A single-packet file is one "chunk" of its own size, which is what
        // makes the estimate continuous across the size fork rather than jumping
        // at the boundary.
        let chunk = if size_bytes > MAX_SINGLE_PACKET_FILE_BYTES as u64 {
            CHUNK_BYTES as u32
        } else {
            size_bytes.max(1) as u32
        };
        (
            estimated_seconds(size_bytes, chunk, OPTIMISTIC_GOODPUT_BYTES_PER_SEC),
            estimated_seconds(size_bytes, chunk, PESSIMISTIC_GOODPUT_BYTES_PER_SEC),
        )
    }
}

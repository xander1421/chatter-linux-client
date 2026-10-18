use super::*;

impl BleRuntime {
    /// Put a file on the air for the mesh room.
    ///
    /// The counterpart of [`broadcast_public`](Self::broadcast_public), and
    /// deliberately shaped the same way: build, sign, remember as our own,
    /// transmit, then mirror locally — because the mesh has no server echo, so
    /// the local copy *is* the message.
    pub async fn broadcast_file(&mut self, message_id: &str, file: &BitchatFilePacket) {
        let now_ms = now_millis();
        let nickname = self.config.nickname();
        let payload = match file.encode() {
            Ok(payload) => payload,
            Err(error) => {
                // Only reachable above `MAX_PAYLOAD_BYTES`, which the service
                // already refused, so this is a bug rather than a user error —
                // and returning quietly would leave the user's row holding a
                // delivery state for a packet that never existed.
                tracing::error!(
                    target: "mesh::ble",
                    %error,
                    bytes = file.content.len(),
                    "a file transfer could not be encoded; nothing was sent"
                );
                return;
            }
        };
        let packet = self.build_packet(
            MessageType::FileTransfer,
            None,
            payload,
            chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
        );
        // Signed, because the receiver requires it: `on_file_transfer` drops an
        // unsigned broadcast before it looks at the content, exactly as the
        // public-message path does.
        let packet = self.maybe_sign(packet, true);
        // Remember it, so the copy that floods back is recognised as ours rather
        // than stored a second time under a new attachment id.
        self.state
            .with(|state| state.self_broadcasts.record(message_id, &packet, now_ms));
        self.originate_broadcast(&packet).await;

        // Our own copy, stored the same way an inbound one is. Without it the row
        // would name an attachment that does not exist, and the sender would be
        // the only person on the mesh who cannot open what they just sent.
        let attachment = match self.events.store_file(
            file.file_name.as_deref(),
            file.mime_type.as_deref(),
            &file.content,
        ) {
            Ok(attachment) => attachment,
            Err(error) => {
                // The packet is already away, so the file did reach the room.
                // Only our own record of it is missing, and saying so is better
                // than a row pointing at nothing.
                tracing::error!(
                    target: "mesh::ble",
                    %error,
                    "a sent mesh file could not be stored locally"
                );
                self.degraded(format!(
                    "the file was sent, but this device could not keep a copy: {error}"
                ));
                return;
            }
        };

        let local = self.identity.peer_id();
        self.events.send(MeshEvent::Message(Box::new(MeshMessage {
            message: mapping::outbound_file_message(
                &MeshConversation::Room,
                message_id,
                &local,
                attachment,
                now_ms,
            ),
            conversation: MeshConversation::Room,
            sender: local,
            nickname,
            is_local: true,
            sender_verified: true,
        })));
    }

    /// Begin a chunked transfer: manifest on the air, local copy stored, chunks
    /// paced out by the tick.
    ///
    /// Shaped like [`broadcast_file`](Self::broadcast_file) — build, sign,
    /// remember as our own, transmit, mirror locally — with one difference that
    /// is the whole of P9 on the send side: **the file is never read whole.** The
    /// local mirror goes through the streaming encryptor straight from the
    /// source path, and the chunks are read one window at a time from
    /// [`on_tick`](Self::on_tick).
    pub async fn start_streamed_file(
        &mut self,
        message_id: &str,
        manifest: FileManifest,
        path: std::path::PathBuf,
    ) {
        let now_ms = now_millis();
        let now_s = now_secs();
        let nickname = self.config.nickname();
        let transfer_id = manifest.transfer_id;

        // Our own copy first, and a failure here aborts the send.
        //
        // The opposite of `broadcast_file`, which puts the packet on the air and
        // then stores — because there it is one packet and by the time storing
        // fails the file has already reached the room. Here the transfer takes
        // minutes, so failing before the first byte costs nothing and avoids a
        // sender streaming out a file it cannot itself open.
        let attachment = match self.events.store_file_streamed(
            manifest.file_name.as_deref(),
            manifest.mime_type.as_deref(),
            &path,
        ) {
            Ok(attachment) => attachment,
            Err(error) => {
                tracing::error!(
                    target: "mesh::ble",
                    %error,
                    path = %path.display(),
                    "a file could not be stored locally; nothing was streamed"
                );
                self.degraded(format!("that file could not be read or saved: {error}"));
                return;
            }
        };

        let transfer = OutboundTransfer::new(manifest, path, message_id.to_string(), now_s);
        // Before `files_out.insert`, so a manifest that will not encode leaves no
        // transfer behind to stream chunks nobody can place.
        let Some(manifest_packet) = self.build_manifest_packet(&transfer.manifest) else {
            return;
        };
        // Remembered so the echo is recognised as ours. The chunks are recorded
        // the same way as they go out — see `pump_streamed_files`.
        self.state.with(|state| {
            state
                .self_broadcasts
                .record(message_id, &manifest_packet, now_ms)
        });
        self.originate_broadcast(&manifest_packet).await;

        tracing::info!(
            target: "mesh::ble",
            transfer = %hex_encode(&transfer_id),
            bytes = transfer.manifest.total_size,
            chunks = transfer.manifest.chunk_count,
            "streaming a file into the mesh room"
        );
        self.files_out.insert(transfer_id, transfer);

        let local = self.identity.peer_id();
        self.events.send(MeshEvent::Message(Box::new(MeshMessage {
            message: mapping::outbound_file_message(
                &MeshConversation::Room,
                message_id,
                &local,
                attachment,
                now_ms,
            ),
            conversation: MeshConversation::Room,
            sender: local,
            nickname,
            is_local: true,
            sender_verified: true,
        })));
    }

    /// The manifest as a signed packet, or `None` if it cannot be encoded.
    ///
    /// `unwrap_or_default` here put an *empty* payload on the air under a
    /// `FileManifest` type. The receiver drops what it cannot decode, so the
    /// sender would go on to stream every chunk of a transfer the far side had
    /// never been told about — a stall with no error on either device.
    ///
    /// `encode` fails on `validate` (a size or chunk size out of range) or a TLV
    /// field beyond 64 KiB. Both should be unreachable, since `mesh_send_file`
    /// applies the cap before a manifest is ever built. Unreachable is the reason
    /// to return the error rather than paper over it: if it does happen, the
    /// invented value hides the cause exactly where nothing else records it.
    fn build_manifest_packet(&self, manifest: &FileManifest) -> Option<BitchatPacket> {
        let payload = match manifest.encode() {
            Ok(payload) => payload,
            Err(error) => {
                tracing::warn!(
                    target: "mesh::ble",
                    transfer = %hex_encode(&manifest.transfer_id),
                    %error,
                    "file manifest could not be encoded; not sent"
                );
                return None;
            }
        };
        let packet = self.build_packet(
            MessageType::FileManifest,
            None,
            payload,
            chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
        );
        // Signed, because the receiver requires it: an unsigned manifest is
        // dropped before its payload is looked at, exactly as an unsigned file
        // transfer is.
        Some(self.maybe_sign(packet, true))
    }

    /// Put the next chunk of each live transfer on the air, and repeat manifests.
    ///
    /// Called from the tick, one chunk at a time, rather than looping over the
    /// whole file: the loop is also the only thing servicing link events, and a
    /// 16 MiB transfer emitted in one pass would leave the mesh deaf for minutes.
    pub(super) async fn pump_streamed_files(&mut self, now_s: u64) {
        if self.files_out.is_empty() {
            return;
        }

        // Backpressure the loop can see. A chunk is the lowest-priority thing in
        // the write buffer, so generating chunks into a full buffer mostly evicts
        // earlier chunks of the same transfer.
        let queued = self.state.with(|state| state.writes.total_queued_bytes());
        let congested = queued > CHUNK_PACING_QUEUE_BYTES;

        let ids: Vec<TransferId> = self.files_out.keys().copied().collect();
        for id in ids {
            // A manifest again, whether or not the queue is busy: it is a few
            // hundred bytes, and a peer that arrived mid-transfer cannot use a
            // single chunk without one.
            let due = self.files_out.get(&id).is_some_and(|t| {
                now_s.saturating_sub(t.last_manifest_secs) >= MANIFEST_REPEAT_SECS
            });
            if due {
                let manifest = {
                    let Some(transfer) = self.files_out.get_mut(&id) else {
                        continue;
                    };
                    transfer.last_manifest_secs = now_s;
                    transfer.manifest.clone()
                };
                let Some(packet) = self.build_manifest_packet(&manifest) else {
                    continue;
                };
                self.state.with(|state| {
                    state
                        .self_broadcasts
                        .record("mesh-manifest-repeat", &packet, now_millis())
                });
                self.originate_broadcast(&packet).await;
            }

            if congested {
                tracing::debug!(
                    target: "mesh::ble",
                    transfer = %hex_encode(&id),
                    queued,
                    cap = CHUNK_PACING_QUEUE_BYTES,
                    "chunk turn skipped: the write queue is backed up"
                );
                continue;
            }

            for _ in 0..CHUNKS_PER_TICK {
                let Some(transfer) = self.files_out.get_mut(&id) else {
                    break;
                };
                let Some(index) = transfer.next_chunk() else {
                    break;
                };
                let bytes = match transfer.read_chunk(index) {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        // The user's file moved or became unreadable mid-transfer.
                        // Nothing recovers from that, and continuing would stream
                        // a file that no longer exists.
                        tracing::error!(
                            target: "mesh::ble",
                            %error,
                            transfer = %hex_encode(&id),
                            index,
                            "a chunk could not be read; abandoning the transfer"
                        );
                        self.files_out.remove(&id);
                        self.degraded("a file being sent became unreadable".to_string());
                        break;
                    }
                };
                let Ok(payload) = FileChunk::new(id, index, bytes).and_then(|c| c.encode()) else {
                    continue;
                };
                // Hold the next chunk back for as long as this one needs on the
                // air. Stamped from the encoded length, so a short final chunk
                // waits proportionally less, and set before the write rather than
                // after so a slow write does not extend the interval twice.

                let packet = self.build_packet(
                    MessageType::FileChunk,
                    None,
                    payload,
                    chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
                );
                // Signed individually. This is the point the design is emphatic
                // about: the manifest's signature says nothing about the chunks,
                // and an unsigned chunk is refused by the receiver.
                let packet = self.maybe_sign(packet, true);
                self.state.with(|state| {
                    state
                        .self_broadcasts
                        .record("mesh-file-chunk", &packet, now_millis())
                });
                self.originate_broadcast(&packet).await;
            }
        }

        // A drained transfer is kept, not dropped, for as long as a receiver
        // might still ask: there is no acknowledgement, so "drained" only means
        // we have nothing left to offer unprompted.
        let expired: Vec<TransferId> = self
            .files_out
            .iter()
            .filter(|(_, transfer)| {
                transfer.is_drained()
                    && now_s.saturating_sub(transfer.started_secs) > TRANSFER_ABANDON_SECS
            })
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            tracing::debug!(
                target: "mesh::ble",
                transfer = %hex_encode(&id),
                "closing a finished outbound transfer; no more retransmit requests will be served"
            );
            self.files_out.remove(&id);
        }
    }

    /// Ask for the chunks of a stalled transfer.
    ///
    /// The receiver-driven half of retransmission, and the direct analogue of
    /// `GossipSyncManager::request_missing_fragments` one layer down — including
    /// the truncation: [`FileChunkRequest`] names at most
    /// [`MAX_REQUESTED_CHUNKS`], so a receiver missing hundreds asks for the
    /// earliest now and the rest next round rather than emitting a request that
    /// would have to be fragmented to fit.
    pub(super) async fn request_missing_chunks(&mut self, now_s: u64) {
        let stalled = self
            .files_in
            .stalled(now_s, CHUNK_REQUEST_QUIET_SECS, MAX_REQUESTED_CHUNKS);
        for (transfer_id, missing) in stalled {
            let Ok(request) = FileChunkRequest::new(transfer_id, missing) else {
                continue;
            };
            let Ok(payload) = request.encode() else {
                continue;
            };
            let packet = self.build_packet(
                MessageType::FileChunkRequest,
                None,
                payload,
                chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
            );
            // Signed: the sender refuses an unauthenticated request, because
            // serving one costs airtime and would otherwise be an amplification
            // lever.
            let packet = self.maybe_sign(packet, true);
            self.state.with(|state| {
                state
                    .self_broadcasts
                    .record("mesh-chunk-request", &packet, now_millis())
            });
            tracing::info!(
                target: "mesh::ble",
                transfer = %hex_encode(&transfer_id),
                chunks = request.indices.len(),
                "asking for chunks that never arrived"
            );
            self.originate_broadcast(&packet).await;
        }
    }
}

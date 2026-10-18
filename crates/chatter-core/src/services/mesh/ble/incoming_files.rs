use super::*;

impl BleRuntime {
    /// A file, image or voice note broadcast into the mesh room.
    ///
    /// Every gate below is [`on_public_message`](Self::on_public_message)'s, and
    /// that is deliberate rather than copied by accident: the two carry the same
    /// authentication problem. A broadcast's `sender_id` is a header field the
    /// sender chose, so registry membership is not proof of identity — only a
    /// signature that verifies against the key an announce established is.
    /// bitchat reaches the same conclusion in the same words and for a concrete
    /// reason: without it "a peer that observed a public voice burst could spoof
    /// a broadcast `voice_<burstID>.m4a` note under the talker's ID and overwrite
    /// the signature-verified live bubble with attacker audio"
    /// (`BLEFileTransferHandler.swift`, `resolveSenderNickname`).
    ///
    /// What is *not* ported is bitchat's MIME allowlist and magic-byte check
    /// (`BLEIncomingFileValidator`). Those exist to protect a decoder, and
    /// nothing here decodes: the bytes are encrypted and stored, never parsed.
    /// Whatever eventually renders them will need that check, and this is the
    /// layer to put it in.
    pub(super) fn on_file_transfer(&mut self, packet: &BitchatPacket, claimed: &PeerId) {
        // Directed file transfers are refused, where bitchat delivers one as a
        // private message (`BLEFileTransferPolicy.deliveryPlan`). A directed
        // `FileTransfer` is a file in the clear addressed to one peer, and this
        // client's private traffic is Noise-encrypted without exception —
        // accepting it would put an unencrypted file in a DM and teach the UI
        // that a DM can arrive unencrypted. `on_public_message` refuses the
        // legacy directed `Message` for exactly this reason. Relaying is
        // untouched: a peer that does implement directed files must not be
        // partitioned by our refusal.
        if !packet.is_broadcast() {
            return;
        }
        let Ok(file) = BitchatFilePacket::decode(&packet.payload) else {
            return;
        };

        let sender = claimed.clone();
        // A file we sent ourselves, recovered from a peer's gossip archive. The
        // ingress guard admits a self-authored TTL-0 sync response on purpose, so
        // this reaches us and has to be handled rather than dropped.
        let is_self = sender == self.identity.peer_id();
        // One of our own broadcasts flooded or synced straight back. We stored
        // and showed it when it was composed, so this copy is a duplicate — and
        // a worse one than a duplicate text message, because storing it again
        // would write a *second encrypted copy of the bytes* to disk under a new
        // attachment id. bitchat suppresses the same case as
        // `BLEFileTransferPolicy.isSelfEcho`.
        if is_self && self.is_own_echo(packet) {
            tracing::debug!(
                target: "mesh::ble",
                "our own file came back; already stored"
            );
            return;
        }
        let (signing_key, nickname, verified) = if is_self {
            // Verified against our own key, because we are deliberately not in
            // our own peer registry — see `on_announce`.
            (self.identity.signing_public(), self.config.nickname(), true)
        } else {
            let known = self.state.with(|state| {
                state.peers.info(&sender).map(|peer| {
                    (
                        peer.signing_public_key.clone(),
                        peer.nickname.clone(),
                        peer.verified_nickname,
                    )
                })
            });
            let Some((Some(key), nickname, verified)) = known else {
                // No announce from this peer, or one that carried no signing
                // key: there is nothing to check the signature against, so the
                // file is unattributable. bitchat drops it here too, as
                // "file transfer from unverified or unknown peer".
                tracing::debug!(
                    target: "mesh::ble",
                    peer = %sender,
                    "file transfer from an unknown or unverified peer dropped"
                );
                return;
            };
            let Ok(key) = <[u8; 32]>::try_from(key.as_slice()) else {
                return;
            };
            (key, nickname, verified)
        };
        if !verify_packet(packet, &signing_key) {
            tracing::debug!(
                target: "mesh::ble",
                peer = %sender,
                "file transfer with missing or invalid signature dropped",
            );
            return;
        }

        // No message ID on the wire, so it is derived from the packet — the same
        // derivation a public message uses, which is what collapses the copies
        // arriving by different paths onto one row.
        let wire_id = MessageDeduplicator::dedup_key(packet);
        // Already have it. The dedup set in `on_packet` catches the flood, but the
        // gossip archive exists to hand public packets back *after* that window
        // has closed — and a file cannot be stored idempotently the way a text
        // message can, because its bytes go to disk before the row that would
        // have been ignored. See `MeshTimeline::already_stored`.
        match self.events.already_stored(&wire_id) {
            Ok(true) => {
                tracing::debug!(
                    target: "mesh::ble",
                    peer = %sender,
                    "a file already on disk arrived again; not stored twice"
                );
                return;
            }
            Ok(false) => {}
            Err(error) => {
                // Store anyway rather than dropping the file over a read failure:
                // a duplicate copy is recoverable, a discarded one is not.
                tracing::warn!(
                    target: "mesh::ble",
                    %error,
                    "could not check whether this file is already stored"
                );
            }
        }

        // Stored before the event exists, because the row the event carries names
        // the attachment by id. A failure here is loud: the packet is off the air,
        // its dedup entry is already recorded, and nothing retransmits — so this
        // is a file the user will never be offered again.
        let attachment = match self.events.store_file(
            file.file_name.as_deref(),
            file.mime_type.as_deref(),
            &file.content,
        ) {
            Ok(attachment) => attachment,
            Err(error) => {
                tracing::error!(
                    target: "mesh::ble",
                    %error,
                    peer = %sender,
                    bytes = file.content.len(),
                    "an inbound mesh file could not be stored; it is lost"
                );
                self.degraded(format!(
                    "a file from {nickname} could not be saved: {error}"
                ));
                return;
            }
        };

        tracing::info!(
            target: "mesh::ble",
            peer = %sender,
            nickname = %nickname,
            bytes = file.content.len(),
            name = ?file.file_name,
            mime = ?file.mime_type,
            is_self,
            "file transfer received"
        );
        self.events.send(MeshEvent::Message(Box::new(MeshMessage {
            message: mapping::inbound_file_message(
                &MeshConversation::Room,
                &wire_id,
                &sender,
                attachment,
                packet.timestamp,
            ),
            conversation: MeshConversation::Room,
            sender,
            nickname,
            is_local: is_self,
            sender_verified: verified,
        })));
    }

    // ------------------------------------------------- chunked file transfer
    //
    // Three handlers, one authentication gate, and one rule that shapes all of
    // them: **a manifest signed once does not authenticate the chunks that
    // follow.** Skipping per-chunk verification would turn a transfer into an
    // unauthenticated write channel into a file the receiver is going to store
    // and show under the manifest sender's name. So every packet below goes
    // through [`authenticate_stream_packet`](Self::authenticate_stream_packet),
    // every time, and the spool separately checks that the verified sender is the
    // *same* peer whose manifest opened the file.

    /// Every gate P8 established, applied to one chunked-transfer packet.
    ///
    /// Deliberately a separate helper rather than a refactor of
    /// [`on_file_transfer`](Self::on_file_transfer): that path works, is tested
    /// against real bitchat, and has one gate this one does not — see the
    /// `is_self` case below.
    fn authenticate_stream_packet(
        &self,
        packet: &BitchatPacket,
        claimed: &PeerId,
    ) -> Option<(PeerId, String, bool)> {
        // Broadcast only, exactly as `on_file_transfer`: a directed cleartext
        // file has no business in a client whose every DM is Noise-sealed.
        if !packet.is_broadcast() {
            return None;
        }

        // Our own packet, coming back. Refused outright, where the single-packet
        // path has to *handle* a self-authored copy because the gossip archive
        // replays public `FileTransfer` packets and that is how a reinstalled
        // device recovers its own history.
        //
        // These three types have no sync bit at all (`SyncTypeFlags::bit_index`
        // returns `None` for each), so they are never replayed out of an archive
        // and a self-authored copy can only be our own flood echoing back. There
        // is nothing to recover and nothing to attribute: the local copy was
        // stored when the transfer started.
        if claimed == &self.identity.peer_id() {
            return None;
        }

        let (key, nickname, verified) = self.state.with(|state| {
            state.peers.info(claimed).map(|peer| {
                (
                    peer.signing_public_key.clone(),
                    peer.nickname.clone(),
                    peer.verified_nickname,
                )
            })
        })?;
        // No announce from this peer, or one with no signing key: there is
        // nothing to check a signature against, so the packet is unattributable.
        let key = <[u8; 32]>::try_from(key?.as_slice()).ok()?;
        if !verify_packet(packet, &key) {
            tracing::debug!(
                target: "mesh::ble",
                peer = %claimed,
                msg_type = packet.msg_type,
                "chunked transfer packet with missing or invalid signature dropped",
            );
            return None;
        }
        Some((claimed.clone(), nickname, verified))
    }

    /// A manifest: the start of a chunked transfer, or a repeat of one.
    pub(super) fn on_file_manifest(&mut self, packet: &BitchatPacket, claimed: &PeerId) {
        let Ok(manifest) = FileManifest::decode(&packet.payload) else {
            return;
        };
        let Some((sender, nickname, _verified)) = self.authenticate_stream_packet(packet, claimed)
        else {
            return;
        };

        // The row identity, fixed now and persisted with the bitmap. Derived from
        // the manifest packet the same way a text message's is, so the copies of
        // the *manifest* arriving by different paths collapse onto one row — and
        // so a transfer resumed after a restart lands on that same row rather
        // than a second one.
        let wire_id = MessageDeduplicator::dedup_key(packet);
        let transfer = hex_encode(&manifest.transfer_id);

        match self.files_in.begin(
            manifest,
            sender.clone(),
            wire_id,
            packet.timestamp,
            now_secs(),
        ) {
            Ok(ManifestOutcome::Started) => {
                tracing::info!(
                    target: "mesh::ble",
                    peer = %sender,
                    nickname = %nickname,
                    %transfer,
                    "a chunked file transfer started"
                );
            }
            Ok(ManifestOutcome::AlreadyComplete) => {
                tracing::debug!(
                    target: "mesh::ble",
                    peer = %sender,
                    "a manifest arrived for a transfer already finished; ignored"
                );
            }
            Ok(ManifestOutcome::Resumed { present, total }) => {
                tracing::info!(
                    target: "mesh::ble",
                    peer = %sender,
                    %transfer,
                    present,
                    total,
                    "resuming a chunked file transfer"
                );
            }
            // The common case: the manifest repeats on a cadence so a peer that
            // joins mid-transfer can pick it up.
            Ok(ManifestOutcome::Unchanged { .. }) => {}
            Err(error) => {
                tracing::warn!(
                    target: "mesh::ble",
                    %error,
                    peer = %sender,
                    %transfer,
                    "a chunked file transfer could not be started"
                );
            }
        }
    }

    /// One chunk, verified in its own right and written straight to disk.
    pub(super) fn on_file_chunk(&mut self, packet: &BitchatPacket, claimed: &PeerId) {
        // Both refusals below were silent, and a chunk refused silently is
        // indistinguishable from one that never arrived — which is precisely the
        // case that needs telling apart when a transfer stops progressing while
        // the sender insists it is serving chunks. Warn rather than debug: the
        // Android build logs at info, so debug here would still say nothing on the
        // device where it matters most.
        let Ok(chunk) = FileChunk::decode(&packet.payload) else {
            tracing::warn!(
                target: "mesh::ble",
                peer = %claimed,
                bytes = packet.payload.len(),
                "a file chunk arrived that could not be decoded"
            );
            return;
        };
        let Some((sender, nickname, _verified)) = self.authenticate_stream_packet(packet, claimed)
        else {
            tracing::warn!(
                target: "mesh::ble",
                peer = %claimed,
                transfer = %hex_encode(&chunk.transfer_id),
                index = chunk.index,
                "a file chunk was refused: unsigned, or from a peer with no \
                 announce to check it against"
            );
            return;
        };

        let outcome = self.files_in.accept_chunk(
            &chunk.transfer_id,
            chunk.index,
            &chunk.bytes,
            &sender,
            now_secs(),
        );
        let transfer = hex_encode(&chunk.transfer_id);
        match outcome {
            Ok(ChunkOutcome::Stored { present, total }) => {
                tracing::debug!(
                    target: "mesh::ble",
                    peer = %sender,
                    %transfer,
                    index = chunk.index,
                    present,
                    total,
                    "chunk stored"
                );
            }
            Ok(ChunkOutcome::Duplicate) => {}
            Ok(ChunkOutcome::Complete) => self.complete_transfer(&chunk.transfer_id, &nickname),
            // A chunk with no manifest is normal, not hostile: chunks and
            // manifests are separate broadcasts and the manifest repeats.
            Ok(ChunkOutcome::Unknown) => {
                tracing::debug!(
                    target: "mesh::ble",
                    peer = %sender,
                    %transfer,
                    "a chunk arrived before its manifest; waiting for one"
                );
            }
            Ok(ChunkOutcome::Refused(reason)) => {
                tracing::warn!(
                    target: "mesh::ble",
                    peer = %sender,
                    %transfer,
                    index = chunk.index,
                    reason,
                    "chunk refused"
                );
            }
            Err(error) => {
                tracing::error!(
                    target: "mesh::ble",
                    %error,
                    peer = %sender,
                    %transfer,
                    "a chunk could not be written; the transfer will stall"
                );
            }
        }
    }

    /// Verify, encrypt into place, and surface the row.
    ///
    /// Ordering here is the whole security property of the receive path:
    ///
    /// 1. **Digest first.** Per-chunk signatures say who sent each chunk; only
    ///    the whole-file digest says the assembly is the file the manifest
    ///    described. It is checked before the encryptor opens the file, because
    ///    after that the bytes are inside the attachments directory.
    /// 2. **`already_stored` next**, for the same reason the single-packet path
    ///    checks it: a file's bytes reach disk *before* the row that names them,
    ///    so storing first and letting an idempotent row write be ignored would
    ///    leave an encrypted copy nothing references.
    /// 3. **Then encrypt into place** with the streaming call the CDN upload path
    ///    uses, and only then the event, because the event names the attachment.
    pub(super) fn complete_transfer(&mut self, transfer_id: &TransferId, nickname: &str) {
        let transfer_hex = hex_encode(transfer_id);
        if let Err(error) = self.files_in.verify(transfer_id) {
            // Terminal. `verify` has already discarded the partial file and its
            // bitmap: a mismatch means some chunk that passed its packet
            // signature still produced the wrong file, so there is no chunk to
            // re-request and asking again would loop forever on the same bytes.
            tracing::error!(
                target: "mesh::ble",
                %error,
                transfer = %transfer_hex,
                "a completed chunked transfer did not match its digest; discarded, not retried"
            );
            self.degraded(format!(
                "a file from {nickname} arrived corrupted and was discarded"
            ));
            return;
        }

        let part = self.files_in.part_path(transfer_id);
        let Some(transfer) = self.files_in.take(transfer_id) else {
            return;
        };

        match self.events.already_stored(&transfer.wire_id) {
            Ok(true) => {
                tracing::debug!(
                    target: "mesh::ble",
                    transfer = %transfer_hex,
                    "a chunked file already on disk completed again; not stored twice"
                );
                self.files_in.finish(transfer_id);
                return;
            }
            Ok(false) => {}
            Err(error) => {
                // Store anyway: a duplicate copy is recoverable, and this file
                // cost minutes of radio that nothing will spend again.
                tracing::warn!(
                    target: "mesh::ble",
                    %error,
                    "could not check whether this chunked file is already stored"
                );
            }
        }

        let attachment = match self.events.store_file_streamed(
            transfer.manifest.file_name.as_deref(),
            transfer.manifest.mime_type.as_deref(),
            &part,
        ) {
            Ok(attachment) => attachment,
            Err(error) => {
                // The partial file is deliberately left on disk. Everything else
                // about this transfer is gone from the air, and re-requesting is
                // not possible — the bytes are all here, only the encryption or
                // the row write failed, and a retry after the user frees disk
                // space is the one recovery left.
                tracing::error!(
                    target: "mesh::ble",
                    %error,
                    transfer = %transfer_hex,
                    bytes = transfer.manifest.total_size,
                    "a completed chunked transfer could not be stored"
                );
                self.degraded(format!(
                    "a file from {nickname} could not be saved: {error}"
                ));
                return;
            }
        };
        self.files_in.finish(transfer_id);

        tracing::info!(
            target: "mesh::ble",
            peer = %transfer.sender,
            nickname = %nickname,
            transfer = %transfer_hex,
            bytes = transfer.manifest.total_size,
            chunks = transfer.manifest.chunk_count,
            name = ?transfer.manifest.file_name,
            "a chunked file transfer completed"
        );

        let verified = self.state.with(|state| {
            state
                .peers
                .info(&transfer.sender)
                .is_some_and(|p| p.verified_nickname)
        });
        self.events.send(MeshEvent::Message(Box::new(MeshMessage {
            message: mapping::inbound_file_message(
                &MeshConversation::Room,
                &transfer.wire_id,
                &transfer.sender,
                attachment,
                // The manifest's timestamp, not now: the row belongs where the
                // sender put it, and a transfer that took ten minutes would
                // otherwise sort ten minutes after everything said during it.
                transfer.timestamp_ms,
            ),
            conversation: MeshConversation::Room,
            sender: transfer.sender,
            nickname: nickname.to_string(),
            is_local: false,
            sender_verified: verified,
        })));
    }

    /// A receiver asking us to send chunks again.
    ///
    /// Authenticated like everything else here, and for a reason beyond
    /// attribution: serving a request costs *our* airtime, so an unauthenticated
    /// request would be an amplification lever for anyone in range.
    pub(super) fn on_file_chunk_request(&mut self, packet: &BitchatPacket, claimed: &PeerId) {
        let Ok(request) = FileChunkRequest::decode(&packet.payload) else {
            return;
        };
        let Some((sender, _nickname, _verified)) = self.authenticate_stream_packet(packet, claimed)
        else {
            return;
        };
        // Only a transfer we are actually sending. A request for anything else is
        // a peer asking us to serve a file we do not have — which, once chunked
        // transfers ride the gossip archive, becomes a real feature; today it is
        // simply not ours to answer.
        let Some(transfer) = self.files_out.get_mut(&request.transfer_id) else {
            return;
        };
        let accepted = transfer.note_requested(&request.indices);
        // `asked` and `queued` separately, because they can differ: an index the
        // manifest does not cover is dropped here, and logging only the request
        // size would report a retransmission that never happened.
        tracing::info!(
            target: "mesh::ble",
            peer = %sender,
            transfer = %hex_encode(&request.transfer_id),
            asked = request.indices.len(),
            queued = accepted,
            "retransmitting chunks a receiver asked for"
        );
    }
}

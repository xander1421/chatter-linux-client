//! The radio runtime: the loop that drives the mesh.
//!
//! Every crate below this one is sans-IO. `chatter-mesh` parses and decides,
//! `chatter-mesh-link` schedules and plans, `chatter-mesh-store` retains and
//! syncs — and not one of them owns a clock, a task, or a socket. Something has
//! to read radio events, feed them through that policy, and put the resulting
//! bytes back on the air on a timer. This is that something.
//!
//! # Three inputs, one loop
//!
//! Link events, service commands, and a one-second tick, multiplexed so that no
//! two of them can touch the policy state concurrently. The tick is what makes
//! the duty cycle, the announce cadence, gossip sync, and outbox maintenance
//! happen at all: none of those layers can schedule themselves.

use std::sync::Arc;
use std::time::Duration;

use chatter_mesh::codec;
use chatter_mesh::dedup::MessageDeduplicator;
use chatter_mesh::fragment;
use chatter_mesh::ids::PeerId;
use chatter_mesh::packet::{
    BitchatPacket, MessageType, NoisePayloadType, SENDER_ID_LEN,
    decode_noise_payload, encode_noise_payload,
};
use chatter_mesh::relay::{self, RelayInput};
use chatter_mesh::tlv::announce::AnnouncementPacket;
use chatter_mesh::tlv::capabilities::PeerCapabilities;
use chatter_mesh::tlv::courier_envelope::CourierEnvelope;
use chatter_mesh::tlv::private_message::PrivateMessagePacket;
use chatter_mesh::tlv::request_sync::RequestSyncPacket;
use chatter_mesh_link::backpressure::{
    InboundAppend, InboundChunk, InboundLimits, PENDING_WRITE_BUFFER_CAP_BYTES, should_buffer, WriteEnqueue,
};
use chatter_mesh_link::duty::{self, ScanCommand};
use chatter_mesh_link::guard::{IngressGuard, IngressInput};
use chatter_mesh_link::planner::{self, DIRECTED_SPOOL_WINDOW_SECS, OutboundPlanRequest};
use chatter_mesh_link::registry::VerifiedDirectAnnounce;
use chatter_mesh_link::scheduler::{
    DiscoveryDecision, ExistingConnectionState, MAX_CENTRAL_LINKS, PeripheralConnectionState,
    QueueDecision,
};
use chatter_mesh_link::{LinkEvent, LinkId, LinkRole, MeshLink};
use chatter_mesh_noise::seal::open_courier;
use chatter_mesh_noise::sign::{sign_packet, verify_packet};
use chatter_mesh_noise::{MeshIdentity, NoiseError};
use chatter_mesh_store::CourierTier;
use chatter_mesh_store::archive::GossipMessageArchive;
use chatter_mesh_store::courier::{AnnounceIngress, CourierStore};
use chatter_mesh_store::outbox::AckKind;
use chatter_mesh_store::sync::{GossipSyncManager, SyncConfig};
use tokio::sync::mpsc::UnboundedReceiver;

use super::config::MeshConfig;
use super::events::{MeshEvent, MeshMessage};
use super::glue::SharedStorage;
use super::mapping::{InboundMessage, MeshConversation, outbound_message};
use super::router::{RouterCommand, RouterHandle};
use super::state::{MeshState, NostrState, link_candidates_locked, now_millis, now_secs};
use super::timeline::MeshEvents;
use super::transport::BleCommand;

/// How often the loop wakes to run the parts that nothing else can schedule.
const TICK: Duration = Duration::from_millis(500);

/// The TTL an announce still carries when it has not been relayed.
///
/// Only ever a *hint*: the TTL is excluded from the signature preimage so relays
/// can decrement it, which means anyone can set it back and look adjacent.
const DIRECT_ANNOUNCE_TTL: u8 = chatter_mesh::relay::MESSAGE_TTL_DEFAULT;

/// The radio loop.
pub struct BleRuntime {
    radio: Arc<dyn MeshLink>,
    state: Arc<MeshState>,
    /// Read only, so a status event reports both transports rather than
    /// resetting the relay count to zero every time a link changes.
    nostr_state: Arc<NostrState>,
    identity: Arc<MeshIdentity>,
    config: Arc<MeshConfig>,
    events: MeshEvents,
    router: Arc<RouterHandle>,
    gossip: GossipSyncManager,
    couriers: CourierStore<SharedStorage>,
    guard: IngressGuard,
}

impl BleRuntime {
    #[must_use]
    pub fn new(
        radio: Arc<dyn MeshLink>,
        state: Arc<MeshState>,
        nostr_state: Arc<NostrState>,
        identity: Arc<MeshIdentity>,
        config: Arc<MeshConfig>,
        events: MeshEvents,
        router: Arc<RouterHandle>,
        storage: SharedStorage,
    ) -> Self {
        let now = now_secs();
        let archive = GossipMessageArchive::new(Arc::new(storage.clone()));
        let gossip = GossipSyncManager::new(
            &identity.peer_id(),
            SyncConfig::default(),
            Some(archive),
            now,
        );
        let couriers = CourierStore::new(storage);
        Self {
            radio,
            state,
            nostr_state,
            identity,
            config,
            events,
            router,
            gossip,
            couriers,
            guard: IngressGuard::default(),
        }
    }

    /// Start the radio and run until the command channel or the radio closes.
    ///
    /// The receiver is a parameter rather than a field so that the `select!`
    /// branch reading it does not borrow the whole runtime, which every handler
    /// needs mutably.
    pub async fn run(mut self, mut commands: UnboundedReceiver<BleCommand>) {
        if let Err(error) = self.radio.start().await {
            self.degraded(format!("the radio would not start: {error}"));
            return;
        }
        self.state.with(|state| state.radio_running = true);
        self.emit_status();
        // Our own ID, said once. Every packet on the wire names its sender by
        // this derivation, so without it an unfamiliar sender in the log cannot
        // even be ruled out as our own traffic coming back.
        tracing::info!(
            target: "mesh::ble",
            local_peer = %self.identity.peer_id(),
            "radio up"
        );

        // Carrying anything already in the courier pool matters more than it
        // looks: a device that forgets its pool on restart drops other people's
        // mail on the floor with no way to recover it.
        if let Err(error) = self.couriers.restore(now_secs()) {
            self.degraded(format!("the courier pool could not be restored: {error}"));
        }

        self.announce().await;

        let radio = Arc::clone(&self.radio);
        let mut ticker = tokio::time::interval(TICK);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                event = radio.next_event() => match event {
                    Some(event) => self.on_link_event(event).await,
                    // The radio has shut down for good.
                    None => break,
                },
                command = commands.recv() => match command {
                    Some(BleCommand::Stop) | None => break,
                    Some(command) => self.on_command(command).await,
                },
                _ = ticker.tick() => self.on_tick().await,
            }
        }

        let _ = self.radio.stop().await;
        self.state.with(|state| {
            state.radio_running = false;
            state.sessions.clear();
        });
        self.emit_status();
    }

    // ------------------------------------------------------------- link events

    async fn on_link_event(&mut self, event: LinkEvent) {
        match event {
            LinkEvent::Discovered { address, rssi } => self.on_discovered(&address, rssi).await,
            LinkEvent::Connected {
                link,
                role,
                max_write,
            } => self.on_connected(link, role, max_write).await,
            LinkEvent::Disconnected { link } => self.on_disconnected(&link).await,
            LinkEvent::Data { link, bytes } => self.on_data(&link, bytes).await,
            LinkEvent::AdapterPowerChanged { powered } => {
                self.state.with(|state| {
                    state.adapter_powered = powered;
                    if !powered {
                        // Every link dies with the adapter. Keeping the bindings
                        // would make the router believe it can still write.
                        state.links.clear();
                        state.max_write.clear();
                        state.address_of.clear();
                        state.connecting.clear();
                        state.scheduler.reset();
                    }
                });
                self.emit_status();
                self.emit_peers();
            }
        }
    }

    async fn on_discovered(&mut self, address: &str, rssi: i16) {
        let now = now_millis();
        let decision = self.state.with(|state| {
            let candidate =
                chatter_mesh_link::scheduler::ConnectionCandidate::new(address, rssi, now);
            let connected_or_connecting =
                state.links.count_role(LinkRole::Central) + state.connecting.len();
            let existing = ExistingConnectionState {
                is_connecting: state.connecting.contains(address),
                is_connected: state
                    .address_of
                    .values()
                    .any(|known| known.as_str() == address),
                last_connect_attempt_ms: None,
            };
            let peripheral = if state.links.count_role(LinkRole::Peripheral) > 0 {
                PeripheralConnectionState::Connected
            } else {
                PeripheralConnectionState::Disconnected
            };
            state.scheduler.handle_discovery(
                &candidate,
                connected_or_connecting,
                Some(existing),
                peripheral,
                now,
            )
        });

        match decision {
            DiscoveryDecision::ConnectNow => self.dial(address).await,
            DiscoveryDecision::CancelStaleConnection => {
                self.state.with(|state| {
                    state.connecting.remove(address);
                });
            }
            // Queued and retry are both "drain later", which the tick does.
            DiscoveryDecision::Queued
            | DiscoveryDecision::ScheduleRetry { .. }
            | DiscoveryDecision::Ignore => {}
        }
    }

    async fn dial(&mut self, address: &str) {
        let now = now_millis();
        self.state.with(|state| {
            state.connecting.insert(address.to_string());
            state.scheduler.note_connect_attempt(now);
        });
        match self.radio.connect(address).await {
            Ok(link) => {
                self.state.with(|state| {
                    state.address_of.insert(link, address.to_string());
                    state.scheduler.note_connected(address);
                });
            }
            Err(error) => {
                let timed_out = matches!(error, chatter_mesh_link::LinkError::ConnectTimedOut { .. });
                self.state.with(|state| {
                    state.connecting.remove(address);
                    // A timeout earns the long ignore window; an outright failure
                    // earns only the short one. `note_connect_timeout` existed and
                    // was never reachable from here, because the radio layer
                    // reported a timeout as an ordinary failure — so the window it
                    // documents had never once applied.
                    if timed_out {
                        state.scheduler.note_connect_timeout(address, now);
                    } else {
                        state.scheduler.note_connect_failed(address, now);
                    }
                });
                tracing::debug!(target: "mesh::ble", %address, %error, "connect failed");
            }
        }
    }

    async fn on_connected(&mut self, link: LinkId, role: LinkRole, max_write: usize) {
        self.state.with(|state| {
            state.links.link_up(link.clone(), role);
            // `Connected` means writable: the trait's contract is that an
            // implementation withholds this event until the link can carry a
            // write, so the flag is set here rather than waited for.
            state.links.set_writable(&link, true);
            state.max_write.insert(link.clone(), max_write);
            if let Some(address) = state.address_of.get(&link).cloned() {
                state.connecting.remove(&address);
            }
        });
        self.emit_status();
        // Announce straight away: a peer that has just connected knows nothing
        // about us until one arrives, and the cadence timer may be seconds out.
        self.announce().await;
        self.flush_buffered_writes().await;
    }

    async fn on_disconnected(&mut self, link: &LinkId) {
        let now = now_millis();
        let peer = self.state.with(|state| {
            let peer = state.links.link_down(link);
            state.max_write.remove(link);
            state.assembler.discard(link);
            state.writes.discard_all(link);
            if let Some(address) = state.address_of.remove(link) {
                state.connecting.remove(&address);
                state.scheduler.note_disconnected(&address, now);
            }
            // A session outlives its link on purpose — the peer may come back on
            // another one — but the link's Noise authentication does not.
            state.links.clear_noise_authentication(link);
            peer
        });
        self.reconcile_peers();
        self.emit_status();
        if let Some(peer) = peer {
            tracing::debug!(target: "mesh::ble", peer = %peer, "link down");
        }
    }

    async fn on_data(&mut self, link: &LinkId, bytes: Vec<u8>) {
        let now = now_millis();
        let appended = self.state.with(|state| {
            let offset = state.assembler.buffered_bytes(link);
            state.last_traffic_ms = now;
            state.assembler.append(
                link,
                &[InboundChunk { offset, bytes }],
                InboundLimits::default(),
                now,
            )
        });

        match appended {
            InboundAppend::Decoded { packet, .. } => self.on_packet(link, *packet).await,
            InboundAppend::Waiting { .. } => {}
            InboundAppend::Oversized { .. } => {
                tracing::debug!(target: "mesh::ble", link = %link, "oversized inbound frame discarded");
            }
        }
    }

    // ----------------------------------------------------------------- inbound

    async fn on_packet(&mut self, link: &LinkId, packet: BitchatPacket) {
        let now_ms = now_millis();
        let now_s = now_secs();
        let local = self.identity.peer_id();
        let claimed = PeerId::from_bytes(&packet.sender_id);
        let bound = self.state.with(|state| state.links.peer_for(link).cloned());

        // Which peer's outstanding request this could be answering.
        //
        // For a sync response that is the peer on the *link* — the one that
        // forwarded it — not the ID stamped on the packet. A gossip response
        // carries another peer's packet verbatim, so its sender is the original
        // author and can be anybody, including us. Asking "do we have a request
        // out to the author" therefore answered a question nobody had, and every
        // response was rejected as unsolicited: the guard names the link peer in
        // its rejection while this had computed the answer for the author.
        //
        // `IngressGuard::evaluate` picks the same peer for the same reason and
        // passes it to the closure below; the closure cannot use it directly only
        // because `self.gossip` may not be borrowed inside `self.state.with`.
        let validation_peer = if packet.is_rsr {
            bound.clone().unwrap_or_else(|| claimed.clone())
        } else {
            claimed.clone()
        };
        let solicited = self
            .gossip
            .is_valid_response(&validation_peer, packet.is_rsr, now_s);

        // Copied out so the closure below borrows only the state, not the whole
        // runtime.
        let guard = self.guard;
        let accepted = self.state.with(|state| {
            guard.evaluate(
                IngressInput {
                    packet: &packet,
                    claimed_sender: &claimed,
                    bound_peer: bound.as_ref(),
                    local_peer: &local,
                    now_unix_millis: now_ms,
                },
                &state.self_broadcasts,
                |_| solicited,
            )
        });

        let accepted = match accepted {
            Ok(accepted) => accepted,
            Err(rejection) => {
                // `pending` is the whole of the RSR decision: a response is
                // exempt from the freshness check only while we have an
                // outstanding request to that peer. Printing the count next to
                // the rejection is what distinguishes "we never asked" from
                // "we asked and the bookkeeping lost it".
                tracing::debug!(
                    target: "mesh::ble",
                    %rejection,
                    is_rsr = packet.is_rsr,
                    claimed_sender = %claimed,
                    pending_sync_peers = ?self.gossip.pending_request_peers(),
                    "packet rejected at ingress"
                );
                return;
            }
        };

        // Dedup before anything with a side effect. A flooded mesh delivers the
        // same packet on several links, and processing each one duplicates the
        // message and re-relays it.
        let duplicate = self.state.with(|state| {
            let key = MessageDeduplicator::dedup_key(&packet);
            if !MessageDeduplicator::should_deduplicate(&packet, &local) {
                return false;
            }
            if state.dedup.is_duplicate(&key, now_s) {
                return true;
            }
            state.dedup.mark_processed(&key, now_s);
            false
        });
        if duplicate {
            return;
        }

        self.state.with(|state| {
            state.ingress.record_if_new(
                &packet,
                link.clone(),
                accepted.received_from.clone(),
                now_ms,
                chatter_mesh_link::registry::INGRESS_RECORD_LIFETIME_MS,
            );
            state.peers.update_last_seen(&accepted.received_from, now_ms);
        });

        self.dispatch(link, &packet, &claimed, now_s).await;

        if packet.is_broadcast() {
            self.gossip.on_public_packet_seen(&packet, now_s);
        }
        self.relay(link, &packet, &local).await;
    }

    /// Unwrap a fragment, then handle whatever it turns out to be.
    ///
    /// Deliberately not recursive. The fragmenter never nests fragments, so one
    /// level of unwrapping is the whole truth — and expressing it as one level
    /// means a hostile peer cannot make us descend at all.
    async fn dispatch(
        &mut self,
        link: &LinkId,
        packet: &BitchatPacket,
        claimed: &PeerId,
        now_s: u64,
    ) {
        if packet.message_type() != Some(MessageType::Fragment) {
            self.handle_typed(link, packet, claimed).await;
            return;
        }

        let reassembled = self
            .state
            .with(|state| state.fragments.accept(packet, now_s));
        let Ok(Some(bytes)) = reassembled else {
            return;
        };
        let Ok(inner) = codec::decode(&bytes) else {
            return;
        };
        if inner.message_type() == Some(MessageType::Fragment) {
            // A fragment carrying a fragment is not something the fragmenter
            // produces; it is someone trying to make us loop.
            return;
        }
        self.handle_typed(link, &inner, claimed).await;
    }

    async fn handle_typed(&mut self, link: &LinkId, packet: &BitchatPacket, claimed: &PeerId) {
        match packet.message_type() {
            Some(MessageType::Announce) => self.on_announce(link, packet, claimed).await,
            Some(MessageType::Message) => self.on_public_message(packet, claimed),
            Some(MessageType::Leave) => {
                self.state.with(|state| {
                    state.peers.remove(claimed);
                    state.sessions.forget(claimed);
                });
                self.gossip.remove_announcement_for_peer(claimed);
                self.emit_peers();
            }
            Some(MessageType::NoiseHandshake) => self.on_handshake(packet, claimed).await,
            Some(MessageType::NoiseEncrypted) => self.on_encrypted(link, packet, claimed).await,
            Some(MessageType::CourierEnvelope) => self.on_courier_envelope(packet, claimed),
            Some(MessageType::RequestSync) => self.on_request_sync(packet, claimed).await,
            // File transfer, boards, groups, voice and prekey bundles were
            // deferred from P0. Not handling them is right; not *relaying* them
            // would partition every peer that does implement them, so the relay
            // decision in `on_packet` still runs.
            _ => {}
        }
    }

    async fn on_announce(&mut self, link: &LinkId, packet: &BitchatPacket, claimed: &PeerId) {
        // Our own announce, come back to us. The ingress guard lets a
        // self-authored TTL-0 sync response through on purpose — that is how a
        // reinstalled device recovers its own history from a peer's archive — so
        // this is the one place that has to say no to it. Without the check we
        // register *ourselves* as a peer, which inflates the "N nearby" count the
        // whole mesh UI is built around and binds a link to our own identity.
        // bitchat refuses it in its announce preflight, as `.reject(.selfAnnounce)`.
        if claimed == &self.identity.peer_id() {
            return;
        }

        let Ok(announcement) = AnnouncementPacket::decode(&packet.payload) else {
            return;
        };
        let Ok(signing_key) = <[u8; 32]>::try_from(announcement.signing_public_key.as_slice())
        else {
            return;
        };
        let verified = verify_packet(packet, &signing_key);
        if !verified {
            // bitchat drops an unverified announce outright rather than
            // recording the peer unverified (`BLEAnnounceHandler.swift:167`).
            // Recording it would be worse than useless: an unverified announce
            // carries an attacker-chosen noise key and nickname, so accepting it
            // is exactly the identity-poisoning their handler exists to prevent.
            tracing::debug!(target: "mesh::ble", peer = %claimed, "unverified announce ignored");
            return;
        }

        let now_ms = now_millis();
        let now_s = now_secs();
        let direct = VerifiedDirectAnnounce::from_packet(
            claimed.clone(),
            packet,
            verified,
            DIRECT_ANNOUNCE_TTL,
        );

        let (is_new, link_authenticated, first_sighting, reconnected, bound_peer) =
            self.state.with(|state| {
            let update = state
                .peers
                .upsert_verified_announce(claimed, &announcement, true, now_ms);
            // Bind first, always. `rebind_after_verified_direct` refuses an
            // *unbound* link by design — it exists for the contested case, where a
            // link already belongs to someone else and a verified direct announce
            // is the only evidence strong enough to move it. Calling only rebind
            // for a direct announce therefore never bound a fresh inbound link at
            // all, which is the normal first-contact case: the peer registered,
            // no link was bound to it, so `reconcile_connectivity` saw no live
            // presence and demoted it 8s later — and the next announce arrived as
            // a reconnect. bitchat binds a peripheral link to the claimed sender
            // on a direct announce before any signature check
            // (`BLEService.swift:2516`), so binding is not the guarded part.
            //
            // Rebinding still runs afterwards, and is still fully guarded: on a
            // link we just bound it refuses as `SamePeer`, and on a contested one
            // `bind_if_unbound` was the no-op instead.
            state.links.bind_if_unbound(link, claimed);
            if let Some(direct) = direct.as_ref() {
                state
                    .links
                    .rebind_after_verified_direct(link, direct, now_ms);
            }
            let authenticated = state.sessions.has_session(claimed);
            if authenticated {
                state.links.mark_noise_authenticated(link, claimed);
            }
            (
                update.is_new_peer || update.was_disconnected,
                authenticated,
                update.is_new_peer,
                update.was_disconnected,
                state.links.peer_for(link).cloned(),
            )
        });

        self.emit_peers();
        // The two halves are reported separately on purpose. `is_new` is
        // `first_sighting || reconnected`, and conflating them hides the
        // interesting case: a peer that keeps *reconnecting* was demoted for
        // silence between announces, which only happens when no live link is
        // bound to it. That is a binding problem wearing a discovery problem's
        // clothes, and one field cannot tell you which you have.
        tracing::debug!(
            target: "mesh::ble",
            peer = %claimed,
            nickname = %announcement.nickname,
            first_sighting,
            reconnected,
            link = %link,
            bound_to = ?bound_peer,
            "announce accepted"
        );

        // Hand over any mail we are carrying for them.
        //
        // The ingress that releases it is deliberately strict: a *signature
        // verified* direct announce arriving on a link that is Noise
        // authenticated for this peer right now. A session outlives its link, so
        // "we have a session with X" is not enough — a replayed announce could
        // otherwise rebind X's ID to the replayer's link and drain their mail.
        let ingress = AnnounceIngress::new(direct.is_some(), link_authenticated);
        if ingress.releases_mail()
            && let Ok(key) = <[u8; 32]>::try_from(announcement.noise_public_key.as_slice())
        {
            self.hand_over_courier_mail(claimed, &key, now_s, ingress).await;
        }

        if is_new {
            // Push anything retained for them, and offer them mail for others.
            let _ = self.router.send(RouterCommand::PeerAvailable {
                peer: claimed.clone(),
            });
            // A peer we have not synced with yet gets our public history.
            if let Ok(send) = self.gossip.initial_sync_to(claimed, now_s) {
                self.dispatch_sync(send).await;
            }
        }
    }

    fn on_public_message(&mut self, packet: &BitchatPacket, claimed: &PeerId) {
        // A `Message` addressed to one recipient is a legacy private message. We
        // do not carry those — private traffic is Noise-encrypted — and showing
        // it as public chat would put a message meant for one person in the room.
        if !packet.is_broadcast() {
            return;
        }
        let Ok(content) = core::str::from_utf8(&packet.payload) else {
            return;
        };

        // The sender is whoever the packet header says, authenticated against
        // the signing key their announce established — there is no self-declared
        // sender field to prefer, and preferring one would let anybody publish
        // under anybody's name. An unsigned or unverifiable public message is
        // dropped, matching `BLEPublicMessageHandler.swift:86-95`.
        let sender = claimed.clone();
        // A message we wrote ourselves, recovered from a peer's gossip archive.
        // The ingress guard admits a self-authored TTL-0 sync response precisely
        // so this can happen — it is how a device that lost its database gets its
        // own history back — so it has to be handled rather than dropped.
        //
        // It is authenticated against our own signing key, because we are not in
        // our own peer registry (and must not be: see `on_announce`). It is
        // attributed to us, because it *is* ours; treating it as inbound would put
        // our own words on screen as though a stranger had said them.
        let is_self = sender == self.identity.peer_id();
        let (signing_key, nickname, verified) = if is_self {
            (
                self.identity.signing_public(),
                self.config.nickname.clone(),
                true,
            )
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
                "public message with missing or invalid signature dropped",
            );
            return;
        }

        tracing::info!(
            target: "mesh::ble",
            peer = %sender,
            nickname = %nickname,
            bytes = content.len(),
            is_self,
            "public message received"
        );
        let inbound = InboundMessage {
            // No message ID on the wire, so one is derived from the packet. The
            // dedup key is a function of sender, timestamp, type and payload
            // hash, so every copy that reaches us over a different path — and
            // the same message seen twice — resolves to one ID.
            wire_id: MessageDeduplicator::dedup_key(packet),
            conversation: MeshConversation::Room,
            sender: sender.clone(),
            nickname: nickname.clone(),
            content: content.to_string(),
            timestamp_ms: packet.timestamp,
        };
        self.events.send(MeshEvent::Message(Box::new(MeshMessage {
            message: inbound.to_domain(),
            conversation: inbound.conversation,
            sender,
            nickname,
            // `is_local` decides which side of the timeline it lands on and
            // whether it counts as unread. A recovered self-authored message is
            // ours on both counts.
            is_local: is_self,
            sender_verified: verified,
        })));
    }

    async fn on_handshake(&mut self, packet: &BitchatPacket, claimed: &PeerId) {
        let local = self.identity.peer_id();
        let identity = Arc::clone(&self.identity);
        let step = self.state.with(|state| {
            state
                .sessions
                .on_handshake(claimed, &local, &identity, &packet.payload)
        });

        let step = match step {
            Ok(step) => step,
            Err(error) => {
                tracing::debug!(target: "mesh::ble", peer = %claimed, %error, "handshake failed");
                return;
            }
        };

        if let Some(reply) = step.reply {
            let packet = self.build_packet(
                MessageType::NoiseHandshake,
                Some(claimed),
                reply,
                chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
            );
            self.transmit(&packet, Some(claimed), None).await;
        }

        if step.established.is_some() {
            // A session is the only signal strong enough for the router to send
            // without retaining, so this is the moment retained mail can go.
            let _ = self.router.send(RouterCommand::PeerAvailable {
                peer: claimed.clone(),
            });
            self.flush_pending_dms(claimed).await;
            self.emit_peers();
        }
    }

    async fn on_encrypted(&mut self, link: &LinkId, packet: &BitchatPacket, claimed: &PeerId) {
        // Only accept encrypted traffic addressed to us; a directed packet for
        // somebody else is relayed, not opened.
        if !self.addressed_to_us(packet) {
            return;
        }
        let opened = self
            .state
            .with(|state| state.sessions.decrypt(claimed, &packet.payload));
        let plaintext = match opened {
            Ok(plaintext) => plaintext,
            Err(NoiseError::WrongState(_)) => {
                // No session. Start one rather than dropping the peer silently:
                // this is how a first message from a peer we have never spoken
                // to gets answered.
                self.start_handshake(claimed).await;
                return;
            }
            Err(error) => {
                tracing::debug!(target: "mesh::ble", peer = %claimed, %error, "decrypt failed");
                return;
            }
        };

        // The link carried traffic only this peer's session key could produce.
        self.state
            .with(|state| state.links.mark_noise_authenticated(link, claimed));

        let Some((payload_type, body)) = decode_noise_payload(&plaintext) else {
            return;
        };
        match payload_type {
            NoisePayloadType::PrivateMessage => {
                let Ok(message) = PrivateMessagePacket::decode(body) else {
                    return;
                };
                self.emit_dm(claimed, &message.message_id, &message.content, packet.timestamp);
                self.send_ack(claimed, &message.message_id, NoisePayloadType::Delivered)
                    .await;
            }
            NoisePayloadType::Delivered => self.forward_ack(claimed, body, AckKind::Delivered),
            NoisePayloadType::ReadReceipt => self.forward_ack(claimed, body, AckKind::Read),
            // Groups, voice, and verification are deferred; dropping the payload
            // is the correct handling for a type this build does not implement.
            _ => {}
        }
    }

    fn forward_ack(&self, peer: &PeerId, body: &[u8], kind: AckKind) {
        // The ack body is the message ID as raw UTF-8, not a TLV.
        let Ok(message_id) = String::from_utf8(body.to_vec()) else {
            return;
        };
        let _ = self.router.send(RouterCommand::Acked {
            message_id,
            peer: peer.clone(),
            kind,
        });
    }

    fn on_courier_envelope(&mut self, packet: &BitchatPacket, claimed: &PeerId) {
        let Ok(envelope) = CourierEnvelope::decode(&packet.payload) else {
            return;
        };

        // Is it ours? Only the seal can answer that — the recipient tag rotates
        // and is a hint, not proof.
        if let Ok((plaintext, sender_key)) = open_courier(&envelope.ciphertext, &self.identity) {
            if let Some((NoisePayloadType::PrivateMessage, body)) = decode_noise_payload(&plaintext)
                && let Ok(message) = PrivateMessagePacket::decode(body)
            {
                let sender = PeerId::from_noise_key(&sender_key);
                self.emit_dm(&sender, &message.message_id, &message.content, packet.timestamp);
            }
            return;
        }

        // Not ours: carry it. Verified strangers and mutual favourites draw from
        // separate quotas, which is what stops strangers crowding out favourites.
        let snapshot = self.state.with(|state| {
            state
                .peers
                .info(claimed)
                .map(|record| (record.noise_public_key.clone(), record.verified_nickname))
        });
        let Some((Some(depositor_key), verified)) = snapshot else {
            return;
        };
        if !verified {
            // An unverified peer may not deposit mail: without a checked
            // signature the pool is a free amplifier.
            return;
        }
        if let Err(error) = self.couriers.deposit(
            &envelope,
            &depositor_key,
            CourierTier::Verified,
            now_secs(),
        ) {
            tracing::debug!(target: "mesh::ble", %error, "courier deposit refused");
        }
    }

    async fn on_request_sync(&mut self, packet: &BitchatPacket, claimed: &PeerId) {
        let Ok(request) = RequestSyncPacket::decode(&packet.payload) else {
            return;
        };
        let sends = self
            .gossip
            .handle_request_sync(claimed, &request, now_secs());
        for send in sends {
            self.dispatch_sync(send).await;
        }
    }

    // ---------------------------------------------------------------- outbound

    async fn on_command(&mut self, command: BleCommand) {
        match command {
            BleCommand::Stop => {}
            BleCommand::Announce => self.announce().await,
            BleCommand::Broadcast { packet, sign } => {
                let packet = self.maybe_sign(packet, sign);
                self.transmit(&packet, None, None).await;
            }
            BleCommand::Directed { peer, packet, sign } => {
                let packet = self.maybe_sign(packet, sign);
                self.transmit(&packet, Some(&peer), None).await;
            }
            BleCommand::PrivateMessage {
                peer,
                message_id,
                content,
            } => self.send_private(&peer, &message_id, &content).await,
            BleCommand::Courier { peer, envelope } => {
                let Ok(payload) = envelope.encode() else {
                    return;
                };
                let packet = self.build_packet(
                    MessageType::CourierEnvelope,
                    Some(&peer),
                    payload,
                    chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
                );
                self.transmit(&packet, Some(&peer), None).await;
            }
            BleCommand::Ack {
                peer,
                message_id,
                payload_type,
            } => self.send_ack(&peer, &message_id, payload_type).await,
            BleCommand::PublicMessage {
                message_id,
                content,
            } => self.broadcast_public(&message_id, &content).await,
        }
    }

    async fn send_private(&mut self, peer: &PeerId, message_id: &str, content: &str) {
        let Ok(tlv) = PrivateMessagePacket::new(message_id.to_string(), content.to_string()).encode()
        else {
            return;
        };
        let payload = encode_noise_payload(NoisePayloadType::PrivateMessage, &tlv);

        let sealed = self
            .state
            .with(|state| state.sessions.encrypt(peer, &payload));
        match sealed {
            Ok(ciphertext) => {
                let packet = self.build_packet(
                    MessageType::NoiseEncrypted,
                    Some(peer),
                    ciphertext,
                    chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
                );
                self.transmit(&packet, Some(peer), None).await;
            }
            Err(_) => {
                // No session yet. Hold the message and start a handshake: the
                // router has already retained a copy, so nothing is lost if the
                // handshake never completes.
                self.state.with(|state| {
                    state
                        .pending_dms
                        .entry(peer.clone())
                        .or_default()
                        .push(super::state::PendingDm {
                            message_id: message_id.to_string(),
                            content: content.to_string(),
                            queued_at_ms: now_millis(),
                        });
                });
                self.start_handshake(peer).await;
            }
        }
    }

    async fn start_handshake(&mut self, peer: &PeerId) {
        let identity = Arc::clone(&self.identity);
        let opening = self
            .state
            .with(|state| state.sessions.initiate(peer, &identity));
        let Ok(opening) = opening else {
            return;
        };
        let packet = self.build_packet(
            MessageType::NoiseHandshake,
            Some(peer),
            opening,
            chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
        );
        self.transmit(&packet, Some(peer), None).await;
    }

    async fn flush_pending_dms(&mut self, peer: &PeerId) {
        let pending = self
            .state
            .with(|state| state.pending_dms.remove(peer).unwrap_or_default());
        for message in pending {
            self.send_private(peer, &message.message_id, &message.content)
                .await;
        }
    }

    async fn send_ack(
        &mut self,
        peer: &PeerId,
        message_id: &str,
        payload_type: NoisePayloadType,
    ) {
        let payload = encode_noise_payload(payload_type, message_id.as_bytes());
        let sealed = self
            .state
            .with(|state| state.sessions.encrypt(peer, &payload));
        let Ok(ciphertext) = sealed else {
            return;
        };
        let packet = self.build_packet(
            MessageType::NoiseEncrypted,
            Some(peer),
            ciphertext,
            chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
        );
        self.transmit(&packet, Some(peer), None).await;
    }

    /// Broadcast a public message into the mesh room.
    pub async fn broadcast_public(&mut self, message_id: &str, content: &str) {
        let now_ms = now_millis();
        let nickname = self.config.nickname.clone();
        // The payload of a public message is the message text and nothing else
        // (`BLEService.swift:504`: `payload: Data(content.utf8)`). There is no
        // room on the wire for a message ID, a timestamp or a sender name:
        // receivers take the sender from the packet header, the time from
        // `packet.timestamp`, and the name from the announce registry. bitchat's
        // `BitchatMessage.toBinaryPayload()` encodes all of those, and is dead
        // code outside its own tests.
        let packet = self.build_packet(
            MessageType::Message,
            None,
            content.as_bytes().to_vec(),
            chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
        );
        // Signed, because the receiver requires it: an unsigned public message
        // is dropped before its payload is even decoded
        // (`BLEPublicMessageHandler.swift:91`).
        let packet = self.maybe_sign(packet, true);
        // Remember it so the copy that floods back to us is recognised as our
        // own rather than displayed twice.
        self.state
            .with(|state| state.self_broadcasts.record(message_id, &packet, now_ms));
        self.transmit(&packet, None, None).await;

        let local = self.identity.peer_id();
        let message = outbound_message(
            &MeshConversation::Room,
            message_id,
            &local,
            content,
            now_ms,
        );
        self.events.send(MeshEvent::Message(Box::new(MeshMessage {
            message,
            conversation: MeshConversation::Room,
            sender: local,
            nickname,
            is_local: true,
            sender_verified: true,
        })));
    }

    async fn announce(&mut self) {
        let now_ms = now_millis();
        let nickname = self.config.nickname.clone();
        let mut capabilities = PeerCapabilities::from_bits(0);
        if self.config.bridge_geohash.is_some() {
            capabilities.insert(PeerCapabilities::BRIDGE);
        }
        let announcement = AnnouncementPacket::new(
            nickname.clone(),
            self.identity.static_public().to_vec(),
            self.identity.signing_public().to_vec(),
            None,
            Some(capabilities),
            self.config.bridge_geohash.clone(),
        );
        let payload = match announcement.encode() {
            Ok(payload) => payload,
            Err(error) => {
                // Silent before. An announce that cannot be encoded means this
                // device is invisible to every peer forever, which is the single
                // worst failure the mesh has and deserves to be loud.
                tracing::error!(
                    target: "mesh::ble",
                    %error,
                    nickname = %nickname,
                    "the announce could not be encoded; this device is invisible"
                );
                return;
            }
        };

        let mut packet = self.build_packet(
            MessageType::Announce,
            None,
            payload,
            chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
        );
        packet.timestamp = now_ms;
        // Signed over the packet preimage, not over `canonicalAnnounceBytes`.
        // bitchat has both, but `buildAnnounceSignature` has no callers outside
        // its own tests — the wire uses `noiseService.signPacket(packet)`
        // (`BLEService.swift:1685`), and an announce signed the other way is
        // dropped by every real peer before it reaches their registry.
        let packet = self.maybe_sign(packet, true);

        self.state.with(|state| state.last_announce_ms = now_ms);
        self.transmit(&packet, None, None).await;
    }

    /// Sign a packet over its own preimage, when the caller asked for it.
    fn maybe_sign(&self, mut packet: BitchatPacket, sign: bool) -> BitchatPacket {
        if sign && let Ok(signature) = sign_packet(&self.identity, &packet) {
            packet.signature = Some(signature);
        }
        packet
    }

    fn build_packet(
        &self,
        msg_type: MessageType,
        recipient: Option<&PeerId>,
        payload: Vec<u8>,
        ttl: u8,
    ) -> BitchatPacket {
        let sender_id = self
            .identity
            .peer_id()
            .to_routing_bytes()
            .unwrap_or([0u8; SENDER_ID_LEN]);
        // Broadcast is encoded by *omitting* the recipient, which is what
        // bitchat sends (`recipientID: nil`). The explicit all-`0xFF` form is
        // also accepted by them, but the omitted form is what their own packets
        // carry, so sending it keeps our traffic byte-comparable with theirs.
        let recipient_id = recipient.and_then(PeerId::to_routing_bytes);
        BitchatPacket::new(
            msg_type,
            sender_id,
            recipient_id,
            now_millis(),
            payload,
            ttl,
        )
    }

    /// Plan and write one packet.
    async fn transmit(
        &self,
        packet: &BitchatPacket,
        directed_peer: Option<&PeerId>,
        ingress_link: Option<&LinkId>,
    ) {
        let padding = fragment::pads_ble_frame(packet.msg_type);
        let Ok(encoded) = codec::encode(packet, padding) else {
            return;
        };

        let (plan, frames) = {
            let links = self.state.link_candidates();
            let request = OutboundPlanRequest::new(packet, encoded.len(), &links)
                .with_ingress_link(ingress_link)
                .with_directed_only_peer(directed_peer)
                .with_direct_announce_ttl(DIRECT_ANNOUNCE_TTL);
            let plan = planner::plan(&request);

            let frames = match plan.fragment_chunk_size {
                Some(chunk) => {
                    let mut id = [0u8; 8];
                    id.copy_from_slice(&packet.packet_id()[..8]);
                    match fragment::plan_fragments(packet, chunk, id) {
                        Ok(fragments) => fragments
                            .iter()
                            .filter_map(|f| codec::encode(f, fragment::pads_ble_frame(f.msg_type)).ok())
                            .collect(),
                        Err(_) => Vec::new(),
                    }
                }
                None => vec![encoded],
            };
            (plan, frames)
        };

        if plan.should_spool_directed
            && let Some(peer) = plan.directed_peer.clone()
        {
            // Directed, with no link to go down. Holding it lets an ordinary
            // link gap heal instead of dropping the packet.
            self.state.with(|state| {
                state.spool.enqueue(
                    packet.clone(),
                    peer,
                    planner::message_id(packet),
                    now_secs(),
                );
            });
            return;
        }

        if plan.selected.is_empty() {
            // Not an error — a broadcast with no links yet is normal on a quiet
            // mesh. Logged because the alternative is indistinguishable from a
            // packet that went out and was ignored, and telling those two apart
            // is most of what bringing up a second device consists of.
            tracing::debug!(
                target: "mesh::ble",
                msg_type = packet.msg_type,
                links = self.state.link_candidates().len(),
                "nothing to write to; packet not sent"
            );
            return;
        }
        tracing::debug!(
            target: "mesh::ble",
            msg_type = packet.msg_type,
            links = plan.selected.len(),
            frames = frames.len(),
            "writing"
        );

        let priority = planner::priority(packet);
        let write_priority = chatter_mesh_link::backpressure::WritePriority {
            level: priority.level,
            suborder: priority.suborder,
        };

        for link in plan.selected.iter() {
            for frame in &frames {
                match self.radio.send(link, frame).await {
                    Ok(()) => {}
                    Err(error) if should_buffer(&error) => {
                        // Routine: BLE controller queues fill, and there is no
                        // retransmission underneath us, so discarding here loses
                        // the packet outright.
                        let admitted = self.state.with(|state| {
                            state.writes.enqueue(
                                link,
                                frame.clone(),
                                write_priority,
                                PENDING_WRITE_BUFFER_CAP_BYTES,
                            )
                        });
                        // The buffer is byte-capped, so enqueue can refuse. Dropping
                        // the outcome would turn a refusal into a silent packet loss,
                        // which is exactly what #[must_use] on WriteEnqueue is for.
                        if !matches!(admitted, WriteEnqueue::Enqueued { .. }) {
                            tracing::warn!(
                                link = %link,
                                "mesh: outbound write buffer full, packet dropped"
                            );
                        }
                        self.state.with(|_state| {
                        });
                    }
                    Err(error) => {
                        tracing::debug!(target: "mesh::ble", link = %link, %error, "write failed");
                    }
                }
            }
        }
    }

    async fn relay(&self, ingress: &LinkId, packet: &BitchatPacket, local: &PeerId) {
        let degree = self.state.with(|state| state.peers.connected_count());
        let sender_is_self = PeerId::from_bytes(&packet.sender_id) == *local;
        let recipient_is_self = self.addressed_to_us(packet);
        let input = RelayInput::from_packet(
            packet,
            sender_is_self,
            recipient_is_self,
            degree,
            chatter_mesh::relay::HIGH_DEGREE_THRESHOLD,
        );
        let mut jitter = |low: u32, high: u32| -> u32 {
            if high <= low {
                return low;
            }
            low + (now_millis() as u32) % (high - low)
        };
        let decision = relay::decide(&input, &mut jitter);
        if !decision.should_relay {
            return;
        }

        let mut forwarded = packet.clone();
        forwarded.ttl = decision.new_ttl;
        // The delay is collision avoidance on a shared channel, so it has to be
        // a real wait rather than a hint. Spawned so the loop keeps servicing the
        // radio while it elapses.
        let radio = Arc::clone(&self.radio);
        let state = Arc::clone(&self.state);
        let ingress = ingress.clone();
        let delay = u64::from(decision.delay_ms);
        tokio::spawn(async move {
            if delay > 0 {
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }
            transmit_forwarded(&radio, &state, &forwarded, &ingress).await;
        });
    }

    // -------------------------------------------------------------------- tick

    async fn on_tick(&mut self) {
        let now_ms = now_millis();
        let now_s = now_secs();

        // Scan duty cycle. Scanning is the expensive half of BLE, so it runs
        // intermittently while advertising stays up continuously.
        let configured_duty = self.config.scan_duty_cycle;
        let scan = self.state.with(|state| {
            let connected = state.peers.connected_count();
            let recent_traffic = now_ms.saturating_sub(state.last_traffic_ms)
                < duty::RECENT_TRAFFIC_FORCE_SCAN_MS;
            let duty_enabled = state.duty_enabled && configured_duty;
            let command = state.duty.update(
                duty_enabled,
                state.app_is_active,
                connected,
                recent_traffic,
                now_ms,
            );
            match command {
                ScanCommand::Unchanged => state.duty.poll(now_ms),
                other => other,
            }
        });
        match scan {
            ScanCommand::StartScanning => {
                let _ = self.radio.set_scanning(true).await;
            }
            ScanCommand::StopScanning => {
                let _ = self.radio.set_scanning(false).await;
            }
            ScanCommand::Unchanged => {}
        }

        // Drain the connection queue: discoveries that were queued rather than
        // dialled immediately.
        let next = self.state.with(|state| {
            let connected_or_connecting =
                state.links.count_role(LinkRole::Central) + state.connecting.len();
            if connected_or_connecting >= MAX_CENTRAL_LINKS {
                return QueueDecision::Idle;
            }
            let connecting = state.connecting.clone();
            state.scheduler.next_candidate(
                connected_or_connecting,
                |address| connecting.contains(address),
                now_ms,
            )
        });
        if let QueueDecision::Connect(candidate) = next {
            self.dial(&candidate.address.clone()).await;
        }

        // Announce cadence, cleanup, and the directed spool.
        let plan = self.state.with(|state| {
            let connected = state.peers.connected_count();
            let elapsed = now_ms.saturating_sub(state.last_announce_ms);
            let recent_traffic =
                now_ms.saturating_sub(state.last_traffic_ms) < duty::MAINTENANCE_RECENT_TRAFFIC_MS;
            let cycle = state.maintenance_cycle;
            let plan = duty::maintenance_plan(
                cycle,
                connected,
                state.peers.is_empty(),
                elapsed,
                recent_traffic,
                duty::no_jitter,
            );
            state.maintenance_cycle = if plan.should_reset_counter {
                0
            } else {
                cycle.saturating_add(1)
            };
            plan
        });

        if plan.should_send_announce {
            self.announce().await;
        }
        if plan.should_ensure_advertising {
            // Nobody can see us. Advertising is the first suspect, and a restart
            // is the only lever the trait offers.
            let _ = self.radio.start().await;
        }
        if plan.should_run_cleanup {
            self.cleanup(now_ms, now_s);
        }
        if plan.should_flush_directed_spool {
            self.flush_directed_spool(now_s).await;
        }

        self.flush_buffered_writes().await;

        // Gossip sync, which nothing else can schedule.
        let connected: Vec<PeerId> = self
            .state
            .with(|state| state.peers.connected_peer_ids());
        match self.gossip.maintenance(&connected, now_s) {
            Ok(sends) => {
                for send in sends {
                    self.dispatch_sync(send).await;
                }
            }
            Err(error) => self.degraded(format!("gossip sync maintenance failed: {error}")),
        }
        if let Err(error) = self.gossip.persist_if_dirty(now_s) {
            self.degraded(format!("gossip archive could not be persisted: {error}"));
        }

        let _ = self.router.send(RouterCommand::Maintenance);
    }

    fn cleanup(&mut self, now_ms: u64, now_s: u64) {
        self.state.with(|state| {
            state.dedup.cleanup(now_s);
            state.fragments.evict_expired(now_s);
            state
                .ingress
                .prune(now_ms, chatter_mesh_link::registry::INGRESS_RECORD_LIFETIME_MS);
            state.self_broadcasts.prune(
                now_ms.saturating_sub(chatter_mesh_link::guard::SELF_BROADCAST_RETENTION_MILLIS),
            );
            state.scheduler.prune_backoff(now_ms);
            state
                .spool
                .prune_expired(now_s, DIRECTED_SPOOL_WINDOW_SECS);
        });
        if let Err(error) = self.couriers.prune_expired(now_s) {
            tracing::debug!(target: "mesh::ble", %error, "courier pool prune failed");
        }
        self.reconcile_peers();
    }

    async fn flush_directed_spool(&mut self, now_s: u64) {
        let spooled = self.state.with(|state| {
            state
                .spool
                .drain_unexpired(now_s, DIRECTED_SPOOL_WINDOW_SECS)
        });
        for packet in spooled {
            self.transmit(&packet.packet, Some(&packet.recipient), None)
                .await;
        }
    }

    async fn flush_buffered_writes(&self) {
        let pending = self.state.with(|state| {
            let links: Vec<LinkId> = state.writes.link_ids().cloned().collect();
            links
                .into_iter()
                .map(|link| {
                    let writes = state.writes.take_all(&link);
                    (link, writes)
                })
                .collect::<Vec<_>>()
        });

        for (link, writes) in pending {
            let mut requeue = Vec::new();
            let mut blocked = false;
            for write in writes {
                if blocked {
                    requeue.push(write);
                    continue;
                }
                match self.radio.send(&link, &write.bytes).await {
                    Ok(()) => {}
                    Err(error) if should_buffer(&error) => {
                        // Still full. Stop and put the rest back in order — a
                        // reordered fragment stream is worse than a delayed one.
                        blocked = true;
                        requeue.push(write);
                    }
                    Err(_) => {}
                }
            }
            if !requeue.is_empty() {
                self.state
                    .with(|state| state.writes.prepend(&link, requeue));
            }
        }
    }

    // ----------------------------------------------------------------- helpers

    async fn hand_over_courier_mail(
        &mut self,
        peer: &PeerId,
        recipient_key: &[u8; 32],
        now_s: u64,
        ingress: AnnounceIngress,
    ) {
        let envelopes = self
            .couriers
            .take_envelopes(recipient_key, now_s, ingress)
            .unwrap_or_default();
        for envelope in envelopes {
            let Ok(payload) = envelope.encode() else {
                continue;
            };
            let packet = self.build_packet(
                MessageType::CourierEnvelope,
                Some(peer),
                payload,
                chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
            );
            self.transmit(&packet, Some(peer), None).await;
        }
    }

    async fn dispatch_sync(&self, send: chatter_mesh_store::sync::SyncSend) {
        let packet = self.maybe_sign(send.packet, send.sign_before_send);
        self.transmit(&packet, send.to.as_ref(), None).await;
    }

    fn emit_dm(&self, peer: &PeerId, message_id: &str, content: &str, timestamp_ms: u64) {
        let nickname = self
            .state
            .with(|state| state.peers.nickname(peer, false).map(str::to_string))
            .unwrap_or_else(|| peer.as_str().to_string());
        let verified = self
            .state
            .with(|state| state.peers.info(peer).is_some_and(|p| p.verified_nickname));
        let inbound = InboundMessage {
            wire_id: message_id.to_string(),
            conversation: MeshConversation::Dm(peer.clone()),
            sender: peer.clone(),
            nickname: nickname.clone(),
            content: content.to_string(),
            timestamp_ms,
        };
        self.events.send(MeshEvent::Message(Box::new(MeshMessage {
            message: inbound.to_domain(),
            conversation: inbound.conversation,
            sender: peer.clone(),
            nickname,
            is_local: false,
            sender_verified: verified,
        })));
    }

    fn addressed_to_us(&self, packet: &BitchatPacket) -> bool {
        let local = self.identity.peer_id().to_routing_bytes();
        match (packet.recipient_id, local) {
            (Some(recipient), Some(local)) => recipient == local,
            _ => false,
        }
    }

    fn reconcile_peers(&mut self) {
        let now = now_millis();
        let removed = self.state.with(|state| {
            let presence = state.links.presence_by_peer();
            let changes = state.peers.reconcile_connectivity(now, &presence);
            for peer in &changes.disconnected {
                state.sessions.forget(peer);
            }
            changes
        });
        for removed in removed.removed {
            self.gossip.remove_announcement_for_peer(&removed.peer);
        }
        self.emit_peers();
    }

    fn emit_peers(&self) {
        self.events.send(MeshEvent::Peers(self.state.peers()));
    }

    fn emit_status(&self) {
        let status = self
            .state
            .status(self.nostr_state.connected_relay_count());
        self.events.send(MeshEvent::Status(status));
    }

    fn degraded(&self, message: String) {
        tracing::debug!(target: "mesh::ble", "{message}");
        self.events.send(MeshEvent::Degraded(message));
    }
}

/// Write a relayed packet, with the ingress link excluded.
///
/// A free function so the relay path can be spawned: it needs the radio and the
/// shared state and nothing else from the runtime.
async fn transmit_forwarded(
    radio: &Arc<dyn MeshLink>,
    state: &Arc<MeshState>,
    packet: &BitchatPacket,
    ingress: &LinkId,
) {
    let padding = fragment::pads_ble_frame(packet.msg_type);
    let Ok(encoded) = codec::encode(packet, padding) else {
        return;
    };
    let links = state.with(|inner| link_candidates_locked(inner));
    let request = OutboundPlanRequest::new(packet, encoded.len(), &links)
        // Split horizon: never send a packet back down the link it arrived on.
        .with_ingress_link(Some(ingress))
        .with_direct_announce_ttl(DIRECT_ANNOUNCE_TTL);
    let plan = planner::plan(&request);

    let frames: Vec<Vec<u8>> = match plan.fragment_chunk_size {
        Some(chunk) => {
            let mut id = [0u8; 8];
            id.copy_from_slice(&packet.packet_id()[..8]);
            match fragment::plan_fragments(packet, chunk, id) {
                Ok(fragments) => fragments
                    .iter()
                    .filter_map(|f| codec::encode(f, fragment::pads_ble_frame(f.msg_type)).ok())
                    .collect(),
                Err(_) => return,
            }
        }
        None => vec![encoded],
    };

    let priority = planner::priority(packet);
    let write_priority = chatter_mesh_link::backpressure::WritePriority {
        level: priority.level,
        suborder: priority.suborder,
    };

    for link in plan.selected.iter() {
        for frame in &frames {
            match radio.send(link, frame).await {
                Ok(()) => {}
                Err(error) if should_buffer(&error) => {
                    let admitted = state.with(|inner| {
                        inner.writes.enqueue(
                            link,
                            frame.clone(),
                            write_priority,
                            PENDING_WRITE_BUFFER_CAP_BYTES,
                        )
                    });
                    if !matches!(admitted, WriteEnqueue::Enqueued { .. }) {
                        tracing::warn!(
                            link = %link,
                            "mesh: outbound write buffer full, packet dropped"
                        );
                    }
                    state.with(|_inner| {
                    });
                }
                Err(_) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::mesh::mock::MockLink;
    use chatter_mesh_store::MemoryStorage;
    use chatter_mesh_store::storage::Storage;
    use rand_core_06::OsRng;

    fn runtime() -> (BleRuntime, Arc<MockLink>, Arc<MeshState>) {
        let radio = MockLink::shared();
        let identity = Arc::new(MeshIdentity::generate(&mut OsRng));
        let state = Arc::new(MeshState::new(identity.peer_id()));
        let nostr_state = Arc::new(NostrState::new());
        let events = MeshEvents::new(64, crate::services::mesh::timeline::temp_timeline());
        let storage = SharedStorage::new(Arc::new(MemoryStorage::new()) as Arc<dyn Storage>);
        let router = Arc::new(RouterHandle::spawn(super::super::router::RouterDeps {
            state: Arc::clone(&state),
            nostr_state: Arc::clone(&nostr_state),
            identity: Arc::clone(&identity),
            storage: Arc::new(MemoryStorage::new()),
            sealer: Arc::new(NoopSealer),
            favorites: Arc::new(super::super::transport::MeshFavorites::new()),
            ble: tokio::sync::mpsc::unbounded_channel().0,
            nostr: tokio::sync::mpsc::unbounded_channel().0,
            events: events.clone(),
            bridge_enabled: false,
        }));
        let runtime = BleRuntime::new(
            Arc::clone(&radio) as Arc<dyn MeshLink>,
            Arc::clone(&state),
            nostr_state,
            identity,
            Arc::new(MeshConfig::default()),
            events,
            router,
            storage,
        );
        (runtime, radio, state)
    }

    struct NoopSealer;

    impl chatter_mesh_store::outbox::Sealer for NoopSealer {
        fn seal(&self, plaintext: &[u8]) -> Result<Vec<u8>, chatter_mesh_store::StoreError> {
            Ok(plaintext.to_vec())
        }

        fn open(&self, sealed: &[u8]) -> Result<Vec<u8>, chatter_mesh_store::StoreError> {
            Ok(sealed.to_vec())
        }
    }

    #[tokio::test]
    async fn our_own_announce_coming_back_does_not_register_us_as_a_peer() {
        // The ingress guard admits a self-authored TTL-0 sync response on purpose,
        // so our own announce really does reach `on_announce`. If it registers, we
        // become our own peer: "N nearby" counts us, and a link binds to our own
        // identity. bitchat refuses it as `.reject(.selfAnnounce)`.
        let (mut runtime, _radio, state) = runtime();
        let local = runtime.identity.peer_id();
        let link_out = LinkId::new("/mock/out");
        state.with(|inner| {
            inner.links.link_up(link_out.clone(), LinkRole::Central);
            inner.links.set_writable(&link_out, true);
            inner.max_write.insert(link_out.clone(), 512);
        });

        // Build the announce exactly as we would send it, then feed it back.
        runtime.announce().await;
        let sent = _radio.writes();
        let packet = codec::decode(&sent[0].1).expect("decodable");
        assert_eq!(
            PeerId::from_bytes(&packet.sender_id),
            local,
            "the fixture must really be our own announce"
        );

        let link = LinkId::new("/mock/self");
        runtime.on_announce(&link, &packet, &local).await;

        assert!(
            state.with(|inner| inner.peers.info(&local).is_none()),
            "we must never appear in our own peer registry"
        );
        assert!(
            state.with(|inner| inner.links.peer_for(&link).is_none()),
            "and no link may be bound to our own identity"
        );
    }

    #[tokio::test]
    async fn a_message_of_ours_recovered_from_a_peer_is_attributed_to_us() {
        // Gossip sync hands back what we wrote — that is how a device with no
        // database gets its own history again. It must land as ours: verified
        // against our own key (we are deliberately not in our own registry) and
        // marked local, or our own words appear on screen as a stranger's.
        let (mut runtime, _radio, _state) = runtime();
        let local = runtime.identity.peer_id();
        let link_out = LinkId::new("/mock/out");
        _state.with(|inner| {
            inner.links.link_up(link_out.clone(), LinkRole::Central);
            inner.links.set_writable(&link_out, true);
            inner.max_write.insert(link_out.clone(), 512);
        });
        let mut events = runtime.events.subscribe();

        runtime.broadcast_public("MSG-SELF", "our own words").await;
        let packet = codec::decode(&_radio.writes()[0].1).expect("decodable");
        // Drain the local mirror `broadcast_public` emits for its own send.
        let _ = events.try_recv();

        runtime.on_public_message(&packet, &local);

        match events.try_recv() {
            Ok(MeshEvent::Message(message)) => {
                assert!(message.is_local, "a recovered self message is ours");
                assert_eq!(message.sender, local);
                assert_eq!(
                    super::super::mapping::text_of(&message.message),
                    Some("our own words")
                );
            }
            other => panic!("expected the recovered message, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_announce_is_signed_and_carries_both_public_keys() {
        let (mut runtime, radio, state) = runtime();
        // One writable link, so the announce has somewhere to go.
        let link = LinkId::new("/mock/one");
        state.with(|inner| {
            inner.links.link_up(link.clone(), LinkRole::Central);
            inner.links.set_writable(&link, true);
            inner.max_write.insert(link.clone(), 512);
        });

        runtime.announce().await;

        let writes = radio.writes();
        assert_eq!(writes.len(), 1, "the announce went out on the one link");
        let packet = codec::decode(&writes[0].1).expect("decodable");
        assert_eq!(packet.message_type(), Some(MessageType::Announce));

        let announcement =
            AnnouncementPacket::decode(&packet.payload).expect("announce payload decodes");
        assert_eq!(
            announcement.noise_public_key,
            runtime.identity.static_public().to_vec()
        );
        assert_eq!(
            announcement.signing_public_key,
            runtime.identity.signing_public().to_vec()
        );

        assert!(packet.signature.is_some(), "announces are signed");
        let signing_key = <[u8; 32]>::try_from(announcement.signing_public_key.as_slice())
            .expect("32 bytes");
        assert!(
            verify_packet(&packet, &signing_key),
            "the signature must verify against the advertised key, over the \
             packet preimage — a peer verifying `canonicalAnnounceBytes` \
             instead never enters anyone's registry"
        );
        assert!(
            packet.recipient_id.is_none(),
            "an announce is addressed to everyone by omitting the recipient"
        );
    }

    #[tokio::test]
    async fn nothing_is_written_when_no_link_is_writable() {
        let (mut runtime, radio, state) = runtime();
        let link = LinkId::new("/mock/one");
        // Bound and up, but not writable.
        state.with(|inner| {
            inner.links.link_up(link.clone(), LinkRole::Central);
            inner.max_write.insert(link.clone(), 512);
        });
        runtime.announce().await;
        assert_eq!(radio.write_count(), 0);
    }

    #[tokio::test]
    async fn a_public_message_is_mirrored_locally_and_recorded_as_our_own() {
        let (mut runtime, radio, state) = runtime();
        let link = LinkId::new("/mock/one");
        state.with(|inner| {
            inner.links.link_up(link.clone(), LinkRole::Central);
            inner.links.set_writable(&link, true);
            inner.max_write.insert(link.clone(), 512);
        });
        let mut events = runtime.events.subscribe();

        runtime.broadcast_public("MSG-1", "hello room").await;

        let writes = radio.writes();
        assert_eq!(writes.len(), 1);
        let packet = codec::decode(&writes[0].1).expect("decodable");
        assert_eq!(packet.message_type(), Some(MessageType::Message));
        assert!(packet.is_broadcast());
        // The three things a real bitchat checks before it will show this, and
        // the three the port originally got wrong: the payload is the message
        // text and nothing else, the packet is signed, and "everyone" is encoded
        // by omitting the recipient.
        assert_eq!(packet.payload, b"hello room");
        assert!(packet.recipient_id.is_none());
        let signing_key = runtime.identity.signing_public();
        assert!(
            verify_packet(&packet, &signing_key),
            "an unsigned public message is dropped before its payload is read"
        );

        // The echo of our own broadcast must be recognisable.
        assert!(state.with(|inner| inner.self_broadcasts.contains(&packet)));
        assert_eq!(
            state.with(|inner| inner.self_broadcasts.message_id(&packet).map(str::to_string)),
            Some("MSG-1".to_string())
        );

        match events.try_recv() {
            Ok(MeshEvent::Message(message)) => {
                assert!(message.is_local);
                assert_eq!(message.conversation, MeshConversation::Room);
                assert_eq!(
                    message.message.conversation_type,
                    crate::domain::message::ConversationType::Mesh
                );
            }
            other => panic!("expected a mirrored message, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_write_rejection_buffers_rather_than_dropping_the_packet() {
        let (mut runtime, radio, state) = runtime();
        let link = LinkId::new("/mock/one");
        state.with(|inner| {
            inner.links.link_up(link.clone(), LinkRole::Central);
            inner.links.set_writable(&link, true);
            inner.max_write.insert(link.clone(), 512);
        });
        radio.set_reject_writes(true);

        runtime.broadcast_public("MSG-1", "hello room").await;

        assert_eq!(radio.write_count(), 0, "the controller refused the write");
        assert_eq!(
            state.with(|inner| inner.writes.queued_count(&link)),
            1,
            "a rejected write is retryable and must not be discarded"
        );

        // Once the controller drains, the buffered write goes out.
        radio.set_reject_writes(false);
        runtime.flush_buffered_writes().await;
        assert_eq!(radio.write_count(), 1);
        assert_eq!(state.with(|inner| inner.writes.queued_count(&link)), 0);
    }

    #[tokio::test]
    async fn a_connected_link_is_treated_as_writable_immediately() {
        let (mut runtime, _radio, state) = runtime();
        let link = LinkId::new("/mock/one");
        runtime
            .on_link_event(LinkEvent::Connected {
                link: link.clone(),
                role: LinkRole::Peripheral,
                max_write: 244,
            })
            .await;
        assert!(state.with(|inner| inner.links.is_writable(&link)));
        assert_eq!(state.with(|inner| inner.max_write.get(&link).copied()), Some(244));
    }

    #[tokio::test]
    async fn a_disconnect_drops_every_piece_of_link_state() {
        let (mut runtime, _radio, state) = runtime();
        let link = LinkId::new("/mock/one");
        runtime
            .on_link_event(LinkEvent::Connected {
                link: link.clone(),
                role: LinkRole::Central,
                max_write: 244,
            })
            .await;
        runtime
            .on_link_event(LinkEvent::Disconnected { link: link.clone() })
            .await;
        assert!(!state.with(|inner| inner.links.contains(&link)));
        assert!(state.with(|inner| inner.max_write.get(&link).is_none()));
        assert!(state.link_candidates().is_empty());
    }

    #[tokio::test]
    async fn losing_the_adapter_clears_every_link() {
        let (mut runtime, _radio, state) = runtime();
        let link = LinkId::new("/mock/one");
        runtime
            .on_link_event(LinkEvent::Connected {
                link,
                role: LinkRole::Central,
                max_write: 244,
            })
            .await;
        runtime
            .on_link_event(LinkEvent::AdapterPowerChanged { powered: false })
            .await;
        assert!(state.with(|inner| inner.links.is_empty()));
        assert!(!state.with(|inner| inner.adapter_powered));
        assert!(state.link_candidates().is_empty());
    }

    #[tokio::test]
    async fn a_private_message_with_no_session_is_held_and_a_handshake_starts() {
        let (mut runtime, radio, state) = runtime();
        let link = LinkId::new("/mock/one");
        let peer = PeerId::from_str("bbbbbbbbbbbbbbbb");
        state.with(|inner| {
            inner.links.link_up(link.clone(), LinkRole::Central);
            inner.links.bind_if_unbound(&link, &peer);
            inner.links.set_writable(&link, true);
            inner.max_write.insert(link.clone(), 512);
        });

        runtime.send_private(&peer, "MSG-1", "secret").await;

        // The message is held, not sent in the clear.
        let held = state.with(|inner| inner.pending_dms.get(&peer).map(Vec::len));
        assert_eq!(held, Some(1));
        let writes = radio.writes();
        assert_eq!(writes.len(), 1, "only the handshake went out");
        let packet = codec::decode(&writes[0].1).expect("decodable");
        assert_eq!(packet.message_type(), Some(MessageType::NoiseHandshake));
        assert!(
            !writes[0].1.windows(6).any(|w| w == b"secret"),
            "plaintext must never reach the wire"
        );
    }
}

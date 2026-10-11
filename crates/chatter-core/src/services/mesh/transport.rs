//! `RouterTransport` over BLE and over Nostr, and the commands they submit.
//!
//! # The four predicates are four different questions
//!
//! The trait's own documentation is emphatic that these are not
//! interchangeable, and the router's decision tree is built entirely out of the
//! differences. Getting one wrong does not produce a compile error; it produces
//! messages that are silently never retained, or retained forever. So each
//! implementation below states which fact it is reporting:
//!
//! | | BLE | Nostr |
//! |---|---|---|
//! | `is_peer_connected` | a link is bound to the peer | never — Nostr has no links |
//! | `is_peer_reachable` | the registry's freshness window | we hold their pubkey |
//! | `can_deliver_promptly` | a *writable* link exists | **a relay is connected** |
//! | `can_deliver_securely` | an **established Noise session** | never |
//!
//! `can_deliver_securely` on the BLE side is the one that matters most: it is
//! the only predicate strong enough to send against without retaining a copy,
//! and answering it from the link binding rather than the session would make
//! every pre-handshake first message unrecoverable, because a link binding heals
//! on an announce whose "directness" rides on an unsigned TTL.
//!
//! Nostr's `can_deliver_promptly` is the second: a publish with no relay
//! connected only joins a queue waiting for internet that may never arrive, and
//! reporting that as prompt suppresses the courier deposit that is the message's
//! only other route.

use std::sync::Arc;

use chatter_mesh::ids::PeerId;
use chatter_mesh::packet::{BitchatPacket, NoisePayloadType, encode_noise_payload};
use chatter_mesh::tlv::courier_envelope::{CourierEnvelope, epoch_day, recipient_tag};
use chatter_mesh::tlv::file::BitchatFilePacket;
use chatter_mesh::tlv::file_stream::FileManifest;
use chatter_mesh::tlv::private_message::PrivateMessagePacket;
use chatter_mesh_noise::MeshIdentity;
use chatter_mesh_store::bridge_drop::DropOperationId;
use chatter_mesh_store::courier::INITIAL_COPIES;
use chatter_mesh_store::router::CourierDirectory;
use chatter_mesh_store::{MESSAGE_TTL_SECS, PeerSnapshot, RouterTransport};

use super::state::{MeshState, NostrState, now_secs, recipient_pubkey};

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// Work for the radio loop.
///
/// The transports are synchronous — `RouterTransport` takes `&self` and returns
/// immediately — so anything that needs the radio is submitted rather than
/// awaited. "Accepted for delivery" therefore means "queued for a loop that can
/// carry it", which is exactly what the trait says acceptance means.
#[derive(Debug)]
pub enum BleCommand {
    /// Encrypt and send a private message, handshaking first if necessary.
    PrivateMessage {
        peer: PeerId,
        message_id: String,
        content: String,
    },
    /// Hand a sealed envelope to a courier over a directed packet.
    Courier {
        peer: PeerId,
        envelope: CourierEnvelope,
    },
    /// Put a packet on every selected link.
    Broadcast { packet: BitchatPacket, sign: bool },
    /// Put a packet on the links bound to one peer.
    Directed {
        peer: PeerId,
        packet: BitchatPacket,
        sign: bool,
    },
    /// Acknowledge a message we received.
    Ack {
        peer: PeerId,
        message_id: String,
        payload_type: NoisePayloadType,
    },
    /// Broadcast a public message into the mesh room.
    PublicMessage { message_id: String, content: String },
    /// Broadcast a file into the mesh room.
    ///
    /// The TLV arrives already built and already size-checked: the service reads
    /// the file and refuses an oversized one before anything is queued, so the
    /// runtime's remaining job is to sign, fragment and mirror it. Nothing here
    /// chunks — the whole payload is one packet that the fragmenter splits for the
    /// radio, and there is no resume above it.
    ///
    /// Only used below [`MAX_SINGLE_PACKET_FILE_BYTES`], because above that a
    /// single packet cannot reassemble inside the fragmenter's lifetime window at
    /// all. See [`Self::StreamedFile`].
    ///
    /// [`MAX_SINGLE_PACKET_FILE_BYTES`]: chatter_mesh::tlv::file_stream::MAX_SINGLE_PACKET_FILE_BYTES
    FileTransfer {
        message_id: String,
        file: BitchatFilePacket,
    },
    /// Stream a file into the mesh room as a chunked, resumable transfer.
    ///
    /// Carries a **path**, deliberately, where [`Self::FileTransfer`] carries
    /// bytes. The file is never read whole on this side of the channel: the
    /// service hashes it in windows to build the manifest, and the runtime reads
    /// one chunk-sized window at a time as it paces the transfer out. A command
    /// holding a `Vec<u8>` would have undone the entire point before the runtime
    /// even saw it.
    StreamedFile {
        message_id: String,
        manifest: FileManifest,
        path: std::path::PathBuf,
    },
    /// Announce our identity now, out of the normal cadence.
    Announce,
    /// Stop the loop.
    Stop,
}

/// Work for the relay loop.
#[derive(Debug)]
pub enum NostrCommand {
    /// Gift-wrap and publish a private message.
    Dm {
        peer: PeerId,
        message_id: String,
        content: String,
    },
    /// Publish a public geohash message.
    GeohashMessage {
        cell: String,
        message_id: String,
        content: String,
        nickname: String,
    },
    /// Acknowledge a gift-wrapped message.
    Ack {
        peer: PeerId,
        message_id: String,
        payload_type: NoisePayloadType,
    },
    /// Subscribe to a geohash cell.
    Join(String),
    /// Unsubscribe from a geohash cell.
    Leave(String),
    /// Park a sealed courier drop on the relays.
    ///
    /// `operation` is the bridge courier's own handle for the attempt; it comes
    /// back verbatim so the publish result can be resolved against it. There is
    /// no public constructor for it, which is exactly right — only the bridge
    /// may mint one.
    PublishDrop {
        operation: DropOperationId,
        tag: [u8; 16],
        sealed: Vec<u8>,
        expires_at_unix_secs: u64,
        publisher_secret: [u8; 32],
    },
    /// Watch a set of recipient tags for drops addressed to us or to peers we
    /// bridge for.
    WatchTags(Vec<[u8; 16]>),
    /// Stop watching.
    CloseWatch,
    /// Stop the loop.
    Stop,
}

/// The identity lookups the router needs to address couriered mail.
pub struct MeshDirectory {
    state: Arc<MeshState>,
}

impl MeshDirectory {
    #[must_use]
    pub fn new(state: Arc<MeshState>) -> Self {
        Self { state }
    }
}

impl CourierDirectory for MeshDirectory {
    fn noise_key(&self, peer: &PeerId) -> Option<[u8; 32]> {
        // A peer ID that *is* a full Noise key needs no lookup. This is the
        // normal case for a peer addressed from the UI, where the stable
        // 64-hex form is what gets stored.
        if let Some(bytes) = peer.noise_key()
            && let Ok(key) = <[u8; 32]>::try_from(bytes.as_slice())
        {
            return Some(key);
        }
        // Otherwise: whatever the announce advertised, or a completed handshake
        // proved.
        let from_mesh = self.state.with(|state| {
            state
                .peers
                .info(peer)
                .and_then(|record| record.noise_public_key.clone())
                .and_then(|bytes| <[u8; 32]>::try_from(bytes.as_slice()).ok())
                .or_else(|| state.sessions.remote_static(peer))
        });
        // `None` for a Nostr-only peer: a secp256k1 pubkey is not a Noise static
        // key and cannot be sealed to. Reporting one here would produce
        // envelopes nobody can open, so the courier path stays closed for them —
        // the bridge drop is their offline route, not the mesh courier.
        from_mesh
    }
}

// ---------------------------------------------------------------------------
// BLE
// ---------------------------------------------------------------------------

/// The mesh radio, as the router sees it.
pub struct BleTransport {
    state: Arc<MeshState>,
    identity: Arc<MeshIdentity>,
    commands: Arc<super::CommandBus<BleCommand>>,
}

impl BleTransport {
    #[must_use]
    pub fn new(
        state: Arc<MeshState>,
        identity: Arc<MeshIdentity>,
        commands: Arc<super::CommandBus<BleCommand>>,
    ) -> Self {
        Self {
            state,
            identity,
            commands,
        }
    }

    /// Seal a message to `recipient_key` and wrap it as a courier envelope.
    ///
    /// The plaintext never leaves this function: what the envelope carries is a
    /// Noise `X` seal to one of the recipient's one-time prekeys when we hold
    /// their fresh bundle, else to their static key — opaque to every courier
    /// that relays it either way.
    fn seal_envelope(
        &self,
        content: &str,
        message_id: &str,
        recipient_key: &[u8; 32],
        now_unix_secs: u64,
    ) -> Option<CourierEnvelope> {
        let inner = PrivateMessagePacket::new(message_id.to_string(), content.to_string())
            .encode()
            .ok()?;
        let payload = encode_noise_payload(NoisePayloadType::PrivateMessage, &inner);
        let (ciphertext, prekey_id) = super::prekeys::seal_offline(
            &self.state,
            &payload,
            recipient_key,
            &self.identity,
            now_unix_secs.saturating_mul(1_000),
        )?;
        let expiry_ms = now_unix_secs
            .saturating_add(MESSAGE_TTL_SECS)
            .saturating_mul(1_000);
        let envelope = CourierEnvelope::new(
            recipient_tag(recipient_key, epoch_day(now_unix_secs)),
            expiry_ms,
            ciphertext,
            INITIAL_COPIES,
            prekey_id,
        );
        // Refuse anything that cannot be put on the wire rather than handing a
        // courier an envelope it will fail to forward.
        envelope.encode().ok()?;
        Some(envelope)
    }
}

impl RouterTransport for BleTransport {
    /// A live link binding exists. Forgeable — never the sole basis for
    /// discarding a retained copy.
    fn is_peer_connected(&self, peer: &PeerId) -> bool {
        self.state.is_connected(peer)
    }

    /// The registry's freshness window: seen recently, radio possibly already
    /// gone. A send against this can silently go nowhere.
    fn is_peer_reachable(&self, peer: &PeerId) -> bool {
        self.state.is_reachable(peer) || self.state.is_connected(peer)
    }

    /// The write will actually leave the device: a link bound to this peer is up
    /// *and writable*. `Connected` already means writable, and the store keeps
    /// that flag current, so this is the honest "it goes out now" answer.
    fn can_deliver_promptly(&self, peer: &PeerId) -> bool {
        self.state.can_write_to(peer)
    }

    /// An **established Noise session**, not a live link.
    ///
    /// This is the only predicate the router trusts enough to retain nothing
    /// against. A link binding is not enough: bindings heal on "direct"
    /// announces whose directness rides on the unsigned TTL, so a replayed
    /// announce can bind an absent peer's ID to the replayer's link, where the
    /// send stalls on a handshake that can never complete.
    fn can_deliver_securely(&self, peer: &PeerId) -> bool {
        self.state.has_session(peer) && self.state.can_write_to(peer)
    }

    fn peer_snapshots(&self) -> Vec<PeerSnapshot> {
        self.state.peer_snapshots()
    }

    fn send_private(&self, content: &str, to: &PeerId, message_id: &str) -> bool {
        // Acceptance is not delivery. What is being claimed here is only that
        // something can carry it: a writable link, or a session whose link is
        // momentarily busy. With neither, the router must fall through to the
        // courier branch rather than believe this was sent.
        if !self.state.can_write_to(to) && !self.state.has_session(to) {
            return false;
        }
        self.commands
            .send(BleCommand::PrivateMessage {
                peer: to.clone(),
                message_id: message_id.to_string(),
                content: content.to_string(),
            })
            .is_ok()
    }

    fn send_courier(
        &self,
        content: &str,
        message_id: &str,
        recipient_key: &[u8; 32],
        via: &[PeerId],
    ) -> Vec<PeerId> {
        let now = now_secs();
        let Some(envelope) = self.seal_envelope(content, message_id, recipient_key, now) else {
            return Vec::new();
        };

        // Return only the couriers that actually took it. Crediting every
        // candidate would burn the message's courier slots against carriers that
        // never received it.
        via.iter()
            .filter(|peer| self.state.can_write_to(peer))
            .filter(|peer| {
                self.commands
                    .send(BleCommand::Courier {
                        peer: (*peer).clone(),
                        envelope: envelope.clone(),
                    })
                    .is_ok()
            })
            .cloned()
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Nostr
// ---------------------------------------------------------------------------

/// Nostr relays, as the router sees them.
pub struct NostrTransport {
    state: Arc<NostrState>,
    commands: Arc<super::CommandBus<NostrCommand>>,
}

impl NostrTransport {
    #[must_use]
    pub fn new(state: Arc<NostrState>, commands: Arc<super::CommandBus<NostrCommand>>) -> Self {
        Self { state, commands }
    }
}

impl RouterTransport for NostrTransport {
    /// Never. Nostr has no link bindings at all — there is no socket to the
    /// peer, only a relay both sides happen to use. Reporting `true` here would
    /// put the router on the "connected" branches, whose whole premise is a
    /// physical link that either carries the packet or does not.
    fn is_peer_connected(&self, _peer: &PeerId) -> bool {
        false
    }

    /// We hold a gift-wrap recipient key for them. Deliberately independent of
    /// relay state: bitchat treats any peer with a known npub as reachable
    /// even with every relay down, and the *prompt* predicate is what corrects
    /// for that.
    fn is_peer_reachable(&self, peer: &PeerId) -> bool {
        self.state.knows_peer(peer)
    }

    /// **False whenever no relay is connected.** With no socket a publish only
    /// joins a queue waiting for internet that may never arrive, and the router
    /// reads "reachable but not prompt" as the signal to also hand a sealed copy
    /// to couriers. Collapsing this into `is_peer_reachable` would silently
    /// delete that fallback.
    fn can_deliver_promptly(&self, peer: &PeerId) -> bool {
        self.state.knows_peer(peer) && self.state.any_relay_connected()
    }

    /// Never. A gift wrap is end-to-end encrypted, but publishing it to a relay
    /// is not a completed delivery — the recipient may never come online to
    /// fetch it — and this predicate is the one that licenses retaining nothing.
    fn can_deliver_securely(&self, _peer: &PeerId) -> bool {
        false
    }

    /// Empty. Courier mail is handed to a peer over a link and a Nostr peer has
    /// none; more fundamentally, a secp256k1 pubkey is not a Noise static key,
    /// so nothing here can be sealed as a mesh envelope. The relay-side offline
    /// route is the bridge drop, which the router reaches through
    /// `set_bridge_deposit` rather than through this method.
    fn peer_snapshots(&self) -> Vec<PeerSnapshot> {
        Vec::new()
    }

    fn send_private(&self, content: &str, to: &PeerId, message_id: &str) -> bool {
        if recipient_pubkey(to).is_none() {
            return false;
        }
        self.commands
            .send(NostrCommand::Dm {
                peer: to.clone(),
                message_id: message_id.to_string(),
                content: content.to_string(),
            })
            .is_ok()
    }

    fn send_courier(
        &self,
        _content: &str,
        _message_id: &str,
        _recipient_key: &[u8; 32],
        _via: &[PeerId],
    ) -> Vec<PeerId> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::os_rng;
    use chatter_mesh_link::{LinkId, LinkRole};
    use tokio::sync::mpsc;

    fn peer(s: &str) -> PeerId {
        PeerId::from_str(s)
    }

    fn ble() -> (
        BleTransport,
        Arc<MeshState>,
        mpsc::UnboundedReceiver<BleCommand>,
    ) {
        let identity = Arc::new(MeshIdentity::generate(&mut os_rng()));
        let state = Arc::new(MeshState::new(identity.peer_id()));
        let (bus, rx) = crate::services::mesh::CommandBus::new();
        (
            BleTransport::new(Arc::clone(&state), identity, bus),
            state,
            rx,
        )
    }

    fn connect(state: &MeshState, link: &LinkId, remote: &PeerId, writable: bool) {
        state.with(|inner| {
            inner.links.link_up(link.clone(), LinkRole::Central);
            inner.links.bind_if_unbound(link, remote);
            inner.links.set_writable(link, writable);
            inner.max_write.insert(link.clone(), 512);
        });
    }

    #[test]
    fn ble_connected_does_not_imply_prompt_or_secure() {
        let (transport, state, _rx) = ble();
        let remote = peer("bbbbbbbbbbbbbbbb");
        let link = LinkId::new("/link/1");

        connect(&state, &link, &remote, false);
        assert!(transport.is_peer_connected(&remote));
        assert!(
            !transport.can_deliver_promptly(&remote),
            "a bound but unwritable link cannot carry a write"
        );
        assert!(
            !transport.can_deliver_securely(&remote),
            "no session means no secure delivery, however live the link"
        );
    }

    #[test]
    fn ble_writable_is_prompt_but_still_not_secure_without_a_session() {
        let (transport, state, _rx) = ble();
        let remote = peer("bbbbbbbbbbbbbbbb");
        let link = LinkId::new("/link/1");

        connect(&state, &link, &remote, true);
        assert!(transport.can_deliver_promptly(&remote));
        assert!(
            !transport.can_deliver_securely(&remote),
            "this is the branch that must retain a copy"
        );
    }

    #[test]
    fn ble_refuses_to_accept_a_send_with_nothing_to_carry_it() {
        let (transport, _state, mut rx) = ble();
        let remote = peer("bbbbbbbbbbbbbbbb");
        assert!(!transport.send_private("hi", &remote, "m1"));
        assert!(rx.try_recv().is_err(), "nothing may be queued");
    }

    #[test]
    fn ble_accepts_and_queues_when_a_writable_link_exists() {
        let (transport, state, mut rx) = ble();
        let remote = peer("bbbbbbbbbbbbbbbb");
        connect(&state, &LinkId::new("/link/1"), &remote, true);

        assert!(transport.send_private("hi", &remote, "m1"));
        match rx.try_recv() {
            Ok(BleCommand::PrivateMessage {
                peer,
                message_id,
                content,
            }) => {
                assert_eq!(peer, remote);
                assert_eq!(message_id, "m1");
                assert_eq!(content, "hi");
            }
            other => panic!("expected a private message command, got {other:?}"),
        }
    }

    #[test]
    fn ble_couriers_only_to_peers_it_can_write_to() {
        let (transport, state, mut rx) = ble();
        let writable = peer("bbbbbbbbbbbbbbbb");
        let unwritable = peer("cccccccccccccccc");
        connect(&state, &LinkId::new("/link/1"), &writable, true);
        connect(&state, &LinkId::new("/link/2"), &unwritable, false);

        let accepted = transport.send_courier(
            "secret",
            "m1",
            &[7u8; 32],
            &[writable.clone(), unwritable.clone()],
        );
        assert_eq!(accepted, vec![writable]);
        assert!(matches!(rx.try_recv(), Ok(BleCommand::Courier { .. })));
        assert!(
            rx.try_recv().is_err(),
            "exactly one envelope was handed over"
        );
    }

    #[test]
    fn a_courier_envelope_carries_no_plaintext() {
        let (transport, _state, _rx) = ble();
        let envelope = transport
            .seal_envelope("attack at dawn", "m1", &[9u8; 32], 1_700_000_000)
            .expect("sealed");
        let haystack = envelope.encode().expect("encodable");
        assert!(
            !haystack
                .windows(b"attack at dawn".len())
                .any(|w| w == b"attack at dawn"),
            "plaintext must never reach an envelope"
        );
        assert_eq!(envelope.copies, INITIAL_COPIES);
    }

    fn nostr() -> (
        NostrTransport,
        Arc<NostrState>,
        mpsc::UnboundedReceiver<NostrCommand>,
    ) {
        let state = Arc::new(NostrState::new());
        let (bus, rx) = crate::services::mesh::CommandBus::new();
        (NostrTransport::new(Arc::clone(&state), bus), state, rx)
    }

    fn nostr_peer() -> PeerId {
        // `nostr_` plus the full 64-hex x-only pubkey: `recipient_pubkey` needs the
        // whole key to encrypt to, so this form is required rather than incidental.
        peer(&format!("nostr_{}", "a".repeat(64)))
    }

    #[test]
    fn nostr_is_never_connected_and_never_secure() {
        let (transport, state, _rx) = nostr();
        let remote = nostr_peer();
        state.with(|inner| {
            inner
                .connected_relays
                .insert("wss://relay.example".to_string());
        });
        assert!(!transport.is_peer_connected(&remote));
        assert!(!transport.can_deliver_securely(&remote));
        assert!(transport.peer_snapshots().is_empty());
    }

    #[test]
    fn nostr_is_reachable_without_a_relay_but_never_prompt() {
        let (transport, _state, _rx) = nostr();
        let remote = nostr_peer();
        assert!(
            transport.is_peer_reachable(&remote),
            "a known pubkey is reachable even with every relay down"
        );
        assert!(
            !transport.can_deliver_promptly(&remote),
            "with no relay a publish only joins a queue"
        );
    }

    #[test]
    fn nostr_becomes_prompt_once_a_relay_connects() {
        let (transport, state, _rx) = nostr();
        let remote = nostr_peer();
        state.with(|inner| {
            inner
                .connected_relays
                .insert("wss://relay.example".to_string());
        });
        assert!(transport.can_deliver_promptly(&remote));
    }

    #[test]
    fn nostr_refuses_peers_it_cannot_address() {
        let (transport, _state, mut rx) = nostr();
        let mesh_only = peer("abcdef0123456789");
        assert!(!transport.is_peer_reachable(&mesh_only));
        assert!(!transport.send_private("hi", &mesh_only, "m1"));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn nostr_queues_a_dm_for_a_peer_it_can_address() {
        let (transport, _state, mut rx) = nostr();
        let remote = nostr_peer();
        assert!(transport.send_private("hi", &remote, "m1"));
        assert!(matches!(rx.try_recv(), Ok(NostrCommand::Dm { .. })));
    }

    #[test]
    fn nostr_never_couriers() {
        let (transport, _state, _rx) = nostr();
        let remote = nostr_peer();
        assert!(
            transport
                .send_courier("x", "m1", &[1u8; 32], &[remote])
                .is_empty()
        );
    }

    #[test]
    fn the_directory_reads_a_key_straight_out_of_a_full_key_peer_id() {
        let identity = Arc::new(MeshIdentity::generate(&mut os_rng()));
        let state = Arc::new(MeshState::new(identity.peer_id()));
        let directory = MeshDirectory::new(state);

        let key = identity.static_public();
        let as_peer = PeerId::from_str(&hex::encode(key));
        assert_eq!(directory.noise_key(&as_peer), Some(key));
    }

    #[test]
    fn the_directory_has_no_key_for_a_nostr_only_peer() {
        let identity = MeshIdentity::generate(&mut os_rng());
        let directory = MeshDirectory::new(Arc::new(MeshState::new(identity.peer_id())));
        // A secp256k1 pubkey is not a Noise static key; sealing to it would
        // produce envelopes nobody can open.
        assert!(directory.noise_key(&nostr_peer()).is_none());
    }
}

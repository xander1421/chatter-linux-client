use super::*;
use crate::crypto::os_rng;
use crate::services::mesh::events::MeshDelivery;
use crate::services::mesh::mock::MockLink;
use chatter_mesh_store::MemoryStorage;
use chatter_mesh_store::storage::Storage;

fn runtime() -> (BleRuntime, Arc<MockLink>, Arc<MeshState>) {
    let (runtime, radio, state, _timeline) = runtime_parts();
    (runtime, radio, state)
}

/// The same runtime, with the timeline it stores through.
///
/// Only the file tests need it: a file has to be readable back out of the
/// store to prove anything, because unlike a text message the row is only a
/// pointer to bytes that landed somewhere else.
fn runtime_parts() -> (
    BleRuntime,
    Arc<MockLink>,
    Arc<MeshState>,
    Arc<crate::services::mesh::timeline::MeshTimeline>,
) {
    let radio = MockLink::shared();
    let identity = Arc::new(MeshIdentity::generate(&mut os_rng()));
    let state = Arc::new(MeshState::new(identity.peer_id()));
    let nostr_state = Arc::new(NostrState::new());
    let timeline = crate::services::mesh::timeline::temp_timeline();
    let events = MeshEvents::new(64, Arc::clone(&timeline));
    let storage = Arc::new(MemoryStorage::new()) as Arc<dyn Storage>;
    let router = Arc::new(RouterHandle::spawn(super::super::router::RouterDeps {
        state: Arc::clone(&state),
        nostr_state: Arc::clone(&nostr_state),
        identity: Arc::clone(&identity),
        storage: Arc::new(MemoryStorage::new()),
        sealer: Arc::new(NoopSealer),
        ble: crate::services::mesh::CommandBus::new().0,
        nostr: crate::services::mesh::CommandBus::new().0,
        events: events.clone(),
        bridge_enabled: false,
    }));
    let binding_dir = tempfile::tempdir().expect("binding store temp dir");
    let binding_db = Arc::new(
        crate::storage::Database::open(&binding_dir.path().join("mesh_bindings.db"), "testpass")
            .expect("binding store database"),
    );
    binding_db
        .run_migrations()
        .expect("binding store migrations");
    crate::test_tempdir::hold(binding_dir);
    let runtime = BleRuntime::new(
        Arc::clone(&radio) as Arc<dyn MeshLink>,
        Arc::clone(&state),
        nostr_state,
        identity,
        Arc::new(MeshConfig::default()),
        events,
        router,
        storage,
        Arc::new(NoopSealer),
        Arc::new(MeshBindingStore::new(binding_db.clone())),
        Arc::new(PrivateChannelStore::new(binding_db.clone())),
        Arc::new(KeyStore::new(binding_db)),
        Arc::new(parking_lot::Mutex::new(())),
    );
    (runtime, radio, state, timeline)
}

/// A peer in the registry with a known signing key, as a verified announce
/// would have left it. Returns the identity that has to sign for it.
fn register_verified_peer(state: &MeshState, peer: &PeerId, nickname: &str) -> MeshIdentity {
    let identity = MeshIdentity::generate(&mut os_rng());
    let announcement = AnnouncementPacket::new(
        nickname.to_string(),
        identity.static_public().to_vec(),
        identity.signing_public().to_vec(),
        None,
        None,
        None,
    );
    state.with(|inner| {
        inner
            .peers
            .upsert_verified_announce(peer, &announcement, true, now_millis());
    });
    identity
}

#[tokio::test]
async fn leave_requires_the_pinned_signature_and_preserves_the_pin() {
    let (mut runtime, _radio, state) = runtime();
    let peer = PeerId::from_str("bbbbbbbbbbbbbbbb");
    let identity = register_verified_peer(&state, &peer, "Alice");
    let attacker = MeshIdentity::generate(&mut os_rng());
    let link = LinkId::new("/mock/leave");
    let mut packet = BitchatPacket::new(
        MessageType::Leave,
        peer.to_routing_bytes().unwrap(),
        None,
        now_millis(),
        Vec::new(),
        chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
    );
    assert!(!runtime.handle_typed(&link, &packet, &peer).await);
    packet.signature = Some(sign_packet(&attacker, &packet).unwrap());
    assert!(!runtime.handle_typed(&link, &packet, &peer).await);
    assert!(state.with(|s| s.peers.info(&peer).is_some()));
    packet.signature = Some(sign_packet(&identity, &packet).unwrap());
    assert!(runtime.handle_typed(&link, &packet, &peer).await);
    assert!(state.with(|s| s.peers.info(&peer).is_none()));
    assert!(state.with(|s| s.peers.pinned(&peer).is_some()));
}

/// One file transfer on the wire, optionally signed.
fn file_packet(
    sender: &PeerId,
    identity: &MeshIdentity,
    file: &BitchatFilePacket,
    recipient: Option<&PeerId>,
    sign: bool,
) -> BitchatPacket {
    let mut packet = BitchatPacket::new(
        MessageType::FileTransfer,
        sender.to_routing_bytes().expect("routing bytes"),
        recipient.and_then(PeerId::to_routing_bytes),
        now_millis(),
        file.encode().expect("encodes"),
        chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
    );
    if sign {
        packet.signature = Some(sign_packet(identity, &packet).expect("signs"));
    }
    packet
}

fn sample_file() -> BitchatFilePacket {
    BitchatFilePacket::new(
        Some("note.txt".to_string()),
        None,
        Some("text/plain".to_string()),
        b"the contents".to_vec(),
    )
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
include!("identity_and_sync.rs");
include!("messaging_and_security.rs");
include!("streamed_files.rs");
include!("radio_and_pacing.rs");

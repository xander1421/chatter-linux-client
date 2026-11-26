//! Compare the server-visible RPC order, connections, credentials, exact sizes and routing.
use super::{
    private_wire_fixture::{grpc_reply, read_wire, repair_bundle},
    two_members_with_ids_at,
};
use crate::client::ChatterClient;
use crate::domain::{ChannelId, ChannelType, UserId};
use crate::services::{
    channel_event_service::ChannelEventService,
    gateway::{MessageGateway, RouteResult},
    message::{
        MessageService,
        dm_bootstrap::tests::{cache_fixture_sender_certificate, seed_tokens},
    },
};
use crate::sync::channel_sync::ChannelSync;
use chatter_proto::chat_v1 as pb;
use libsignal_protocol::SealedSenderV2SentMessage;
use prost::Message as _;
use rand::{SeedableRng, rngs::StdRng};
use std::{
    collections::BTreeMap,
    convert::Infallible,
    net::SocketAddr,
    sync::{Arc, Mutex},
};
use tonic::{
    body::Body,
    transport::{Server, server::TcpConnectInfo},
};

#[derive(Debug, PartialEq, Eq)]
struct Rpc {
    phase: &'static str,
    connection: usize,
    endpoint: String,
    authorization: Option<String>,
    blind_credential: bool,
    access_key: bool,
    payload_size: usize,
    routing: Routing,
}

#[derive(Debug, PartialEq, Eq)]
enum Routing {
    Prekeys(String, String),
    Deposit(String, i32, Vec<(u32, u32, usize)>),
    Group(String, usize, Vec<(String, Vec<(u32, u32)>)>),
    Other,
}

struct ServerState {
    phase: &'static str,
    connections: BTreeMap<SocketAddr, usize>,
    request_ids: BTreeMap<String, usize>,
    inbox: BTreeMap<String, Vec<pb::DeviceMessage>>,
    calls: Vec<Rpc>,
    bundles: BTreeMap<String, Vec<pb::SignalPreKeyBundle>>,
    listing: Option<pb::Channel>,
    cdn_url: String,
    stale: Option<String>,
    lose_response: bool,
}

impl ServerState {
    fn new(bundles: BTreeMap<String, Vec<pb::SignalPreKeyBundle>>) -> Self {
        Self {
            phase: "cold",
            connections: BTreeMap::new(),
            request_ids: BTreeMap::new(),
            inbox: BTreeMap::new(),
            calls: Vec::new(),
            bundles,
            listing: None,
            cdn_url: "https://cdn.test/fixture".into(),
            stale: None,
            lose_response: false,
        }
    }

    fn phase(&mut self, phase: &'static str) {
        self.phase = phase;
    }

    async fn call(
        state: Arc<Mutex<Self>>,
        request: http::Request<Body>,
    ) -> Result<http::Response<Body>, Infallible> {
        let (head, body) = request.into_parts();
        let remote = head
            .extensions
            .get::<TcpConnectInfo>()
            .unwrap()
            .remote_addr()
            .unwrap();
        let payload = read_wire(body).await;
        let endpoint = head.uri.path().to_owned();
        let mut state = state.lock().unwrap();
        let authorization = head
            .headers
            .get(http::header::AUTHORIZATION)
            .map(|value| value.to_str().unwrap().to_owned());
        let account = || {
            use base64::Engine as _;
            let credential = authorization
                .as_ref()
                .unwrap()
                .strip_prefix("Basic ")
                .unwrap();
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(credential)
                .unwrap();
            String::from_utf8(decoded)
                .unwrap()
                .split_once('.')
                .unwrap()
                .0
                .to_owned()
        };
        let (routing, reply) = match endpoint.rsplit('/').next().unwrap() {
            "ListChannels" => (
                Routing::Other,
                grpc_reply(pb::ListChannelsResponse {
                    channels: state.listing.clone().into_iter().collect(),
                    ..Default::default()
                }),
            ),
            "GetChannelMembers" => {
                let members = state.listing.as_ref().unwrap().members.clone();
                (
                    Routing::Other,
                    grpc_reply(pb::GetChannelMembersResponse {
                        total: members.len() as i32,
                        members,
                    }),
                )
            }
            "GetChannelChanges" => (
                Routing::Other,
                grpc_reply(pb::GetChannelChangesResponse::default()),
            ),
            "GetPrivateAttachmentUploadUrl" => (
                Routing::Other,
                grpc_reply(pb::GetAttachmentUploadUrlResponse {
                    cdn_key: if state.phase == "selected GIF" {
                        "pa_qrstuvwxyzabcdefghijkl"
                    } else {
                        "pa_abcdefghijklmnopqrstuv"
                    }
                    .into(),
                    upload_url: state.cdn_url.clone(),
                    expires_at: i64::MAX,
                }),
            ),
            "ConfirmAttachmentUpload" => (
                Routing::Other,
                grpc_reply(pb::ConfirmAttachmentUploadResponse { confirmed: true }),
            ),
            "GetPrivateAttachmentDownloadUrl" => (
                Routing::Other,
                grpc_reply(pb::GetAttachmentDownloadUrlResponse {
                    download_url: state.cdn_url.clone(),
                    expires_at: i64::MAX,
                }),
            ),
            "SearchPrivateGifs" => (
                Routing::Other,
                grpc_reply(pb::SearchGifsResponse {
                    results: vec![pb::GifResult {
                        preview_url: "https://media.example/fixture.gif".into(),
                        ..Default::default()
                    }],
                    ..Default::default()
                }),
            ),
            "GetPrivateTrendingGifs" => (
                Routing::Other,
                grpc_reply(pb::GetTrendingGifsResponse::default()),
            ),
            "GetPrivateGifMedia" => (
                Routing::Other,
                grpc_reply(pb::GetGifMediaResponse {
                    data: b"GIF89a".to_vec(),
                    ..Default::default()
                }),
            ),
            "GetDeviceMessages" => (
                Routing::Other,
                grpc_reply(pb::GetDeviceMessagesResponse {
                    messages: state.inbox.get(&account()).cloned().unwrap_or_default(),
                    has_more: false,
                }),
            ),
            "AcknowledgeMessages" => {
                let request = pb::AcknowledgeMessagesRequest::decode(payload.as_slice()).unwrap();
                let pending = state.inbox.entry(account()).or_default();
                pending.retain(|message| !request.message_guids.contains(&message.message_guid));
                (
                    Routing::Other,
                    grpc_reply(pb::AcknowledgeMessagesResponse::default()),
                )
            }
            "GetPreKeyBundle" => {
                use pb::get_pre_key_bundle_response::Result as Bundles;
                let request = pb::GetPreKeyBundleRequest::decode(payload.as_slice()).unwrap();
                let bundles = &state.bundles[&request.user_id];
                let result = if request.device_id == "*" {
                    Some(Bundles::Devices(pb::PreKeyBundleDeviceList {
                        bundles: bundles.clone(),
                        identity_key: bundles[0].identity_key.clone(),
                    }))
                } else {
                    bundles
                        .iter()
                        .find(|b| request.device_id == b.device_id.to_string())
                        .cloned()
                        .map(Bundles::Bundle)
                };
                let reply = grpc_reply(pb::GetPreKeyBundleResponse { result });
                (Routing::Prekeys(request.user_id, request.device_id), reply)
            }
            "SendSealedMessage" => {
                let request = pb::SendSealedMessageRequest::decode(payload.as_slice()).unwrap();
                let devices = request
                    .messages
                    .iter()
                    .map(|message| {
                        (
                            message.device_id,
                            message.registration_id,
                            message.sealed_message.as_ref().unwrap().sealed_body.len(),
                        )
                    })
                    .collect();
                let reply = grpc_reply(pb::SendSealedMessageResponse {
                    success: true,
                    delivered_to_devices: request.messages.iter().map(|m| m.device_id).collect(),
                    ..Default::default()
                });
                (
                    Routing::Deposit(request.destination_user_id, request.delivery, devices),
                    reply,
                )
            }
            "SendGroupMessage" => {
                let request = pb::SendGroupMessageRequest::decode(payload.as_slice()).unwrap();
                let sealed =
                    SealedSenderV2SentMessage::parse(&request.multi_recipient_message).unwrap();
                let roster = sealed
                    .recipients
                    .iter()
                    .map(|(user, recipient)| {
                        (
                            user.service_id_string(),
                            recipient
                                .devices
                                .iter()
                                .map(|(device, registration)| {
                                    (u32::from(*device), u32::from(*registration))
                                })
                                .collect(),
                        )
                    })
                    .collect();
                let reply = if std::mem::take(&mut state.lose_response) {
                    tonic::Status::unavailable("fixture lost response").into_http()
                } else if let Some(user_id) = state.stale.take() {
                    grpc_reply(pb::SendGroupMessageResponse {
                        mismatched: vec![pb::AccountMismatchedDevices {
                            user_id,
                            devices: Some(pb::MismatchedDevices {
                                stale_devices: vec![1],
                                ..Default::default()
                            }),
                        }],
                        ..Default::default()
                    })
                } else {
                    let queued_at = crate::util::time::TimestampNanos::now().as_u64();
                    for (user, recipient) in &sealed.recipients {
                        let user = user.service_id_string();
                        let pending = state.inbox.entry(user.clone()).or_default();
                        pending.push(pb::DeviceMessage {
                            queue_id: pending.len() as i64 + 1,
                            message_guid: request.client_request_id.clone(),
                            envelope: sealed
                                .received_message_parts_for_recipient(recipient)
                                .as_ref()
                                .concat(),
                            envelope_type: chatter_proto::mailbox_v1::EnvelopeType::SealedSender
                                as i32,
                            destination_user_id: user,
                            server_timestamp: queued_at,
                            ..Default::default()
                        });
                    }
                    grpc_reply(pb::SendGroupMessageResponse {
                        success: true,
                        server_timestamp: 17,
                        ..Default::default()
                    })
                };
                let next = state.request_ids.len();
                let id = *state
                    .request_ids
                    .entry(request.client_request_id)
                    .or_insert(next);
                (Routing::Group(request.channel_id, id, roster), reply)
            }
            _ => (
                Routing::Other,
                tonic::Status::unimplemented("unexpected RPC").into_http(),
            ),
        };
        let phase = state.phase;
        let next_connection = state.connections.len();
        let connection = *state.connections.entry(remote).or_insert(next_connection);
        state.calls.push(Rpc {
            phase,
            connection,
            endpoint,
            authorization,
            blind_credential: head.headers.contains_key("x-anonymous-credential"),
            access_key: head.headers.contains_key("unidentified-access-key"),
            payload_size: payload.len(),
            routing,
        });
        Ok(reply)
    }
}

async fn start_server(
    state: ServerState,
) -> (
    String,
    Arc<Mutex<ServerState>>,
    tokio::task::JoinHandle<Result<(), tonic::transport::Error>>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let probe = Arc::new(Mutex::new(state));
    let state = probe.clone();
    let incoming = futures::stream::unfold(listener, |listener| async {
        Some((listener.accept().await.map(|(socket, _)| socket), listener))
    });
    let server = tokio::spawn(Server::builder().serve_with_incoming(
        tower::service_fn(move |request| ServerState::call(state.clone(), request)),
        incoming,
    ));
    (url, probe, server)
}

fn roster_bundles(members: [UserId; 2]) -> BTreeMap<String, Vec<pb::SignalPreKeyBundle>> {
    let mut rng = StdRng::seed_from_u64(911);
    members
        .into_iter()
        .map(|user| {
            let devices = (1..=2)
                .map(|device| {
                    let mut bundle = repair_bundle(user, 40 + device, &mut rng);
                    bundle.device_id = device;
                    bundle
                })
                .collect();
            (user.to_string(), devices)
        })
        .collect()
}

async fn prepare_author(
    client: &ChatterClient,
    owner: UserId,
    channel: ChannelId,
    bundles: &BTreeMap<String, Vec<pb::SignalPreKeyBundle>>,
) {
    let messages = client.messages();
    let identity = libsignal_protocol::IdentityKeyPair::generate(&mut StdRng::seed_from_u64(
        owner.as_uuid().as_u128() as u64,
    ));
    let keys = &messages.signal_service.key_store;
    let device = messages.current_device_id_or_zero();
    keys.set_local_identity(40 + device, &identity.serialize())
        .unwrap();
    messages
        .signal_service
        .actor
        .rotate_signed_prekey()
        .await
        .unwrap();
    for (peer, devices) in bundles {
        keys.mark_peer_verified(peer, &devices[0].identity_key)
            .unwrap();
    }
    messages
        .grpc_pool
        .auth()
        .set_basic(format!("{owner}.{device}"), "fixture".into());
    cache_fixture_sender_certificate(client, owner);
    seed_tokens(messages, 90);
    let conn = messages.database.connection().unwrap();
    conn.execute(
        "INSERT INTO mailbox_server_keys (key_id, public_key_der, valid_until, created_at)
         VALUES ('attachment-live', x'00', ?1, '')",
        [i64::MAX],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO mailbox_tokens (id, ephemeral_public_key, ephemeral_private_key, signature, server_key_id, created_at, attachment)
         SELECT 'attachment-' || id, CAST(x'01' || substr(ephemeral_public_key, 2) AS BLOB),
                ephemeral_private_key, signature, 'attachment-live', created_at, 1
         FROM mailbox_tokens WHERE attachment = 0",
        [],
    ).unwrap();
    drop(conn);
    let state = messages.group_gate().for_distribution(&channel).unwrap();
    messages
        .group_send_tokens
        .put(&channel, state.state_hash, u64::MAX, vec![1]);
}

async fn post(
    author: &MessageService,
    channel: ChannelId,
    index: u128,
) -> (
    pb::SendMessageRequest,
    chatter_proto::content_v1::SenderKeyDistribution,
) {
    let distribution = author
        .create_sender_key_for_channel(&channel)
        .await
        .unwrap();
    let (content, _) = author
        .prepare_encrypted_content(
            &channel,
            &chatter_proto::protovalidate::test_plaintext("same post"),
            pb::message::Content::default(),
            true,
            index as u64,
        )
        .await
        .unwrap();
    let request = pb::SendMessageRequest {
        channel_id: channel.to_string(),
        content: Some(content),
        client_request_id: uuid::Uuid::from_u128(index).to_string(),
        ..Default::default()
    };
    (request, distribution)
}

async fn deliver_before_distribution(
    receiver: &MessageService,
    author: UserId,
    request: &pb::SendMessageRequest,
    mut distribution: chatter_proto::content_v1::SenderKeyDistribution,
) {
    let gateway = MessageGateway::new(
        (*receiver.signal_service).clone(),
        receiver.message_repo.clone(),
        receiver.channel_repo.clone(),
        receiver.user_repo.clone(),
        receiver.settings_repo.clone(),
        receiver.solicited_resends.clone(),
    );
    let pb::message::content::Type::Encrypted(encrypted) =
        request.content.as_ref().unwrap().r#type.as_ref().unwrap()
    else {
        panic!("the fixture must post E2EE content");
    };
    let RouteResult::NeedsSenderKey {
        channel_id,
        sender_id,
    } = gateway
        .route_encrypted_message(encrypted, &author, 1)
        .await
        .unwrap()
    else {
        panic!("the fixture must exercise a missing sender key");
    };
    assert_eq!(sender_id, author);
    // Exercise the outbound boundary regardless of the inbox's request
    // throttle so another caller cannot reintroduce this RPC.
    receiver
        .send_sender_key_request(&channel_id, &sender_id)
        .await
        .unwrap();

    // The delayed SKDM can still open the retained post. Its profile key is
    // stored locally without asking the server for the poster's profile.
    let state = receiver.group_gate().for_distribution(&channel_id).unwrap();
    distribution.state_revision = state.state.revision;
    distribution.state_hash = state.state_hash.to_vec();
    distribution.profile_key = Some(vec![92; 32]);
    assert_eq!(
        gateway
            .accept_skdm(&author, 1, &channel_id, &distribution)
            .await
            .unwrap(),
        Some(false)
    );
    assert!(matches!(
        gateway
            .route_encrypted_message(encrypted, &author, 1)
            .await
            .unwrap(),
        RouteResult::Decrypted(_)
    ));
}

#[tokio::test]
async fn private_post_server_trace_is_independent_of_author() {
    let channel = ChannelId::new();
    let alice_id = UserId::new();
    let bob_id = UserId::new();
    let bundles = roster_bundles([alice_id, bob_id]);
    let (url, probe, server) = start_server(ServerState::new(bundles.clone())).await;

    let mut traces = Vec::new();
    for index in [0, 1] {
        let fixture = two_members_with_ids_at(&url, channel, alice_id, bob_id);
        let members = [
            (alice_id, fixture.alice.messages(), &fixture.alice),
            (bob_id, fixture.bob.messages(), &fixture.bob),
        ];
        for (owner, _, client) in &members {
            prepare_author(client, *owner, channel, &bundles).await;
        }
        let (author, sender, _) = members[index];
        let (_, receiver, _) = members[1 - index];
        {
            let mut probe = probe.lock().unwrap();
            probe.bundles = bundles.clone();
            probe.connections.clear();
            probe.request_ids.clear();
            probe.inbox.clear();
            probe.phase("cold");
        }
        sender
            .send_control(
                &channel,
                crate::services::build_typing_content_with_channel(
                    true,
                    Some(&channel.to_string()),
                ),
                None,
            )
            .await
            .unwrap();
        let (first, key) = post(sender, channel, 1).await;
        sender.submit_group_request(&first, &key).await.unwrap();

        probe.lock().unwrap().phase("missing SKDM");
        deliver_before_distribution(receiver, author, &first, key).await;

        probe.lock().unwrap().phase("warm");
        let (warm, key) = post(sender, channel, 2).await;
        sender.submit_group_request(&warm, &key).await.unwrap();

        {
            let mut probe = probe.lock().unwrap();
            probe.phase("rotation and mismatch");
            let target = alice_id.to_string();
            probe.bundles.get_mut(&target).unwrap()[0].registration_id = 51;
            // The same device is the echo in one run and a peer in the other.
            probe.stale = Some(target);
        }
        sender
            .signal_service
            .rotate_sender_key(&channel)
            .await
            .unwrap();
        let (rotated, key) = post(sender, channel, 3).await;
        sender.submit_group_request(&rotated, &key).await.unwrap();

        {
            let mut probe = probe.lock().unwrap();
            probe.phase("lost response and retry");
            probe.lose_response = true;
        }
        let (retry, key) = post(sender, channel, 4).await;
        assert!(sender.submit_group_request(&retry, &key).await.is_err());
        sender.submit_group_request(&retry, &key).await.unwrap();
        traces.push(std::mem::take(&mut probe.lock().unwrap().calls));
    }
    server.abort();

    assert_eq!(
        traces[0], traces[1],
        "the server could tell which member posted"
    );
    let trace = &traces[0];
    assert!(
        trace
            .iter()
            .all(|rpc| rpc.authorization.is_none() && !rpc.access_key)
    );
    assert_eq!(
        trace
            .iter()
            .filter(|rpc| rpc.phase == "missing SKDM")
            .count(),
        0
    );
    let cold_accounts: Vec<_> = trace
        .iter()
        .filter_map(|rpc| match &rpc.routing {
            Routing::Prekeys(account, selector) if rpc.phase == "cold" && selector == "*" => {
                Some(account.clone())
            }
            _ => None,
        })
        .collect();
    assert_eq!(cold_accounts, bundles.into_keys().collect::<Vec<_>>());
    assert_eq!(trace.iter().filter(|rpc| rpc.phase == "warm").count(), 1);
    assert_eq!(
        trace
            .iter()
            .filter(|rpc| rpc.phase == "lost response and retry")
            .count(),
        2
    );
}

fn private_actions(
    channel: ChannelId,
    target: UserId,
) -> Vec<(&'static str, chatter_proto::content_v1::Content)> {
    use crate::services::gateway::builders::*;
    let id = channel.to_string();
    let target = target.to_string();
    vec![
        (
            "edit",
            build_edit_content_with_channel(17, "updated text", Some(&id)),
        ),
        (
            "reaction",
            build_reaction_content_with_channel("👍", false, &target, 17, Some(&id)),
        ),
        (
            "pin",
            build_pin_content_with_channel(&target, 17, false, Some(&id)),
        ),
        (
            "delete",
            build_targeted_delete_content(17, Some(&target), Some(&id)),
        ),
    ]
}

async fn receive_group_messages(
    client: &Arc<ChatterClient>,
    author: UserId,
    author_device: u32,
    channel: ChannelId,
    distribution: Option<&chatter_proto::content_v1::SenderKeyDistribution>,
    controls: bool,
) {
    use crate::inbox::{
        InboxConfig, InboxEvent,
        ack_policy::{AckDecision, RetryBudget},
        poller::InboxPoller,
        polling::process_device_message,
        state::InboxState,
    };
    use crate::mailbox::group::{GroupCoordinator, GroupMailboxManager};
    use crate::storage::repositories::{AnonGroupRepository, ephemeral_repo::EphemeralRepository};
    let ms = client.messages();
    let db = ms.database.clone();
    let gateway = Arc::new(MessageGateway::new(
        (*ms.signal_service).clone(),
        ms.message_repo.clone(),
        ms.channel_repo.clone(),
        ms.user_repo.clone(),
        ms.settings_repo.clone(),
        ms.solicited_resends.clone(),
    ));
    if let Some(distribution) = distribution {
        let state = ms.group_gate().for_distribution(&channel).unwrap();
        let mut distribution = distribution.clone();
        distribution.state_revision = state.state.revision;
        distribution.state_hash = state.state_hash.to_vec();
        gateway
            .accept_skdm(&author, author_device, &channel, &distribution)
            .await
            .unwrap();
    }
    let (events, mut received) = tokio::sync::broadcast::channel::<InboxEvent>(32);
    let (_shutdown_tx, shutdown) = tokio::sync::watch::channel(false);
    let stats = Arc::new(parking_lot::RwLock::new(Default::default()));
    let poller = InboxPoller::new(
        InboxConfig::default(),
        Arc::new(tokio::sync::Mutex::new(ms.message_client())),
        ms.grpc_pool.clone(),
        shutdown,
        events.clone(),
        stats.clone(),
        Arc::new(parking_lot::RwLock::new(InboxState::Idle)),
    );
    let (messages, _) = poller.run_once().await.unwrap().unwrap();
    let expected = if controls {
        5 + usize::from(cfg!(feature = "media"))
    } else {
        2
    };
    assert_eq!(messages.len(), expected);
    let processor = client.inbox_event_processor();
    let mut accepted = Vec::new();
    let mut received_controls = Vec::new();
    for message in &messages {
        let decision = process_device_message(
            message,
            &crate::network::validation::MessageValidationPipeline::new(),
            &events,
            &stats,
            &gateway,
            &ms.message_repo,
            &ms.channel_repo,
            &AnonGroupRepository::new(db.clone()),
            &Arc::new(EphemeralRepository::new(db.clone())),
            &Arc::new(GroupCoordinator::new(Arc::new(GroupMailboxManager::new(
                db.clone(),
            )))),
            None,
            &RetryBudget::new(),
        )
        .await;
        if distribution.is_some() && message.queue_id == 2 {
            let id = crate::domain::AttachmentId::from_cdn_key("pa_abcdefghijklmnopqrstuv");
            let attachment = client.attachments().get(&id).unwrap().unwrap();
            assert!(
                attachment.private_channel == Some(channel),
                "store the private route before ACK"
            );
        }
        let mut delivered = false;
        while let Ok(event) = received.try_recv() {
            delivered |= matches!(event, InboxEvent::ChannelMessageReceived { .. });
            match &event {
                InboxEvent::ChannelEdit { .. } => received_controls.push("edit"),
                InboxEvent::ChannelReaction { .. } => received_controls.push("reaction"),
                InboxEvent::ChannelPin { .. } => received_controls.push("pin"),
                InboxEvent::ChannelDeleteForEveryone { .. } => received_controls.push("delete"),
                InboxEvent::GroupCallSignalReceived { .. } => received_controls.push("call ring"),
                InboxEvent::CallHangup { .. } => received_controls.push("call cancel"),
                InboxEvent::MessageFailed { error, .. } => panic!("inbound fixture failed: {error}"),
                InboxEvent::ChannelMessageReceived { text, .. } if controls => {
                    panic!("control appeared as a message: {text:?}")
                }
                _ => {}
            }
            processor.process(&event).await;
        }
        assert_eq!(decision, AckDecision::Ack);
        assert_eq!(
            delivered,
            distribution.is_some(),
            "a post reached the wrong member, or was missing (queue {}, controls={controls}, events={received_controls:?})",
            message.queue_id
        );
        accepted.push(message.message_guid.clone());
    }
    if controls {
        let mut expected = Vec::new();
        if ms.current_user_id().unwrap() != author {
            expected.extend(["edit", "reaction", "pin", "delete", "call ring"]);
            if cfg!(feature = "media") {
                expected.push("call cancel");
            }
        }
        assert_eq!(received_controls, expected);
    }
    poller.acknowledge(accepted).await.unwrap();
}

#[cfg(feature = "onion")]
#[tokio::test]
async fn private_channel_journal_and_actions_match_with_shield_on_and_off() {
    use crate::domain::AttachmentId;
    use crate::state::ActiveConversation;
    use std::sync::atomic::Ordering;
    let channel = ChannelId::new();
    let alice = UserId::new();
    let bob = UserId::new();
    let mut bundles = roster_bundles([alice, bob]);
    // Alice has one device. Bob posts and polls from device 2 of three.
    bundles.get_mut(&alice.to_string()).unwrap().truncate(1);
    let mut rng = StdRng::seed_from_u64(110);
    let mut additional = repair_bundle(bob, 43, &mut rng);
    additional.device_id = 3;
    bundles.get_mut(&bob.to_string()).unwrap().push(additional);
    let (url, probe, server) = start_server(ServerState::new(bundles.clone())).await;

    for shield in [false, true] {
        let mut traces = Vec::new();
        for poster in [0, 1] {
            let fixture = two_members_with_ids_at(&url, channel, alice, bob);
            let members = [(alice, &fixture.alice), (bob, &fixture.bob)];
            let (author, sending_client) = members[poster];
            let (_, receiving_client) = members[1 - poster];
            let sending = sending_client.messages();
            let conversation = ActiveConversation::Channel(channel);
            {
                let mut probe = probe.lock().unwrap();
                probe.inbox.clear();
                probe.connections.clear();
                probe.request_ids.clear();
                probe.phase("isolated routes");
            }
            for (i, (member, client)) in members.iter().enumerate() {
                let ms = client.messages();
                ms.settings_repo
                    .set(
                        crate::storage::repositories::settings_repo::keys::CURRENT_DEVICE_ID,
                        &(i as u32 + 1),
                    )
                    .unwrap();
                client
                    .auth_state()
                    .set_logged_in(*member, i as u32 + 1, "fixture".into());
                prepare_author(client, *member, channel, &bundles).await;
                if shield {
                    ms.grpc_pool.install_loopback_shield_routes(&url);
                    ms.shield_mode.store(true, Ordering::Release);
                }
                let _ = ms
                    .grpc_pool
                    .anonymous_mailbox_client()
                    .get_blind_signature_public_key()
                    .await;
            }

            probe.lock().unwrap().phase("upload URL");
            let attachment = sending_client
                .attachments()
                .for_conversation(&conversation)
                .file_client()
                .unwrap()
                .get_attachment_upload_url(512)
                .await
                .unwrap();
            let attachment_tokens = sending_client
                .attachments()
                .attachment_repo()
                .attachment_tokens();
            let before_picker = attachment_tokens.available_token_count().unwrap();
            probe.lock().unwrap().phase("GIF search");
            let results = sending_client
                .search_gifs_in(&conversation, "fixture", 20)
                .await
                .unwrap();
            probe.lock().unwrap().phase("GIF trending");
            sending_client
                .trending_gifs_in(&conversation, 20)
                .await
                .unwrap();
            probe.lock().unwrap().phase("GIF preview");
            for _ in 0..2 {
                assert_eq!(
                    sending_client
                        .fetch_gif_media_in(&conversation, &results[0].preview_url)
                        .await
                        .unwrap(),
                    b"GIF89a",
                );
            }
            assert_eq!(
                attachment_tokens.available_token_count().unwrap(),
                before_picker
            );
            let attachment_id = AttachmentId::from_cdn_key(&attachment.cdn_key);
            let pointer = chatter_proto::content_v1::AttachmentPointer {
                cdn_key: attachment.cdn_key.clone(),
                encryption_key: vec![3; 64],
                digest: vec![4; 32],
                plaintext_size: 432,
                content_type: "application/octet-stream".into(),
                file_name: "file.bin".into(),
                ..Default::default()
            };

            let distribution = sending
                .create_sender_key_for_channel(&channel)
                .await
                .unwrap();
            probe.lock().unwrap().phase("journal post");
            // Different body sizes within the same padding bucket are
            // indistinguishable at the exact RPC length.
            let text = ["short post", "slightly longer post"][poster];
            let thread = crate::domain::MessageId::from_uuid(uuid::Uuid::from_u128(200));
            let quote = crate::services::gateway::QuoteIntent {
                target_author_id: alice,
                target_timestamp: 17,
                target_preview: Some("quoted text".into()),
            };
            sending_client
                .send_channel_message_with_quote(
                    None,
                    &channel,
                    text,
                    vec![],
                    vec![],
                    Some(&thread),
                    vec![],
                    false,
                    Some(&quote),
                )
                .await
                .unwrap();
            probe.lock().unwrap().phase("attachment post");
            sending_client
                .send_channel_message(
                    None,
                    &channel,
                    "message with attachment",
                    vec![attachment_id],
                    vec![pointer],
                    None,
                    vec![],
                    false,
                )
                .await
                .unwrap();

            probe.lock().unwrap().phase("device inbox");
            for (i, (_, client)) in members.iter().enumerate() {
                receive_group_messages(
                    client,
                    author,
                    sending.current_device_id_or_zero(),
                    channel,
                    (i != poster).then_some(&distribution),
                    false,
                )
                .await;
            }
            assert!(
                receiving_client
                    .attachments()
                    .get(&attachment_id)
                    .unwrap()
                    .unwrap()
                    .private_channel
                    == Some(channel)
            );
            probe.lock().unwrap().phase("download URL");
            receiving_client
                .attachments()
                .for_conversation(&conversation)
                .file_client()
                .unwrap()
                .get_attachment_download_url(&attachment.cdn_key)
                .await
                .unwrap();

            for (phase, content) in private_actions(channel, author) {
                probe.lock().unwrap().phase(phase);
                sending.send_control(&channel, content, None).await.unwrap();
            }

            probe.lock().unwrap().phase("call ring");
            let call_id = uuid::Uuid::from_u128(73).to_string();
            sending_client
                .call_signaling()
                .ring_group_call(&channel, &call_id)
                .await
                .unwrap();
            #[cfg(feature = "media")]
            {
                probe.lock().unwrap().phase("call cancel");
                sending_client
                    .call_signaling()
                    .cancel_channel_start_rings(&channel, &author, &call_id)
                    .await;
            }
            probe.lock().unwrap().phase("receive controls");
            for (_, client) in &members {
                receive_group_messages(
                    client, author, sending.current_device_id_or_zero(), channel, None, true,
                ).await;
            }
            let trace = std::mem::take(&mut probe.lock().unwrap().calls);
            let private = trace
                .iter()
                .find(|rpc| {
                    rpc.phase == "journal post" && matches!(rpc.routing, Routing::Group(..))
                })
                .unwrap()
                .connection;
            let picker: Vec<_> = trace
                .iter()
                .filter(|rpc| rpc.phase.starts_with("GIF "))
                .collect();
            assert_eq!(
                picker
                    .iter()
                    .map(|rpc| rpc.endpoint.rsplit('/').next().unwrap())
                    .collect::<Vec<_>>(),
                [
                    "SearchPrivateGifs",
                    "GetPrivateTrendingGifs",
                    "GetPrivateGifMedia",
                    "GetPrivateGifMedia"
                ]
            );
            let account_routes: std::collections::HashSet<_> = trace
                .iter()
                .filter(|rpc| rpc.authorization.is_some())
                .map(|rpc| rpc.connection)
                .collect();
            assert!(!account_routes.is_empty());
            let mut picker_connections = std::collections::HashSet::new();
            for rpc in picker {
                assert!(rpc.authorization.is_none() && !rpc.blind_credential && !rpc.access_key);
                assert_ne!(rpc.connection, private);
                assert!(!account_routes.contains(&rpc.connection));
                assert!(
                    picker_connections.insert(rpc.connection),
                    "picker reused a connection"
                );
            }
            assert!(
                trace
                    .iter()
                    .filter(|rpc| matches!(
                        rpc.phase,
                        "upload URL"
                            | "call cancel"
                            | "journal post"
                            | "attachment post"
                            | "edit"
                            | "reaction"
                            | "pin"
                            | "delete"
                            | "call ring"
                    ))
                    .all(|rpc| rpc.connection == private
                        && rpc.authorization.is_none()
                        && !rpc.access_key),
                "{trace:?}"
            );
            assert!(
                trace
                    .iter()
                    .filter(|rpc| matches!(rpc.phase, "upload URL" | "download URL"))
                    .all(|rpc| rpc.blind_credential && rpc.authorization.is_none()),
                "{trace:?}"
            );
            assert!(
                trace
                    .iter()
                    .filter(|rpc| rpc.phase == "isolated routes" || rpc.authorization.is_some())
                    .all(|rpc| rpc.connection != private),
                "{trace:?}"
            );
            assert_eq!(
                trace
                    .iter()
                    .filter(|rpc| matches!(rpc.routing, Routing::Group(..)))
                    .count(),
                7 + usize::from(cfg!(feature = "media"))
            );

            assert_eq!(
                trace
                    .iter()
                    .filter(|rpc| matches!(rpc.phase, "device inbox" | "receive controls"))
                    .map(|rpc| rpc.endpoint.as_str())
                    .collect::<Vec<_>>(),
                [
                    "/chat.v1.MessageService/GetDeviceMessages",
                    "/chat.v1.MessageService/AcknowledgeMessages",
                    "/chat.v1.MessageService/GetDeviceMessages",
                    "/chat.v1.MessageService/AcknowledgeMessages",
                    "/chat.v1.MessageService/GetDeviceMessages",
                    "/chat.v1.MessageService/AcknowledgeMessages",
                    "/chat.v1.MessageService/GetDeviceMessages",
                    "/chat.v1.MessageService/AcknowledgeMessages",
                ]
            );
            traces.push(trace);
        }
        assert_eq!(
            traces[0], traces[1],
            "anonymous traffic differs with Shield={shield}"
        );
    }
    server.abort();
}

#[tokio::test]
async fn private_control_uses_the_anonymous_channel_route() {
    let channel = ChannelId::new();
    let alice = UserId::new();
    let bob = UserId::new();
    let outside_log = UserId::new();
    let bundles = roster_bundles([alice, bob]);
    let mut state = ServerState::new(bundles.clone());
    state.listing = Some(pb::Channel {
        id: channel.to_string(),
        members: [alice, bob, outside_log]
            .into_iter()
            .map(|user| pb::channel::ChannelMember {
                user_id: user.to_string(),
                role: pb::ChannelRole::Member as i32,
                joined_at: 17,
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    });
    let (url, probe, server) = start_server(state).await;
    let fixture = two_members_with_ids_at(&url, channel, alice, bob);
    prepare_author(&fixture.alice, alice, channel, &bundles).await;
    let result = fixture
        .alice
        .messages()
        .send_control(
            &channel,
            crate::services::gateway::builders::build_edit_content_with_channel(
                17,
                "updated text",
                Some(&channel.to_string()),
            ),
            None,
        )
        .await;

    server.abort();
    let probe = probe.lock().unwrap();
    assert!(
        probe.calls.iter().all(|rpc| rpc.authorization.is_none()
            && !rpc.endpoint.ends_with("/GetChannelMembers")),
        "channel control used an identified route: {:?}", probe.calls,
    );
    result.unwrap();
    assert!(
        probe
            .calls
            .iter()
            .any(|rpc| matches!(rpc.routing, Routing::Group(..)))
    );
    let outside_log = outside_log.to_string();
    assert!(probe.calls.iter().all(|rpc| match &rpc.routing {
        Routing::Prekeys(user, _) | Routing::Deposit(user, _, _) => user != &outside_log,
        Routing::Group(_, _, roster) => roster.iter().all(|(user, _)| user != &outside_log),
        Routing::Other => true,
    }));
}

#[tokio::test]
async fn private_attachment_mints_without_account_credentials() {
    let channel = ChannelId::new();
    let alice = UserId::new();
    let bob = UserId::new();
    let bundles = roster_bundles([alice, bob]);
    let (url, probe, server) = start_server(ServerState::new(bundles.clone())).await;
    let fixture = two_members_with_ids_at(&url, channel, alice, bob);
    prepare_author(&fixture.alice, alice, channel, &bundles).await;
    let attachments = fixture.alice.attachments().for_conversation(
        &crate::state::chat_state::ActiveConversation::Channel(channel),
    );
    attachments
        .file_client()
        .unwrap()
        .get_attachment_upload_url(512)
        .await
        .unwrap();

    server.abort();
    let probe = probe.lock().unwrap();
    let mint = probe
        .calls
        .iter()
        .find(|rpc| rpc.endpoint.ends_with("/GetPrivateAttachmentUploadUrl"))
        .expect("the private attachment requested an upload URL");
    assert!(
        mint.authorization.is_none() && mint.blind_credential,
        "{mint:?}"
    );
}

#[cfg(feature = "onion")]
#[tokio::test]
async fn private_attachment_credentials_and_connections_are_channel_scoped() {
    use crate::state::ActiveConversation;
    for shield in [false, true] {
        let channels = [ChannelId::new(), ChannelId::new()];
        let alice = UserId::new();
        let bob = UserId::new();
        let bundles = roster_bundles([alice, bob]);
        let (url, probe, server) = start_server(ServerState::new(bundles.clone())).await;
        let fixture = two_members_with_ids_at(&url, channels[0], alice, bob);
        prepare_author(&fixture.alice, alice, channels[0], &bundles).await;
        let messages = fixture.alice.messages();
        if shield {
            messages.grpc_pool.install_loopback_shield_routes(&url);
        }
        let token_manager =
            crate::mailbox::token_manager::TokenManager::new(messages.database.clone());
        let original_mailbox_balance = token_manager.available_token_count().unwrap();
        let file_client = |channel| {
            fixture
                .alice
                .attachments()
                .for_conversation(&ActiveConversation::Channel(channel))
                .file_client()
                .unwrap()
        };
        let mut retained = file_client(channels[0]);
        retained.get_attachment_upload_url(512).await.unwrap();
        file_client(channels[1])
            .get_attachment_upload_url(512)
            .await
            .unwrap();
        retained.get_attachment_upload_url(512).await.unwrap();
        tokio::time::pause();
        tokio::time::advance(std::time::Duration::from_secs(5 * 60)).await;
        tokio::time::resume();
        retained.get_attachment_upload_url(512).await.unwrap();
        let calls = &probe.lock().unwrap().calls;
        assert_eq!(calls.len(), 4);
        assert!(
            calls
                .iter()
                .all(|rpc| rpc.blind_credential && rpc.authorization.is_none())
        );
        assert_ne!(calls[0].connection, calls[1].connection, "Shield={shield}");
        assert_eq!(calls[0].connection, calls[2].connection, "Shield={shield}");
        assert_ne!(calls[0].connection, calls[3].connection, "Shield={shield}");
        assert_eq!(
            token_manager.available_token_count().unwrap(),
            original_mailbox_balance
        );
        server.abort();
    }
}

#[cfg(feature = "onion")]
#[tokio::test]
async fn private_attachment_put_and_get_use_a_separate_https_connection() {
    use bytes::Bytes;
    use http_body_util::{BodyExt, Full};
    use hyper::server::conn::http1;
    use hyper_util::rt::TokioIo;
    use rustls::pki_types::PrivateKeyDer;

    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert.cert.der().clone()).unwrap();
    let tls =
        rustls::ServerConfig::builder_with_provider(Arc::new(crate::network::pq_tls::provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![cert.cert.der().clone()],
                PrivateKeyDer::try_from(cert.key_pair.serialize_der()).unwrap(),
            )
            .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let cdn_url = format!(
        "https://localhost:{}/fixture",
        listener.local_addr().unwrap().port()
    );
    let blob = Arc::new(Mutex::new(Bytes::new()));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let https = tokio::spawn({
        let calls = calls.clone();
        async move {
            let tls = tokio_rustls::TlsAcceptor::from(Arc::new(tls));
            loop {
                let (socket, address) = listener.accept().await.unwrap();
                let tls = tls.clone();
                let blob = blob.clone();
                let calls = calls.clone();
                tokio::spawn(async move {
                    let Ok(socket) = tls.accept(socket).await else {
                        return;
                    };
                    let _ = http1::Builder::new()
                        .serve_connection(
                            TokioIo::new(socket),
                            hyper::service::service_fn(
                                move |req: http::Request<hyper::body::Incoming>| {
                                    let blob = blob.clone();
                                    let calls = calls.clone();
                                    async move {
                                        let (head, body) = req.into_parts();
                                        let body = body.collect().await.unwrap().to_bytes();
                                        let auth = head
                                            .headers
                                            .contains_key(http::header::AUTHORIZATION)
                                            || head.headers.contains_key("x-anonymous-credential");
                                        calls.lock().unwrap().push((
                                            head.method.clone(),
                                            address,
                                            body.len(),
                                            auth,
                                        ));
                                        let response = if head.method == http::Method::PUT {
                                            *blob.lock().unwrap() = body;
                                            Bytes::new()
                                        } else {
                                            blob.lock().unwrap().clone()
                                        };
                                        Ok::<_, Infallible>(http::Response::new(Full::new(
                                            response,
                                        )))
                                    }
                                },
                            ),
                        )
                        .await;
                });
            }
        }
    });
    let channel = ChannelId::new();
    let alice = UserId::new();
    let bob = UserId::new();
    let bundles = roster_bundles([alice, bob]);
    let mut state = ServerState::new(bundles.clone());
    state.cdn_url = cdn_url.clone();
    let (url, probe, server) = start_server(state).await;
    let fixture = two_members_with_ids_at(&url, channel, alice, bob);
    prepare_author(&fixture.alice, alice, channel, &bundles).await;
    prepare_author(&fixture.bob, bob, channel, &bundles).await;
    let conversation = crate::state::ActiveConversation::Channel(channel);
    crate::network::storage_http::STORAGE_FIXTURE_ROOTS
        .scope(roots, async {
            fixture
                .alice
                .attachments()
                .cdn_http_get(&cdn_url)
                .await
                .unwrap();
            let mut source = tempfile::NamedTempFile::new().unwrap();
            use std::io::Write as _;
            source.write_all(b"private attachment").unwrap();
            let upload = fixture
                .alice
                .attachments()
                .for_conversation(&conversation)
                .upload_to_cdn_encrypted(source.path())
                .await
                .unwrap();
            let ids = fixture
                .bob
                .attachments()
                .for_conversation(&conversation)
                .create_from_pointers(&[upload.to_attachment_pointer()])
                .unwrap();
            fixture
                .bob
                .attachments()
                .download_to_filesystem(&ids[0])
                .await
                .unwrap();
            probe.lock().unwrap().phase("selected GIF");
            fixture
                .alice
                .download_and_upload_gif_to(
                    "https://media.example/selected.gif",
                    "selected",
                    &conversation,
                )
                .await
                .unwrap();
        })
        .await;
    https.abort();
    server.abort();
    let calls = calls.lock().unwrap();
    assert_eq!(
        calls.iter().map(|(method, ..)| method).collect::<Vec<_>>(),
        [
            &http::Method::GET,
            &http::Method::PUT,
            &http::Method::GET,
            &http::Method::PUT
        ]
    );
    assert_ne!(calls[0].1, calls[1].1);
    assert_ne!(calls[0].1, calls[2].1);
    assert!(calls[1].2 > b"private attachment".len());
    assert!(calls.iter().all(|call| !call.3));
    let trace = &probe.lock().unwrap().calls;
    let selected: Vec<_> = trace
        .iter()
        .filter(|rpc| rpc.phase == "selected GIF")
        .collect();
    assert_eq!(
        selected
            .iter()
            .map(|rpc| rpc.endpoint.rsplit('/').next().unwrap())
            .collect::<Vec<_>>(),
        [
            "GetPrivateGifMedia",
            "GetPrivateAttachmentUploadUrl",
            "ConfirmAttachmentUpload"
        ]
    );
    assert!(
        selected
            .iter()
            .all(|rpc| rpc.authorization.is_none() && !rpc.access_key)
    );
    assert!(!selected[0].blind_credential);
    assert!(selected[1].blind_credential);
    assert_ne!(selected[0].connection, selected[1].connection);
    assert_eq!(selected[1].connection, selected[2].connection);
    assert_eq!(calls[1].1, calls[3].1);
    for name in [
        "GetPrivateAttachmentUploadUrl",
        "GetPrivateAttachmentDownloadUrl",
    ] {
        let mint = trace
            .iter()
            .find(|rpc| rpc.endpoint.ends_with(name))
            .unwrap();
        assert!(mint.blind_credential && mint.authorization.is_none());
    }
}

#[tokio::test]
async fn forged_public_listing_cannot_send_a_private_post_in_plaintext() {
    let channel = ChannelId::new();
    let alice_id = UserId::new();
    let bob_id = UserId::new();
    let bundles = roster_bundles([alice_id, bob_id]);
    let mut server = ServerState::new(bundles.clone());
    server.listing = Some(pb::Channel {
        id: channel.to_string(),
        owner_id: alice_id.to_string(),
        name: "Server listing".into(),
        r#type: pb::ChannelType::ChannelPublic as i32,
        config: Some(pb::channel::Config {
            max_members: 100,
            encrypted: false,
            ..Default::default()
        }),
        members: [alice_id, bob_id]
            .into_iter()
            .map(|member| pb::channel::ChannelMember {
                user_id: member.to_string(),
                role: pb::ChannelRole::Member as i32,
                joined_at: 17,
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    });
    let (url, probe, server) = start_server(server).await;
    let fixture = two_members_with_ids_at(&url, channel, alice_id, bob_id);
    let alice = &fixture.alice;
    prepare_author(alice, alice_id, channel, &bundles).await;
    let messages = alice.messages();
    let events = Arc::new(ChannelEventService::new(
        messages.message_repo.clone(),
        messages.channel_repo.clone(),
        messages.user_repo.clone(),
        messages.grpc_pool.clone(),
        alice.event_bus().clone(),
        messages.settings_repo.clone(),
        alice.channels().membership_log().clone(),
    ));
    let sync = ChannelSync::new(
        messages.grpc_pool.clone(),
        messages.channel_repo.clone(),
        messages.settings_repo.clone(),
        events,
    );
    probe.lock().unwrap().phase("forged listing");
    let (listed, stats) = sync.sync().await.unwrap();
    assert!(!stats.incomplete, "{stats:?}");
    assert_eq!(stats.updated_channels, 1);

    probe.lock().unwrap().phase("post");
    let sent = alice
        .send_channel_message(
            None,
            &channel,
            "private text",
            vec![],
            vec![],
            None,
            vec![],
            false,
        )
        .await;
    server.abort();

    let trace = &probe.lock().unwrap().calls;
    assert!(
        trace
            .iter()
            .any(|rpc| rpc.endpoint.ends_with("/ListChannels"))
    );
    assert!(
        trace
            .iter()
            .all(|rpc| !rpc.endpoint.ends_with("/SendMessage")),
        "{trace:?}"
    );
    assert!(
        trace
            .iter()
            .any(|rpc| matches!(&rpc.routing, Routing::Group(..)))
    );
    assert!(
        trace
            .iter()
            .filter(|rpc| rpc.phase == "post")
            .all(|rpc| rpc.authorization.is_none())
    );
    sent.expect("the post should succeed on the encrypted route");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].name, "Server listing");
    assert_eq!(listed[0].channel_type, ChannelType::Private);
    assert!(listed[0].config.require_encryption);
}

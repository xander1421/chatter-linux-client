//! Compare complete RPC traces while changing only the posting member.
use super::{private_wire_fixture::{grpc_reply, read_wire, repair_bundle}, two_members_with_ids_at};
use crate::client::ChatterClient;
use crate::domain::{ChannelId, ChannelType, UserId};
use crate::services::{
    channel_event_service::ChannelEventService,
    gateway::{MessageGateway, RouteResult},
    message::{MessageService, dm_bootstrap::tests::{cache_fixture_sender_certificate, seed_tokens}},
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
    time::Instant,
};
use tonic::{body::Body, transport::{Server, server::TcpConnectInfo}};

#[derive(Debug, PartialEq, Eq)]
struct Rpc {
    phase: &'static str,
    time_bucket: u64,
    connection: usize,
    endpoint: String,
    authorization: Option<String>,
    blind_credential: bool,
    access_key: bool,
    payload_bucket: usize,
    routing: Routing,
}

#[derive(Debug, PartialEq, Eq)]
enum Routing {
    Prekeys(String, String),
    Deposit(String, i32, Vec<(u32, u32, usize)>),
    Group(String, String, Vec<(String, Vec<(u32, u32)>)>),
    Other,
}

fn bucket(bytes: usize) -> usize {
    bytes.div_ceil(128)
}

struct ServerState {
    phase: &'static str,
    phase_started: Instant,
    connections: BTreeMap<SocketAddr, usize>,
    calls: Vec<Rpc>,
    bundles: BTreeMap<String, Vec<pb::SignalPreKeyBundle>>,
    listing: Option<pb::Channel>,
    stale: Option<String>,
    lose_response: bool,
}

impl ServerState {
    fn new(bundles: BTreeMap<String, Vec<pb::SignalPreKeyBundle>>) -> Self {
        Self {
            phase: "cold",
            phase_started: Instant::now(),
            connections: BTreeMap::new(),
            calls: Vec::new(),
            bundles,
            listing: None,
            stale: None,
            lose_response: false,
        }
    }

    fn phase(&mut self, phase: &'static str) {
        self.phase = phase;
        self.phase_started = Instant::now();
    }

    async fn call(
        state: Arc<Mutex<Self>>, request: http::Request<Body>,
    ) -> Result<http::Response<Body>, Infallible> {
        let arrival = Instant::now();
        let (head, body) = request.into_parts();
        let remote = head.extensions.get::<TcpConnectInfo>().unwrap().remote_addr().unwrap();
        let payload = read_wire(body).await;
        let endpoint = head.uri.path().to_owned();
        let mut state = state.lock().unwrap();
        let (routing, reply) = match endpoint.rsplit('/').next().unwrap() {
            "ListChannels" => (Routing::Other, grpc_reply(pb::ListChannelsResponse {
                channels: state.listing.clone().into_iter().collect(),
                ..Default::default()
            })),
            "GetChannelMembers" => {
                let members = state.listing.as_ref().unwrap().members.clone();
                (Routing::Other, grpc_reply(pb::GetChannelMembersResponse {
                    total: members.len() as i32,
                    members,
                }))
            }
            "GetChannelChanges" => (Routing::Other, grpc_reply(pb::GetChannelChangesResponse::default())),
            "GetPreKeyBundle" => {
                use pb::get_pre_key_bundle_response::Result as Bundles;
                let request = pb::GetPreKeyBundleRequest::decode(payload.as_slice()).unwrap();
                let bundles = &state.bundles[&request.user_id];
                let result = if request.device_id == "*" {
                    Some(Bundles::Devices(pb::PreKeyBundleDeviceList { bundles: bundles.clone(), identity_key: bundles[0].identity_key.clone() }))
                } else {
                    bundles.iter().find(|b| request.device_id == b.device_id.to_string())
                        .cloned().map(Bundles::Bundle)
                };
                let reply = grpc_reply(pb::GetPreKeyBundleResponse { result });
                (Routing::Prekeys(request.user_id, request.device_id), reply)
            }
            "SendSealedMessage" => {
                let request = pb::SendSealedMessageRequest::decode(payload.as_slice()).unwrap();
                let devices = request.messages.iter().map(|message| (
                    message.device_id, message.registration_id,
                    bucket(message.sealed_message.as_ref().unwrap().sealed_body.len()),
                )).collect();
                let reply = grpc_reply(pb::SendSealedMessageResponse {
                    success: true,
                    delivered_to_devices: request.messages.iter().map(|m| m.device_id).collect(),
                    ..Default::default()
                });
                (Routing::Deposit(request.destination_user_id, request.delivery, devices), reply)
            }
            "SendGroupMessage" => {
                let request = pb::SendGroupMessageRequest::decode(payload.as_slice()).unwrap();
                let sealed = SealedSenderV2SentMessage::parse(&request.multi_recipient_message).unwrap();
                let roster = sealed.recipients.iter().map(|(user, recipient)| (
                    user.service_id_string(),
                    recipient.devices.iter().map(|(device, registration)| (
                        u32::from(*device), u32::from(*registration),
                    )).collect(),
                )).collect();
                let reply = if std::mem::take(&mut state.lose_response) {
                    tonic::Status::unavailable("fixture lost response").into_http()
                } else if let Some(user_id) = state.stale.take() {
                    grpc_reply(pb::SendGroupMessageResponse {
                        mismatched: vec![pb::AccountMismatchedDevices {
                            user_id,
                            devices: Some(pb::MismatchedDevices { stale_devices: vec![1], ..Default::default() }),
                        }],
                        ..Default::default()
                    })
                } else {
                    grpc_reply(pb::SendGroupMessageResponse {
                        success: true, server_timestamp: 17, ..Default::default()
                    })
                };
                (Routing::Group(request.channel_id, request.client_request_id, roster), reply)
            }
            _ => (Routing::Other, tonic::Status::unimplemented("unexpected RPC").into_http()),
        };
        let phase = state.phase;
        let time_bucket = arrival.saturating_duration_since(state.phase_started).as_secs() / 5;
        let next_connection = state.connections.len();
        let connection = *state.connections.entry(remote).or_insert(next_connection);
        state.calls.push(Rpc {
            phase,
            time_bucket,
            connection,
            endpoint,
            authorization: head.headers.get(http::header::AUTHORIZATION)
                .map(|value| value.to_str().unwrap().to_owned()),
            blind_credential: head.headers.contains_key("x-anonymous-credential"),
            access_key: head.headers.contains_key("unidentified-access-key"),
            payload_bucket: bucket(payload.len()),
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
    let identity = libsignal_protocol::IdentityKeyPair::generate(
        &mut StdRng::seed_from_u64(owner.as_uuid().as_u128() as u64),
    );
    let keys = &messages.signal_service.key_store;
    keys.set_local_identity(41, &identity.serialize()).unwrap();
    messages.signal_service.actor.rotate_signed_prekey().await.unwrap();
    for (peer, devices) in bundles {
        keys.mark_peer_verified(peer, &devices[0].identity_key).unwrap();
    }
    messages.grpc_pool.auth().set_basic(format!("{owner}.1"), "fixture".into());
    cache_fixture_sender_certificate(client, owner);
    seed_tokens(messages, 90);
    let state = messages.group_gate().for_distribution(&channel).unwrap();
    messages.group_send_tokens.put(&channel, state.state_hash, u64::MAX, vec![1]);
}

async fn post(
    author: &MessageService, channel: ChannelId, index: u128,
) -> (pb::SendMessageRequest, chatter_proto::content_v1::SenderKeyDistribution) {
    let distribution = author.create_sender_key_for_channel(&channel).await.unwrap();
    let (content, _) = author.prepare_encrypted_content(
        &channel, &chatter_proto::protovalidate::test_plaintext("same post"),
        pb::message::Content::default(), true, index as u64,
    ).await.unwrap();
    let request = pb::SendMessageRequest {
        channel_id: channel.to_string(),
        content: Some(content),
        client_request_id: uuid::Uuid::from_u128(index).to_string(),
        ..Default::default()
    };
    (request, distribution)
}

async fn deliver_before_distribution(
    receiver: &MessageService, author: UserId, request: &pb::SendMessageRequest,
    mut distribution: chatter_proto::content_v1::SenderKeyDistribution,
) {
    let gateway = MessageGateway::new(
        (*receiver.signal_service).clone(), receiver.message_repo.clone(),
        receiver.channel_repo.clone(), receiver.user_repo.clone(),
        receiver.settings_repo.clone(), receiver.solicited_resends.clone(),
    );
    let pb::message::content::Type::Encrypted(encrypted) =
        request.content.as_ref().unwrap().r#type.as_ref().unwrap()
    else {
        panic!("the fixture must post E2EE content");
    };
    let RouteResult::NeedsSenderKey { channel_id, sender_id } =
        gateway.route_encrypted_message(encrypted, &author, 1).await.unwrap()
    else {
        panic!("the fixture must exercise a missing sender key");
    };
    assert_eq!(sender_id, author);
    // Exercise the outbound boundary regardless of the inbox's request
    // throttle so another caller cannot reintroduce this RPC.
    receiver.send_sender_key_request(&channel_id, &sender_id).await.unwrap();

    // The delayed SKDM can still open the retained post. Its profile key is
    // stored locally without asking the server for the poster's profile.
    let state = receiver.group_gate().for_distribution(&channel_id).unwrap();
    distribution.state_revision = state.state.revision;
    distribution.state_hash = state.state_hash.to_vec();
    distribution.profile_key = Some(vec![92; 32]);
    assert_eq!(gateway.accept_skdm(&author, 1, &channel_id, &distribution).await.unwrap(), Some(false));
    assert!(matches!(
        gateway.route_encrypted_message(encrypted, &author, 1).await.unwrap(),
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
            probe.phase("cold");
        }
        sender.send_control(&channel, crate::services::build_typing_content_with_channel(true, Some(&channel.to_string())), None).await.unwrap();
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
        sender.signal_service.rotate_sender_key(&channel).await.unwrap();
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

    assert_eq!(traces[0], traces[1], "the server could tell which member posted");
    let trace = &traces[0];
    assert!(trace.iter().all(|rpc| rpc.authorization.is_none() && !rpc.access_key));
    assert_eq!(trace.iter().filter(|rpc| rpc.phase == "missing SKDM").count(), 0);
    let cold_accounts: Vec<_> = trace.iter().filter_map(|rpc| match &rpc.routing {
        Routing::Prekeys(account, selector) if rpc.phase == "cold" && selector == "*" =>
            Some(account.clone()),
        _ => None,
    }).collect();
    assert_eq!(cold_accounts, bundles.into_keys().collect::<Vec<_>>());
    assert_eq!(trace.iter().filter(|rpc| rpc.phase == "warm").count(), 1);
    assert_eq!(trace.iter().filter(|rpc| rpc.phase == "lost response and retry").count(), 2);
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
        .send_channel_message(None, &channel, "private text", vec![], vec![], None, vec![], false)
        .await;
    server.abort();

    let trace = &probe.lock().unwrap().calls;
    assert!(trace.iter().any(|rpc| rpc.endpoint.ends_with("/ListChannels")));
    assert!(trace.iter().all(|rpc| !rpc.endpoint.ends_with("/SendMessage")), "{trace:?}");
    assert!(trace.iter().any(|rpc| matches!(&rpc.routing, Routing::Group(..))));
    assert!(trace.iter().filter(|rpc| rpc.phase == "post").all(|rpc| rpc.authorization.is_none()));
    sent.expect("the post should succeed on the encrypted route");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].name, "Server listing");
    assert_eq!(listed[0].channel_type, ChannelType::Private);
    assert!(listed[0].config.require_encryption);
}

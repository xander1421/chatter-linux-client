//! Sealed or nothing: what the server sees from `send_sealed_batch` when our
//! UAK is missing or rejected. A loopback gRPC server records every
//! `SendSealedMessage` and how it was authenticated, and refuses a UAK unless
//! it is the one derived from `accepted_key`. The loopback server speaks
//! plain HTTP, which only a `dev-trust-root` build may dial.
use super::*;
#[cfg(feature = "dev-trust-root")]
mod loopback {
use super::*;
use crate::client::ChatterClient;
use crate::error::NetworkError;
use crate::storage::repositories::settings_repo::keys;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use tonic::body::Body;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Auth {
    Basic,
    AccessKey(String),
}

struct Probe {
    accepted_uak: Option<String>,
    sends: Vec<Auth>,
}

fn uak_header(key: &[u8; 32]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(crate::crypto::derive_uak(key))
}

/// Trailers-only gRPC response: `status` and no message.
fn grpc_status(status: tonic::Code) -> http::Response<Body> {
    http::Response::builder()
        .header("content-type", "application/grpc")
        .header("grpc-status", (status as i32).to_string())
        .body(Body::empty())
        .unwrap()
}

struct GrpcFrames(std::collections::VecDeque<http_body::Frame<bytes::Bytes>>);

impl http_body::Body for GrpcFrames {
    type Data = bytes::Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        std::task::Poll::Ready(self.0.pop_front().map(Ok))
    }
}

fn grpc_reply(response: impl prost::Message) -> http::Response<Body> {
    let mut body = vec![0];
    body.extend_from_slice(&(response.encoded_len() as u32).to_be_bytes());
    response.encode(&mut body).unwrap();
    let mut trailers = http::HeaderMap::new();
    trailers.insert("grpc-status", "0".parse().unwrap());
    http::Response::builder()
        .header("content-type", "application/grpc")
        .body(Body::new(GrpcFrames(std::collections::VecDeque::from([
            http_body::Frame::data(body.into()),
            http_body::Frame::trailers(trailers),
        ]))))
        .unwrap()
}

async fn start_server(probe: Arc<Mutex<Probe>>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let incoming = futures::stream::unfold(listener, |listener| async {
        Some((listener.accept().await.map(|(socket, _)| socket), listener))
    });
    let service = tower::service_fn(move |request: http::Request<Body>| {
        let probe = probe.clone();
        async move {
            if !request.uri().path().ends_with("/SendSealedMessage") {
                return Ok::<_, Infallible>(grpc_status(tonic::Code::Unimplemented));
            }
            let headers = request.headers();
            let auth = match headers.get("unidentified-access-key") {
                Some(key) => Auth::AccessKey(key.to_str().unwrap().to_owned()),
                None if headers.contains_key(http::header::AUTHORIZATION) => Auth::Basic,
                None => return Ok(grpc_status(tonic::Code::Unauthenticated)),
            };
            let mut probe = probe.lock().unwrap();
            probe.sends.push(auth.clone());
            Ok(match auth {
                Auth::AccessKey(key) if Some(&key) != probe.accepted_uak.as_ref() => {
                    grpc_status(tonic::Code::Unauthenticated)
                }
                _ => grpc_reply(SendSealedMessageResponse {
                    delivered_to_devices: vec![1],
                    server_timestamp: 1,
                    ..Default::default()
                }),
            })
        }
    });
    tokio::spawn(tonic::transport::Server::builder().serve_with_incoming(service, incoming));
    url
}

struct Fixture {
    client: Arc<ChatterClient>,
    probe: Arc<Mutex<Probe>>,
    me: UserId,
    peer: UserId,
    _dir: tempfile::TempDir,
}

async fn fixture(accepted_key: Option<[u8; 32]>) -> Fixture {
    let probe = Arc::new(Mutex::new(Probe {
        accepted_uak: accepted_key.as_ref().map(uak_header),
        sends: Vec::new(),
    }));
    let url = start_server(probe.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    let db = crate::system::Directories::with_base_dir(dir.path().to_path_buf())
        .unwrap()
        .database_path();
    std::fs::write(
        db.parent().unwrap().join(crate::system::keyring::NO_KEYRING_MARKER),
        b"",
    )
    .unwrap();
    let client = ChatterClient::builder()
        .server_url(&url)
        .data_dir(dir.path().to_str().unwrap())
        .build()
        .expect("build client");
    let (me, peer) = (UserId::new(), UserId::new());
    let messages = client.messages();
    messages.settings_repo.set(keys::CURRENT_USER_ID, &me.to_string()).unwrap();
    messages.settings_repo.set(keys::CURRENT_DEVICE_ID, &1u32).unwrap();
    client.auth_state().set_logged_in(me, 1, "fixture".into());
    messages.grpc_pool.auth().set_basic(format!("{me}.1"), "fixture".into());
    messages.user_repo.ensure_user_exists(&peer).unwrap();
    Fixture { client, probe, me, peer, _dir: dir }
}

impl Fixture {
    fn store_peer_key(&self, key: [u8; 32]) {
        self.client
            .messages()
            .user_repo
            .set_friend_profile_key(&self.me, &self.peer, &key)
            .unwrap();
    }

    async fn send(
        &self,
        uak: Option<[u8; 32]>,
        sealed_only: bool,
    ) -> Result<SendSealedMessageResponse, NetworkError> {
        let messages = self.client.messages();
        messages
            .send_sealed_batch(
                &mut messages.message_client(),
                &self.peer.to_string(),
                vec![SealedDeviceMessage {
                    device_id: 1,
                    registration_id: 1,
                    sealed_message: Some(SealedSenderMessage {
                        sealed_body: vec![7; 32],
                        server_key_id: 1,
                        timestamp: 1,
                    }),
                }],
                SealedDelivery::NamedDevices,
                uak.map(|key| crate::crypto::derive_uak(&key)).as_ref(),
                sealed_only,
                &uuid::Uuid::new_v4().to_string(),
            )
            .await
    }

    fn sends(&self) -> Vec<Auth> {
        self.probe.lock().unwrap().sends.clone()
    }
}

/// A stale UAK (the peer rotated their profile key) is refused and the send
/// fails: the server never receives a Basic-auth copy, receipt or message.
#[tokio::test]
async fn rejected_uak_never_falls_back_to_basic_auth() {
    let fresh = [2; 32];
    for sealed_only in [true, false] {
        let f = fixture(Some(fresh)).await;
        f.store_peer_key([1; 32]);
        let error = f.send(Some([1; 32]), sealed_only).await.unwrap_err();
        assert!(matches!(error, NetworkError::SealedAccessUnavailable), "{error:?}");
        assert_eq!(f.sends(), vec![Auth::AccessKey(uak_header(&[1; 32]))]);
    }
}

/// The refresh: a newer profile key stored since the UAK was derived (a
/// queued envelope keeps the UAK it was sealed with) is retried sealed.
#[tokio::test]
async fn rejected_uak_retries_sealed_with_the_newer_stored_key() {
    let fresh = [2; 32];
    let f = fixture(Some(fresh)).await;
    f.store_peer_key(fresh);
    let response = f.send(Some([1; 32]), true).await.expect("sealed retry");
    assert_eq!(response.delivered_to_devices, vec![1]);
    assert_eq!(
        f.sends(),
        vec![
            Auth::AccessKey(uak_header(&[1; 32])),
            Auth::AccessKey(uak_header(&fresh)),
        ]
    );
}

/// Without any UAK, receipts/typing and everything in Shield are dropped
/// before the network; only an ordinary message to a peer whose key we never
/// had still degrades to identified delivery.
#[tokio::test]
async fn no_uak_goes_identified_only_for_ordinary_content_outside_shield() {
    let f = fixture(None).await;
    let error = f.send(None, true).await.unwrap_err();
    assert!(matches!(error, NetworkError::SealedAccessUnavailable), "{error:?}");
    f.client
        .messages()
        .shield_mode
        .store(true, std::sync::atomic::Ordering::Release);
    let error = f.send(None, false).await.unwrap_err();
    assert!(matches!(error, NetworkError::SealedAccessUnavailable), "{error:?}");
    assert!(f.sends().is_empty(), "nothing reached the server: {:?}", f.sends());

    f.client
        .messages()
        .shield_mode
        .store(false, std::sync::atomic::Ordering::Release);
    f.send(None, false).await.expect("identified degradation");
    assert_eq!(f.sends(), vec![Auth::Basic]);
}
}

#[test]
fn receipts_and_typing_are_sealed_only() {
    use chatter_proto::content_v1::{Content, ReceiptMessage, TypingMessage, content::Content as Body};
    let only = |body| is_sealed_only_content(&Content { content: Some(body), ..Default::default() });
    assert!(only(Body::Receipt(ReceiptMessage::default())));
    assert!(only(Body::Typing(TypingMessage::default())));
    assert!(!only(Body::DataMessage(Default::default())));
}

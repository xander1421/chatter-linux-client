//! All certificates/keys are disposable test fixtures generated in memory.
use super::*;
use futures_util::SinkExt;
use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use tokio_rustls::{rustls, TlsAcceptor};

fn fixtures(host: &str) -> (String, TlsAcceptor) {
    let mut params = CertificateParams::default();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let ca_key = KeyPair::generate().unwrap();
    let ca = params.self_signed(&ca_key).unwrap();
    let key = KeyPair::generate().unwrap();
    let leaf = CertificateParams::new(vec![host.into()])
        .unwrap()
        .signed_by(&key, &ca, &ca_key)
        .unwrap();
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(
        vec![leaf.der().clone()],
        rustls::pki_types::PrivatePkcs8KeyDer::from(key.serialize_der()).into(),
    )
    .unwrap();
    (ca.pem(), TlsAcceptor::from(Arc::new(config)))
}

async fn server(host: &str, validation: bool) -> (String, String, tokio::task::JoinHandle<()>) {
    let (ca, acceptor) = fixtures(host);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("wss://localhost:{}", listener.local_addr().unwrap().port());
    let task = tokio::spawn(async move {
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let Ok(mut stream) = acceptor.accept(socket).await else {
                    return;
                };
                if validation {
                    let mut buf = [0; 8192];
                    let count = stream.read(&mut buf).await.unwrap();
                    let request = String::from_utf8_lossy(&buf[..count]).to_lowercase();
                    assert!(request.contains("authorization: bearer fixture-token"));
                    stream.write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 7\r\nConnection: close\r\n\r\nfixture").await.unwrap();
                    return;
                }
                let reconnect = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let captured = reconnect.clone();
                let mut ws = tokio_tungstenite::accept_hdr_async(
                    stream,
                    move |request: &tokio_tungstenite::tungstenite::handshake::server::Request,
                          response| {
                        assert_eq!(request.headers()["authorization"], "Bearer fixture-token");
                        captured.store(
                            request
                                .uri()
                                .query()
                                .unwrap_or_default()
                                .contains("reconnect=1"),
                            Ordering::Relaxed,
                        );
                        Ok(response)
                    },
                )
                .await
                .unwrap();
                let message = if reconnect.load(Ordering::Relaxed) {
                    proto::signal_response::Message::Reconnect(proto::ReconnectResponse::default())
                } else {
                    proto::signal_response::Message::Join(proto::JoinResponse {
                        participant: Some(proto::ParticipantInfo {
                            sid: "PA_fixture".into(),
                            ..Default::default()
                        }),
                        ..Default::default()
                    })
                };
                let response = proto::SignalResponse {
                    message: Some(message),
                };
                ws.send(tokio_tungstenite::tungstenite::Message::Binary(
                    prost::Message::encode_to_vec(&response).into(),
                ))
                .await
                .unwrap();
                // Resume is exercised after the old signaling socket closes.
                ws.close(None).await.unwrap();
            });
        }
    });
    (address, ca, task)
}

fn options(ca: String) -> SignalOptions {
    SignalOptions {
        tls_root_ca_pem: Some(ca),
        single_peer_connection: false,
        connect_timeout: Duration::from_secs(2),
        ..Default::default()
    }
}

#[tokio::test]
async fn private_ca_authenticates_initial_signal_and_reconnect() {
    let (url, ca, task) = server("localhost", false).await;
    let (inner, _, _) = SignalInner::connect(&url, "fixture-token", options(ca), None)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), inner.restart())
        .await
        .unwrap()
        .unwrap();
    inner.close(false).await;
    task.abort();
}

#[tokio::test]
async fn private_ca_rejects_wrong_ca_and_hostname() {
    for wrong_host in [false, true] {
        let (url, ca, task) = server(
            if wrong_host {
                "wrong.example.invalid"
            } else {
                "localhost"
            },
            false,
        )
        .await;
        let ca = if wrong_host {
            ca
        } else {
            fixtures("localhost").0
        };
        assert!(
            SignalInner::connect(&url, "fixture-token", options(ca), None)
                .await
                .is_err()
        );
        task.abort();
    }
}

#[tokio::test]
async fn validation_uses_private_ca_and_bearer_header() {
    let (url, ca, task) = server("localhost", true).await;
    let result = SignalInner::validate(
        url::Url::parse(&format!("{url}/rtc")).unwrap(),
        "fixture-token",
        Some(&ca),
    )
    .await;
    assert!(
        matches!(result, Err(SignalError::Client(status, _)) if status == StatusCode::UNAUTHORIZED)
    );
    task.abort();
}

#[tokio::test]
async fn validation_http_client_rejects_wrong_ca_and_hostname() {
    for wrong_host in [false, true] {
        let (url, ca, task) = server(
            if wrong_host {
                "wrong.example.invalid"
            } else {
                "localhost"
            },
            true,
        )
        .await;
        let ca = if wrong_host {
            ca
        } else {
            fixtures("localhost").0
        };
        let url = url.replacen("wss://", "https://", 1);
        assert!(pinned_tls::http_client(&ca)
            .unwrap()
            .get(url)
            .bearer_auth("fixture-token")
            .send()
            .await
            .is_err());
        task.abort();
    }
}

#[test]
fn malformed_or_empty_ca_never_loads_default_roots() {
    assert!(pinned_tls::connector(Some("")).is_err());
    assert!(pinned_tls::connector(Some("invalid")).is_err());
    assert!(pinned_tls::http_client("invalid").is_err());
    assert!(pinned_tls::connector(None).unwrap().is_none());
}

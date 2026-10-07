use std::{
    future::poll_fn,
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    task::{Context, Wake},
    time::Duration,
};

use hyper::body::Body as _;

use h3::quic::{RecvStream as _, StreamErrorIncoming};
use h3_quinn::{Connection, RecvStream};
use quinn::{ClientConfig, Endpoint, ServerConfig, VarInt};
use rustls::pki_types::PrivatePkcs8KeyDer;

// Real loopback transport, with an ephemeral certificate explicitly trusted by
// this client. No permissive verifier and no production credentials are used.
async fn connected() -> (Endpoint, Endpoint, quinn::Connection, Connection) {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let key = PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der());
    let mut tls = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert.cert.der().clone()], key.into())
        .unwrap();
    tls.alpn_protocols = vec![b"h3".to_vec()];
    let config = ServerConfig::with_crypto(Arc::new(
        quinn::crypto::rustls::QuicServerConfig::try_from(tls).unwrap(),
    ));
    let server = Endpoint::server(config, "127.0.0.1:0".parse().unwrap()).unwrap();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert.cert.der().clone()).unwrap();
    let mut client = Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
    let mut tls = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    tls.alpn_protocols = vec![b"h3".to_vec()];
    client.set_default_client_config(ClientConfig::new(Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(tls).unwrap(),
    )));
    let connecting = client
        .connect(server.local_addr().unwrap(), "localhost")
        .unwrap();
    let (client_conn, server_conn) = tokio::join!(connecting, async {
        server.accept().await.unwrap().await.unwrap()
    });
    (
        server,
        client,
        server_conn,
        Connection::new(client_conn.unwrap()),
    )
}

async fn pending_stream(
    sender: &quinn::Connection,
    receiver: &mut Connection,
) -> (quinn::SendStream, RecvStream) {
    let mut send = sender.open_uni().await.unwrap();
    // Quinn only announces a stream once it is used. Consume this prefix first
    // so the next read is deterministically Pending while the sender stays open.
    send.write_all(b"prefix").await.unwrap();
    let mut recv = poll_fn(|cx| {
        <Connection as h3::quic::Connection<bytes::Bytes>>::poll_accept_recv(receiver, cx)
    })
    .await
    .unwrap();
    let mut prefix = Vec::new();
    while prefix.len() < 6 {
        prefix.extend_from_slice(&poll_fn(|cx| recv.poll_data(cx)).await.unwrap().unwrap());
    }
    assert_eq!(prefix, b"prefix");
    assert_pending(&mut recv);
    (send, recv)
}

fn assert_pending(recv: &mut RecvStream) {
    let mut cx = Context::from_waker(futures::task::noop_waker_ref());
    assert!(recv.poll_data(&mut cx).is_pending());
}

#[tokio::test]
async fn pending_read_keeps_id_and_resumes_in_order_through_eof() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let (_server, _client, sender, mut receiver) = connected().await;
        let (mut send, mut recv) = pending_stream(&sender, &mut receiver).await;
        // This call also panics on unpatched h3-quinn after Pending.
        assert_eq!(
            recv.recv_id(),
            h3::quic::StreamId::try_from(u64::from(send.id())).unwrap()
        );
        assert_pending(&mut recv);
        send.write_all(b"suffix").await.unwrap();
        send.finish().unwrap();
        let mut data = Vec::new();
        while let Some(chunk) = poll_fn(|cx| recv.poll_data(cx)).await.unwrap() {
            data.extend_from_slice(&chunk);
        }
        assert_eq!(data, b"suffix");
        assert_eq!(
            recv.recv_id(),
            h3::quic::StreamId::try_from(u64::from(send.id())).unwrap()
        );
    })
    .await
    .expect("loopback regression timed out");
}

#[tokio::test]
async fn stop_after_pending_read_reaches_peer_with_exact_h3_code() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let (_server, _client, sender, mut receiver) = connected().await;
        let (mut send, mut recv) = pending_stream(&sender, &mut receiver).await;
        // Exercise cancellation itself before querying the ID: the original
        // stop_sending unwrap must fail this test, not just the recv_id unwrap.
        let code = h3::error::Code::H3_REQUEST_CANCELLED.value();
        recv.stop_sending(code);
        assert_eq!(
            recv.recv_id(),
            h3::quic::StreamId::try_from(u64::from(send.id())).unwrap()
        );
        assert_eq!(
            send.stopped().await.unwrap(),
            Some(VarInt::from_u64(code).unwrap())
        );
        // Quinn records an explicit stop as all data read.
        assert!(poll_fn(|cx| recv.poll_data(cx)).await.unwrap().is_none());
        recv.stop_sending(code); // Already stopped remains harmless.
        drop(recv);
    })
    .await
    .expect("loopback regression timed out");
}

#[tokio::test]
async fn peer_reset_after_pending_read_preserves_application_code() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let (_server, _client, sender, mut receiver) = connected().await;
        let (mut send, mut recv) = pending_stream(&sender, &mut receiver).await;
        send.reset(VarInt::from_u32(0x123)).unwrap();
        assert!(matches!(
            poll_fn(|cx| recv.poll_data(cx)).await,
            Err(StreamErrorIncoming::StreamTerminated { error_code: 0x123 })
        ));
    })
    .await
    .expect("loopback regression timed out");
}

#[tokio::test]
async fn drop_after_pending_read_preserves_quinn_implicit_stop() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let (_server, _client, sender, mut receiver) = connected().await;
        let (mut send, recv) = pending_stream(&sender, &mut receiver).await;
        drop(recv);
        assert_eq!(send.stopped().await.unwrap(), Some(VarInt::from_u32(0)));
    })
    .await
    .expect("loopback regression timed out");
}

// Notify keeps a permit if the transport wakes before the waiter starts; the
// counter independently proves that the registered waker was actually invoked.
#[derive(Default)]
struct CountingWake {
    count: AtomicUsize,
    notified: tokio::sync::Notify,
}

impl Wake for CountingWake {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.count.fetch_add(1, Ordering::SeqCst);
        self.notified.notify_one();
    }
}

#[tokio::test]
async fn registered_pending_reader_is_woken_by_later_data_then_reads_eof() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let (_server, _client, sender, mut receiver) = connected().await;
        let (mut send, mut recv) = pending_stream(&sender, &mut receiver).await;
        let wake = Arc::new(CountingWake::default());
        let waker = std::task::Waker::from(wake.clone());
        assert!(recv
            .poll_data(&mut Context::from_waker(&waker))
            .is_pending());
        assert_eq!(wake.count.load(Ordering::SeqCst), 0);
        // No data/FIN has been sent when the counting waker is registered.
        send.write_all(b"later bytes").await.unwrap();
        send.finish().unwrap();
        wake.notified.notified().await;
        assert!(wake.count.load(Ordering::SeqCst) > 0);
        // Only now may the test repoll the receiver.
        let mut data = Vec::new();
        while let Some(chunk) = poll_fn(|cx| recv.poll_data(cx)).await.unwrap() {
            data.extend_from_slice(&chunk);
        }
        assert_eq!(data, b"later bytes");
    })
    .await
    .expect("registered-waker regression timed out");
}

#[tokio::test]
async fn production_incoming_body_drop_after_pending_cancels_real_h3_response() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let (_server, _client, server_quic, client_transport) = connected().await;
        let (mut driver, mut requests) = h3::client::new(client_transport).await.unwrap();
        let mut server =
            h3::server::Connection::<_, bytes::Bytes>::new(Connection::new(server_quic))
                .await
                .unwrap();
        let scenario = async {
            let mut request = requests
                .send_request(
                    http::Request::builder()
                        .uri("https://localhost/cancel")
                        .body(())
                        .unwrap(),
                )
                .await
                .unwrap();
            request.finish().await.unwrap();
            let (_, mut response) = server
                .accept()
                .await
                .unwrap()
                .unwrap()
                .resolve_request()
                .await
                .unwrap();
            response
                .send_response(http::Response::new(()))
                .await
                .unwrap();
            assert_eq!(
                request.recv_response().await.unwrap().status(),
                http::StatusCode::OK
            );
            let (_request_send, response_recv) = request.split();
            // The production public constructor wraps the actual H3 receive
            // stream, rather than reproducing its Drop implementation here.
            let mut body = h3_util::client_body::H3IncomingClient::new(response_recv, None);
            let wake = Arc::new(CountingWake::default());
            let waker = std::task::Waker::from(wake);
            assert!(Pin::new(&mut body)
                .poll_frame(&mut Context::from_waker(&waker))
                .is_pending());
            // The server has sent headers only and remains open: this Pending
            // must reach the native receive poll, with no body data queued.
            drop(body);
            // Public H3 send errors expose the peer's STOP_SENDING code. Keep
            // trying after yielding until that transport notification arrives;
            // the enclosing deadline bounds propagation and flow-control waits.
            loop {
                match response
                    .send_data(bytes::Bytes::from_static(b"after drop"))
                    .await
                {
                    Ok(()) => tokio::task::yield_now().await,
                    Err(h3::error::StreamError::RemoteTerminate { code, .. }) => {
                        assert_eq!(code, h3::error::Code::H3_REQUEST_CANCELLED);
                        break;
                    }
                    Err(error) => panic!("unexpected peer send error: {error:?}"),
                }
            }
        };
        // Drive H3 control streams without spawning a task that can outlive the
        // test. Both futures and both owned endpoints drop on timeout/failure.
        tokio::select! {
            _ = scenario => {},
            error = poll_fn(|cx| driver.poll_close(cx)) => {
                panic!("H3 driver closed before cancellation was observed: {error:?}");
            }
        }
    })
    .await
    .expect("production-body cancellation regression timed out");
}

use std::{future::poll_fn, sync::Arc, task::Context, time::Duration};

use h3::quic::{RecvStream as _, StreamErrorIncoming};
use h3_quinn::{Connection, RecvStream};
use quinn::{ClientConfig, Endpoint, ServerConfig, VarInt};
use rustls::pki_types::PrivatePkcs8KeyDer;

// Real loopback transport, with an ephemeral certificate explicitly trusted by
// this client. No permissive verifier and no production credentials are used.
async fn connected() -> (Endpoint, Endpoint, quinn::Connection, Connection) {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let key = PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der());
    let config = ServerConfig::with_single_cert(vec![cert.cert.der().clone()], key.into()).unwrap();
    let server = Endpoint::server(config, "127.0.0.1:0".parse().unwrap()).unwrap();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert.cert.der().clone()).unwrap();
    let mut client = Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
    client
        .set_default_client_config(ClientConfig::with_root_certificates(Arc::new(roots)).unwrap());
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

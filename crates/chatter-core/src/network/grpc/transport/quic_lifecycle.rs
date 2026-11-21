use std::task::{Context, Poll};

use h3::quic::{Connection, ConnectionErrorIncoming, OpenStreams, StreamErrorIncoming};
use h3_util::client::H3Connector;
use tonic_h3::quinn::{H3QuinnConnector, h3_quinn};

use super::BoxError;

// h3-util feeds connector errors back through the next RPC. Keep the stage
// attached so a handshake timeout cannot be mistaken for a live stream failure.
#[derive(Debug, thiserror::Error)]
#[error(transparent)]
pub(super) struct ConnectFailure(#[from] pub(super) BoxError);

#[derive(Clone)]
pub struct TrackedConnector(pub(super) H3QuinnConnector);

impl H3Connector for TrackedConnector {
    type CONN = TrackedConnection;
    type OS = h3_quinn::OpenStreams;
    type SS = h3_quinn::SendStream<bytes::Bytes>;
    type RS = h3_quinn::RecvStream;
    type BS = h3_quinn::BidiStream<bytes::Bytes>;

    async fn connect(&self) -> Result<Self::CONN, BoxError> {
        let connection = self.0.connect().await.map_err(ConnectFailure)?;
        tracing::event!(target: "network_transport::h3_connected", tracing::Level::INFO, {});
        Ok(TrackedConnection(connection))
    }
}

// The h3 driver owns this connection. Observe the end of that ownership
// without holding another Quinn handle that would keep the socket alive.
pub struct TrackedConnection(h3_quinn::Connection);

impl Drop for TrackedConnection {
    fn drop(&mut self) {
        tracing::event!(target: "network_transport::h3_driver_closed", tracing::Level::INFO, {});
    }
}

impl Connection<bytes::Bytes> for TrackedConnection {
    type RecvStream = h3_quinn::RecvStream;
    type OpenStreams = h3_quinn::OpenStreams;

    fn poll_accept_recv(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Self::RecvStream, ConnectionErrorIncoming>> {
        Connection::<bytes::Bytes>::poll_accept_recv(&mut self.0, cx)
    }

    fn poll_accept_bidi(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Self::BidiStream, ConnectionErrorIncoming>> {
        Connection::<bytes::Bytes>::poll_accept_bidi(&mut self.0, cx)
    }

    fn opener(&self) -> Self::OpenStreams {
        Connection::<bytes::Bytes>::opener(&self.0)
    }
}

impl OpenStreams<bytes::Bytes> for TrackedConnection {
    type BidiStream = h3_quinn::BidiStream<bytes::Bytes>;
    type SendStream = h3_quinn::SendStream<bytes::Bytes>;

    fn poll_open_bidi(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Self::BidiStream, StreamErrorIncoming>> {
        OpenStreams::<bytes::Bytes>::poll_open_bidi(&mut self.0, cx)
    }

    fn poll_open_send(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Self::SendStream, StreamErrorIncoming>> {
        OpenStreams::<bytes::Bytes>::poll_open_send(&mut self.0, cx)
    }

    fn close(&mut self, code: h3::error::Code, reason: &[u8]) {
        OpenStreams::<bytes::Bytes>::close(&mut self.0, code, reason);
    }
}

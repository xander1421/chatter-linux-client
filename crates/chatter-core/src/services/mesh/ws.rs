//! WebSocket transport for Nostr relays, over an arbitrary byte stream.
//!
//! `chatter_mesh_nostr` deliberately declares no WebSocket or TLS stack: its
//! [`RelayConnector`](chatter_mesh_nostr::relay::RelayConnector) is injected so the
//! integration layer can supply a connection over the Tor client this crate already
//! owns. This module supplies one — nothing more. It opens no sockets and knows
//! nothing about relays.
//!
//! # Why this is an adapter and not an implementation
//!
//! An earlier version hand-rolled RFC 6455 here: framing, masking, continuation,
//! the close handshake, and a SHA-1 for `Sec-WebSocket-Accept`. Around 770 lines,
//! and tested. It was written that way because chatter-core declared no WebSocket
//! crate.
//!
//! It was still the wrong shape. This code parses frames from relays we neither
//! control nor authenticate; extended lengths, continuation chains and masking are
//! all attacker-influenced inputs, and a hand-written SHA-1 sitting next to them is
//! fine right up until it is not. `tungstenite` was already in the lockfile
//! transitively, so the hand-rolled version bought nothing a vetted implementation
//! does not already provide.
//!
//! Non-text frames are swallowed rather than surfaced, per the [`RelayStream`]
//! contract: ping/pong is tungstenite's business and Nostr is a text protocol.

use chatter_mesh_nostr::NostrError;
use chatter_mesh_nostr::relay::{BoxFuture, RelaySink, RelayStream};
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;

/// Read half of an upgraded connection.
pub struct WsStream<S> {
    inner: SplitStream<WebSocketStream<S>>,
}

/// Write half of an upgraded connection.
pub struct WsSink<S> {
    inner: SplitSink<WebSocketStream<S>, Message>,
}

/// Performs the upgrade over an already-connected stream, then splits it.
///
/// `authority` is the `host[:port]` the relay expects in its `Host` header and
/// `path` the request target. TLS is the caller's: `stream` is whatever it has
/// already negotiated, which here is rustls over a Tor `DataStream`.
pub async fn handshake<S>(
    stream: S,
    authority: &str,
    path: &str,
) -> Result<(WsSink<S>, WsStream<S>), NostrError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let url = format!("wss://{authority}{path}");
    let (socket, _response) = tokio_tungstenite::client_async(&url, stream)
        .await
        .map_err(|error| NostrError::Relay(format!("websocket upgrade failed: {error}")))?;

    let (sink, source) = socket.split();
    Ok((WsSink { inner: sink }, WsStream { inner: source }))
}

impl<S> RelaySink for WsSink<S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    fn send(&mut self, text: String) -> BoxFuture<'_, Result<(), NostrError>> {
        Box::pin(async move {
            self.inner
                .send(Message::Text(text.into()))
                .await
                .map_err(|error| NostrError::Relay(format!("websocket send failed: {error}")))
        })
    }

    fn close(&mut self) -> BoxFuture<'_, Result<(), NostrError>> {
        Box::pin(async move {
            // `SinkExt::close` emits the close frame and flushes. A relay that has
            // already gone away makes this fail, which is not worth surfacing: the
            // caller's only reaction would be to drop the socket, which is what
            // happens anyway.
            self.inner
                .close()
                .await
                .map_err(|error| NostrError::Relay(format!("websocket close failed: {error}")))
        })
    }
}

impl<S> RelayStream for WsStream<S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    fn recv(&mut self) -> BoxFuture<'_, Result<Option<String>, NostrError>> {
        Box::pin(async move {
            loop {
                match self.inner.next().await {
                    None => return Ok(None),
                    Some(Ok(Message::Text(text))) => return Ok(Some(text.to_string())),
                    Some(Ok(Message::Close(_))) => return Ok(None),
                    // Swallowed per the RelayStream contract. tungstenite answers
                    // pings itself; binary and raw frames are not Nostr.
                    Some(Ok(
                        Message::Binary(_) | Message::Ping(_) | Message::Pong(_) | Message::Frame(_),
                    )) => continue,
                    Some(Err(error)) => {
                        return Err(NostrError::Relay(format!("websocket read failed: {error}")));
                    }
                }
            }
        })
    }
}

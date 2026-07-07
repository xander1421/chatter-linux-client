//! Tor v3 onion transport connector for the gRPC pool (`onion` feature only).
//!
//! Bridges tonic's channel builder onto an Arti-managed Tor circuit: when the
//! pool is in [`TransportMode::Onion`](super::client_pool::TransportMode), the
//! channel is built with [`OnionConnector`] instead of the default TCP dialer.
//! Each dial hands tonic a [`DataStream`] tunnelled through Tor to the pinned
//! `.onion`, which hides both the client's and the server's IP.
//!
//! The [`arti_client::TorClient`] bootstraps **lazily** on the first dial
//! (bootstrap walks the Tor directory and is slow) and is then cached in a
//! shared [`OnceCell`] so every reconnect reuses the same warm client.
//!
//! # Scope (Stage 2)
//! One Tor circuit for the whole pool. That already hides client + server IP.
//! Per-purpose circuit isolation (anonymous mailbox polling on a different
//! circuit than authenticated RPCs, via arti `StreamPrefs`/`isolated_client`)
//! is a deliberate follow-up — see the `TODO(onion)` in [`OnionConnector::call`].

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use arti_client::{DataStream, TorClient, TorClientConfig};
use http::Uri;
use hyper_util::rt::TokioIo;
use tokio::sync::OnceCell;
use tor_rtcompat::PreferredRuntime;
use tower::Service;

/// The bootstrapped Tor client, shared across the pool so bootstrap happens once.
pub type SharedTorClient = Arc<OnceCell<TorClient<PreferredRuntime>>>;

/// A `tower::Service<Uri>` that dials the pinned `.onion` over Tor.
///
/// Cheaply cloneable — tonic clones the connector internally. All clones share
/// the same lazily-bootstrapped [`TorClient`] via the inner [`OnceCell`].
#[derive(Clone)]
pub struct OnionConnector {
    tor: SharedTorClient,
    /// When set, dials over an ISOLATED Tor circuit (a separate client derived
    /// via `isolated_client()`), so this connector's traffic shares no circuit
    /// with the pool's main (authenticated) connector. Used for the anonymous
    /// mailbox channel so the server can't link anon ops to the account by
    /// circuit. Cached so all anon ops reuse one isolated circuit-group.
    isolated: Option<SharedTorClient>,
}

impl OnionConnector {
    /// Build a connector over the shared (authenticated) Tor client.
    pub fn new(tor: SharedTorClient) -> Self {
        Self { tor, isolated: None }
    }

    /// Build a connector that dials over a SEPARATE, isolated circuit — for the
    /// anonymous mailbox channel. `iso` caches the isolated client so every anon
    /// dial reuses the same isolated circuit-group (distinct from auth).
    pub fn new_isolated(tor: SharedTorClient, iso: SharedTorClient) -> Self {
        Self { tor, isolated: Some(iso) }
    }
}

fn io_err(msg: impl Into<String>) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Other, msg.into())
}

impl Service<Uri> for OnionConnector {
    /// tonic needs a hyper IO handle; `TokioIo` adapts arti's tokio-flavoured
    /// `DataStream` into hyper's `Read`/`Write`.
    type Response = TokioIo<DataStream>;
    type Error = std::io::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, uri: Uri) -> Self::Future {
        let tor = self.tor.clone();
        let isolated = self.isolated.clone();
        Box::pin(async move {
            // The endpoint is built from `http://<onion>:443`, so host+port come
            // straight off the URI. Default to 443 to match the pinned form.
            let host = uri
                .host()
                .ok_or_else(|| io_err("onion transport: endpoint URI has no host"))?
                .to_string();
            let port = uri.port_u16().unwrap_or(443);

            // Lazily bootstrap the Tor client on the first dial, then reuse it.
            let base = tor
                .get_or_try_init(|| async {
                    TorClient::create_bootstrapped(TorClientConfig::default()).await
                })
                .await
                .map_err(|e| io_err(format!("onion transport: Tor bootstrap failed: {e}")))?;

            // Circuit isolation: the anon connector dials over a separate isolated
            // client (its own circuit-group), so the server can't correlate anon
            // mailbox ops with authenticated RPCs by circuit. The auth connector
            // uses the base client directly.
            let client = match &isolated {
                Some(cell) => cell.get_or_init(|| async { base.isolated_client() }).await,
                None => base,
            };

            let stream = client
                .connect((host.as_str(), port))
                .await
                .map_err(|e| io_err(format!("onion transport: Tor connect to {host}:{port} failed: {e}")))?;

            Ok(TokioIo::new(stream))
        })
    }
}

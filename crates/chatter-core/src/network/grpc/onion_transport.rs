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
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};

use std::time::Duration;

use arti_client::{DataStream, TorClient, TorClientConfig};
use http::Uri;
use hyper_util::rt::TokioIo;
use tokio::sync::OnceCell;
use tor_rtcompat::PreferredRuntime;
use tower::Service;

/// Dedicated multi-thread runtime that owns ALL Arti work — bootstrap, the
/// directory manager, and every circuit reactor.
///
/// The app's main runtime (`#[tokio::main]`) parks its worker threads on
/// synchronous SQLCipher/`parking_lot` DB calls (the storage layer is sync and
/// mostly runs inline, not via `spawn_blocking`). When onion mode fans out its
/// poll loops, those parks can occupy every worker at once — and if Arti shared
/// that runtime, its directory/circuit tasks would stop being polled and the
/// onion connect would freeze mid-bootstrap (observed: `dir.sqlite3` writes and
/// the connect stall at the same instant). Isolating Arti on its own threads is
/// the same move the Signal actor and call worker make for their workloads.
///
/// `TorClient::create_bootstrapped` binds Arti to `PreferredRuntime::current()`,
/// so it MUST be spawned onto this runtime (see `call`). The app runtime then
/// only awaits a `JoinHandle` — a channel wake, never the Tor CPU work.
///
/// 2 worker threads — ample for one/two onion connections' directory
/// + circuit work. Bump if many concurrent circuits ever contend.
pub(crate) fn tor_runtime() -> &'static tokio::runtime::Handle {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("chatter-tor")
            .build()
            .expect("failed to build dedicated Tor runtime")
    })
    .handle()
}

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
            // The bootstrap is SPAWNED onto the dedicated Tor runtime (not awaited
            // inline) so Arti binds to that runtime's threads — see [`tor_runtime`].
            // The app runtime only awaits the JoinHandle. A 120s ceiling turns a
            // stalled bootstrap (censored/blocked directory) into a retryable error
            // instead of a permanent wedge — `get_or_try_init` caches only Ok, so a
            // failed attempt is re-tried on the next dial.
            let base = tor
                .get_or_try_init(|| async {
                    let jh = tor_runtime().spawn(async {
                        // Arti's rustls runtime needs a process-wide default rustls
                        // CryptoProvider. Without it, guard/HS-descriptor/rendezvous
                        // TLS silently fails ("no CryptoProvider is installed").
                        // Idempotent — Err means another init already installed one.
                        let _ = tokio_rustls::rustls::crypto::ring::default_provider()
                            .install_default();
                        TorClient::create_bootstrapped(TorClientConfig::default()).await
                    });
                    match tokio::time::timeout(Duration::from_secs(120), jh).await {
                        Err(_) => Err(io_err("onion transport: Tor bootstrap timed out after 120s")),
                        Ok(Err(join)) => Err(io_err(format!("onion transport: Tor bootstrap task panicked: {join}"))),
                        Ok(Ok(Err(e))) => Err(io_err(format!("onion transport: Tor bootstrap failed: {e}"))),
                        Ok(Ok(Ok(c))) => Ok(c),
                    }
                })
                .await?;

            // Circuit isolation: the anon connector dials over a separate isolated
            // client (its own circuit-group), so the server can't correlate anon
            // mailbox ops with authenticated RPCs by circuit. The auth connector
            // uses the base client directly. Cloning a TorClient is cheap (Arc) and
            // keeps the client bound to the Tor runtime.
            let client = match &isolated {
                Some(cell) => cell.get_or_init(|| async { base.isolated_client() }).await.clone(),
                None => base.clone(),
            };

            // Dial the HS on the dedicated Tor runtime too, so the connect
            // coordination can't be starved by app-side worker parking either.
            // Bound it: Arti's `connect()` has no internal deadline, and tonic's
            // `Endpoint::connect_timeout` does NOT wrap a custom
            // `connect_with_connector` future — so without this a stuck rendezvous
            // (dead HSDir/intro circuit) hangs forever. 90s covers a cold HS connect
            // with one circuit retry (~24s observed) plus generous slack.
            let dial_host = host.clone();
            let jh = tor_runtime()
                .spawn(async move { client.connect((dial_host.as_str(), port)).await });
            let stream = match tokio::time::timeout(Duration::from_secs(90), jh).await {
                Err(_) => {
                    tracing::error!(host = %host, port, "onion transport: Tor connect timed out after 90s");
                    return Err(io_err(format!("onion transport: Tor connect to {host}:{port} timed out after 90s")));
                }
                Ok(Err(join)) => {
                    return Err(io_err(format!("onion transport: Tor connect task panicked: {join}")));
                }
                Ok(Ok(Err(e))) => {
                    // Surface the real Arti error — tonic otherwise collapses it
                    // into an opaque "Unknown"/"server unavailable". `{e:?}` gives
                    // the full source chain (descriptor fetch / rendezvous / etc).
                    tracing::error!(host = %host, port, error = ?e, "onion transport: Tor connect failed");
                    return Err(io_err(format!("onion transport: Tor connect to {host}:{port} failed: {e}")));
                }
                Ok(Ok(Ok(s))) => s,
            };

            Ok(TokioIo::new(stream))
        })
    }
}

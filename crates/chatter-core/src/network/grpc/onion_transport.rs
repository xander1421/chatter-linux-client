//! Tor v3 onion transport connector for the gRPC pool (`onion` feature only).
//!
//! Bridges tonic's channel builder onto an Arti-managed Tor circuit: when the
//! pool is in [`TransportMode::Onion`](super::client_pool::TransportMode), the
//! channel is built with [`OnionConnector`] instead of the default TCP dialer.
//! Each dial hands tonic a [`DataStream`] tunnelled through Tor to the pinned
//! `.onion`, which hides both the client's and the server's IP.
//!
//! The [`arti_client::TorClient`] is prewarmed at app launch, with first-dial retry
//! (bootstrap walks the Tor directory and is slow) and is then cached in a
//! shared [`OnceCell`] so every reconnect reuses the same warm client.
//!
//! # Scope (Stage 2)
//! One Tor circuit for the whole pool. That already hides client + server IP.
//! Per-purpose circuit isolation (anonymous mailbox polling on a different
//! circuit than authenticated RPCs, via arti `StreamPrefs`/`isolated_client`)
//! is a deliberate follow-up — see the `TODO(onion)` in [`OnionConnector::call`].

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};

use std::time::{Duration, Instant};

use arti_client::{DataStream, HasKind, TorClient, TorClientConfig};
use http::Uri;
use hyper_util::rt::TokioIo;
use tokio::sync::OnceCell;
use tor_rtcompat::PreferredRuntime;
use tower::Service;

/// Phase budgets: 120s bootstrap (including shared-initializer wait), then
/// 90s hidden-service dial. The total cap also bounds OnceCell contention.
pub(crate) const BOOTSTRAP_TIMEOUT: Duration = Duration::from_secs(120);
pub(crate) const DIAL_TIMEOUT: Duration = Duration::from_secs(90);
const FIRST_DIAL_TIMEOUT: Duration = Duration::from_secs(45);
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(210);
static ACTIVE_DIALS: AtomicUsize = AtomicUsize::new(0);

/// Used only by the opt-in diagnostic layer; it never changes Tor behavior.
pub fn stage_trace_active() -> bool {
    ACTIVE_DIALS.load(Ordering::Relaxed) != 0
}

struct ActiveDial;
impl ActiveDial {
    fn new() -> Self {
        ACTIVE_DIALS.fetch_add(1, Ordering::Relaxed);
        Self
    }
}
impl Drop for ActiveDial {
    fn drop(&mut self) {
        ACTIVE_DIALS.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Dropping a JoinHandle detaches its task. Keep ownership until completion so
/// phase timeout, overall timeout, and caller cancellation all request abort.
/// Tokio drops the task future on its next poll; no detached bootstrap/dial
/// can later return a client/stream. Arti's cached client's internal reactors
/// remain client-owned, intentionally reusable across successful reconnects.
struct AbortOnDrop<T>(tokio::task::JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Logs phase metadata only: never hosts, identities, RPC data or Arti errors.
struct PhaseTrace {
    phase: &'static str,
    started: Instant,
    outcome: &'static str,
}

impl PhaseTrace {
    fn new(phase: &'static str) -> Self {
        tracing::info!(phase, "onion connector phase started");
        Self {
            phase,
            started: Instant::now(),
            outcome: "cancelled",
        }
    }
}

impl Drop for PhaseTrace {
    fn drop(&mut self) {
        tracing::info!(
            phase = self.phase,
            outcome = self.outcome,
            elapsed_ms = self.started.elapsed().as_millis() as u64,
            "onion connector phase ended"
        );
    }
}

async fn await_tor_task<T, E: std::fmt::Display>(
    task: tokio::task::JoinHandle<Result<T, E>>,
    limit: Duration,
    phase: &'static str,
) -> Result<T, std::io::Error> {
    let mut task = AbortOnDrop(task);
    let mut trace = PhaseTrace::new(phase);
    match tokio::time::timeout(limit, &mut task.0).await {
        Err(_) => {
            trace.outcome = "timeout";
            Err(io_err(format!(
                "onion transport: {phase} timed out after {}s",
                limit.as_secs()
            )))
        }
        Ok(Err(join)) => {
            trace.outcome = "task_failed";
            Err(io_err(format!(
                "onion transport: {phase} task failed: {join}"
            )))
        }
        Ok(Ok(Err(error))) => {
            trace.outcome = "failed";
            // Preserve diagnostic errors for the caller; phase tracing above
            // deliberately never emits the Arti error or its source chain.
            Err(io_err(format!("onion transport: {phase} failed: {error}")))
        }
        Ok(Ok(Ok(value))) => {
            trace.outcome = "completed";
            Ok(value)
        }
    }
}

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
/// Arti's state and directory-cache locations, under the app's own data and
/// cache directories, registered once at startup ([`set_tor_directories`]).
/// `TorClientConfig::default()` derives them from `$HOME`/XDG, which an Android
/// app does not have: Tor then refuses to start with "invalid configuration".
static TOR_DIRS: OnceLock<(PathBuf, PathBuf)> = OnceLock::new();

/// Register where Arti keeps its state and cache. First call wins.
pub fn set_tor_directories(state_dir: PathBuf, cache_dir: PathBuf) {
    let _ = TOR_DIRS.set((state_dir, cache_dir));
}

/// The one Tor client configuration every Arti bootstrap in the app uses.
pub(crate) fn tor_client_config() -> Result<TorClientConfig, String> {
    let (state, cache) = TOR_DIRS
        .get()
        .ok_or_else(|| "Tor storage directories are not set".to_string())?;
    let mut builder = arti_client::config::TorClientConfigBuilder::from_directories(state, cache);
    set_cbt_floor(&mut builder);
    builder
        .build()
        .map_err(|e| format!("Tor configuration: {e}"))
}

fn set_cbt_floor(builder: &mut arti_client::config::TorClientConfigBuilder) {
    // Arti 0.47's consensus default is only 10 ms. A learned sub-second CBT
    // persisted across restarts can discard otherwise healthy three-hop paths.
    builder
        .override_net_params()
        .insert("cbtmintimeout".into(), 10_000);
}

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
pub type SharedTorClient = Arc<OnceCell<Arc<TorClient<PreferredRuntime>>>>;

/// Start directory/guard bootstrap while the app is initializing. No service
/// connection is opened here, and secret/unidentified isolation is still derived
/// only at dial time. A failed warmup is retryable through the same OnceCell.
pub fn prewarm(tor: SharedTorClient) {
    tor_runtime().spawn(async move {
        if shared_bootstrap(&tor).await.is_err() {
            tracing::warn!("Tor startup warmup failed; next request will retry");
        }
    });
}

async fn shared_bootstrap(
    tor: &SharedTorClient,
) -> Result<&Arc<TorClient<PreferredRuntime>>, std::io::Error> {
    let base_result = tokio::time::timeout(
        BOOTSTRAP_TIMEOUT,
        tor.get_or_try_init(|| async {
            let config =
                tor_client_config().map_err(|e| io_err(format!("onion transport: {e}")))?;
            let jh = tor_runtime().spawn(async {
                // Arti's rustls runtime needs a process-wide default rustls
                // CryptoProvider. Without it, guard/HS-descriptor/rendezvous
                // TLS silently fails ("no CryptoProvider is installed").
                // Idempotent — Err means another init already installed one.
                let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
                TorClient::create_bootstrapped(config).await
            });
            await_tor_task(jh, BOOTSTRAP_TIMEOUT, "bootstrap").await
        }),
    )
    .await;
    base_result.map_err(|_| io_err("onion transport: shared Tor bootstrap timed out after 120s"))?
}

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
        Self {
            tor,
            isolated: None,
        }
    }

    /// Build a connector that dials over a SEPARATE, isolated circuit — for the
    /// anonymous mailbox channel. `iso` caches the isolated client so every anon
    /// dial reuses the same isolated circuit-group (distinct from auth).
    pub fn new_isolated(tor: SharedTorClient, iso: SharedTorClient) -> Self {
        Self {
            tor,
            isolated: Some(iso),
        }
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
            let _active_dial = ActiveDial::new();
            let mut trace = PhaseTrace::new("connector");
            let result = tokio::time::timeout(CONNECT_TIMEOUT, async move {
                // The endpoint is built from `http://<onion>:443`, so host+port come
                // straight off the URI. Default to 443 to match the pinned form.
                let host = uri
                    .host()
                    .ok_or_else(|| io_err("onion transport: endpoint URI has no host"))?
                    .to_string();
                // This connector exists to reach the pinned hidden service and
                // nothing else. A clearnet host here would mean the pool's endpoint
                // and transport disagree; carrying it out through a Tor exit would
                // quietly "work" while bypassing the onion service, so refuse.
                if !host.to_ascii_lowercase().ends_with(".onion") {
                    return Err(io_err(format!(
                        "onion transport: refusing non-.onion host {host}"
                    )));
                }
                let port = uri.port_u16().unwrap_or(443);

                // Reuse launch warmup, or initialize here if it failed/was not run.
                let mut bootstrap_trace = PhaseTrace::new("bootstrap_wait");
                let result = shared_bootstrap(&tor).await;
                bootstrap_trace.outcome = if result.is_ok() {
                    "completed"
                } else {
                    "failed"
                };
                drop(bootstrap_trace);
                let base = result?;
                // get_or_try_init publishes only success. Cancellation drops the
                // initializer and releases its permit, so the next waiter retries;
                // await_tor_task's guard aborts the initializer's spawned work.

                // Circuit isolation: the anon connector dials over a separate isolated
                // client (its own circuit-group), so the server can't correlate anon
                // mailbox ops with authenticated RPCs by circuit. The auth connector
                // uses the base client directly. Cloning a TorClient is cheap (Arc) and
                // keeps the client bound to the Tor runtime.
                let client = match &isolated {
                    Some(cell) => cell
                        .get_or_init(|| async { base.isolated_client() })
                        .await
                        .clone(),
                    None => base.clone(),
                };

                // tonic DOES wrap custom connectors in connect_timeout. Its
                // endpoint budget allows our 120s + 90s phases plus scheduling slack.
                // The inner 210s cap bounds this connector even for lazy channels.
                let jh = tor_runtime().spawn(async move {
                    // Arti keys its hidden-service connection state (including a
                    // Working task and cached descriptor) by circuit isolation.
                    // A cancelled caller can leave that task running. Repeating
                    // connect on the same client then waits on the same stalled
                    // work, as RC119's three consecutive 90s deadlines did.
                    // Give the first attempt half the budget. Every failed
                    // attempt gets a new isolation token: Arti 0.47 caches the
                    // descriptor and in-flight work by service and isolation,
                    // and only an introduction NACK forces a descriptor refetch.
                    // A completed descriptor/intro/rendezvous error must not
                    // reuse that state on the final attempt.
                    let first = tokio::time::timeout(
                        FIRST_DIAL_TIMEOUT,
                        client.connect((host.as_str(), port)),
                    )
                    .await;
                    match first {
                        Ok(Ok(stream)) => return Ok(stream),
                        Ok(Err(error)) => {
                            tracing::warn!(
                                error_kind = ?error.kind(),
                                "onion dial failed; retrying with fresh Tor isolation"
                            );
                            tokio::time::sleep(Duration::from_millis(500)).await;
                        }
                        Err(_) => {
                            tracing::warn!(
                                phase = "hidden_service_connect",
                                elapsed_ms = FIRST_DIAL_TIMEOUT.as_millis() as u64,
                                "onion dial stalled; retrying with fresh Tor isolation"
                            );
                        }
                    }
                    let retry_client = client.isolated_client();
                    let result = retry_client.connect((host.as_str(), port)).await;
                    if let Err(error) = &result {
                        tracing::warn!(error_kind = ?error.kind(), "onion dial retry failed");
                    }
                    result
                });
                let stream = await_tor_task(jh, DIAL_TIMEOUT, "dial").await?;

                Ok(TokioIo::new(stream))
            })
            .await;
            trace.outcome = match &result {
                Err(_) => "timeout",
                Ok(Err(_)) => "failed",
                Ok(Ok(_)) => "completed",
            };
            result.map_err(|_| io_err("onion transport: connector timed out after 210s"))?
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::oneshot;

    #[test]
    fn circuit_build_timeout_floor_is_ten_seconds() {
        let mut builder = arti_client::config::TorClientConfigBuilder::from_directories(
            "/tmp/chatter-tor-state",
            "/tmp/chatter-tor-cache",
        );
        set_cbt_floor(&mut builder);
        assert_eq!(
            builder.override_net_params().get("cbtmintimeout"),
            Some(&10_000)
        );
        builder
            .build()
            .expect("CBT floor is a valid Arti configuration");
    }

    #[test]
    fn trace_gate_tracks_overlapping_dials() {
        assert!(!stage_trace_active());
        let first = ActiveDial::new();
        let second = ActiveDial::new();
        assert!(stage_trace_active());
        drop(first);
        assert!(stage_trace_active());
        drop(second);
        assert!(!stage_trace_active());
    }

    struct DropSignal(Option<oneshot::Sender<()>>);
    impl Drop for DropSignal {
        fn drop(&mut self) {
            let _ = self.0.take().unwrap().send(());
        }
    }

    /// Use the production dedicated runtime: cancellation must cross runtimes.
    fn pending_task() -> (
        tokio::task::JoinHandle<Result<(), &'static str>>,
        oneshot::Receiver<()>,
        oneshot::Receiver<()>,
    ) {
        let (started_tx, started) = oneshot::channel();
        let (dropped_tx, dropped) = oneshot::channel();
        let task = tor_runtime().spawn(async move {
            let _signal = DropSignal(Some(dropped_tx));
            let _ = started_tx.send(());
            std::future::pending::<Result<(), &'static str>>().await
        });
        (task, started, dropped)
    }

    async fn signal(rx: oneshot::Receiver<()>) {
        tokio::time::timeout(Duration::from_secs(2), rx)
            .await
            .expect("task lifecycle signal timed out")
            .expect("task lifecycle signal lost");
    }

    #[tokio::test]
    async fn phase_timeout_aborts_remote_work() {
        let (task, started, dropped) = pending_task();
        signal(started).await;
        let error = await_tor_task(task, Duration::from_millis(10), "dial")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("dial timed out"));
        signal(dropped).await;
    }

    #[tokio::test]
    async fn caller_cancellation_aborts_remote_work_and_allows_cell_retry() {
        let cell = Arc::new(OnceCell::<()>::new());
        let init_cell = cell.clone();
        let (task, started, dropped) = pending_task();
        let (owned_tx, owned) = oneshot::channel();
        let initializer = tokio::spawn(async move {
            init_cell
                .get_or_try_init(|| async move {
                    // Establish task ownership before announcing the cancellation
                    // point; the JoinHandle must never be dropped unguarded.
                    let mut wait = Box::pin(await_tor_task(task, BOOTSTRAP_TIMEOUT, "bootstrap"));
                    assert!(
                        std::future::poll_fn(|cx| {
                            Poll::Ready(wait.as_mut().poll(cx).is_pending())
                        })
                        .await
                    );
                    let _ = owned_tx.send(());
                    wait.await
                })
                .await
                .map(|_| ())
        });
        signal(started).await;
        signal(owned).await;
        initializer.abort();
        assert!(initializer.await.unwrap_err().is_cancelled());
        signal(dropped).await;
        assert!(!cell.initialized());
        cell.get_or_try_init(|| async { Ok::<(), std::io::Error>(()) })
            .await
            .unwrap();
        assert!(cell.initialized());
    }

    #[tokio::test]
    async fn failed_initializer_is_retryable_and_success_is_cached() {
        let cell = OnceCell::new();
        assert!(
            cell.get_or_try_init(|| async {
                await_tor_task(
                    tor_runtime().spawn(async { Err::<(), _>("synthetic bootstrap failure") }),
                    BOOTSTRAP_TIMEOUT,
                    "bootstrap",
                )
                .await
            })
            .await
            .is_err()
        );
        assert!(!cell.initialized());
        cell.get_or_try_init(|| async { Ok::<(), std::io::Error>(()) })
            .await
            .unwrap();
        cell.get_or_try_init(|| async {
            panic!("successful bootstrap must be reused");
            #[allow(unreachable_code)]
            Ok::<(), std::io::Error>(())
        })
        .await
        .unwrap();
    }
}

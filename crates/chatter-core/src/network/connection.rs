//! Connection, server-reachability and Tor status for the UI.

/// Failure reasons for terminal connection states.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailReason {
    /// Authentication token expired, needs re-login.
    AuthExpired,
    /// Device was unlinked from account.
    DeviceDelinked,
    /// Maximum retry attempts exceeded.
    MaxRetriesExceeded,
}

/// Tor onion transport status, for the UI indicator.
///
/// Reflects the *Circuit + RPC* signal: `Connected` means an onion RPC has
/// actually round-tripped to the server (not merely that a circuit was built).
/// Only meaningful while anon/shield mode is on; `Off` otherwise.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum TorStatus {
    /// Not using onion transport (direct mode / shield off).
    #[default]
    Off,
    /// Switched to onion; bootstrapping/dialing, no successful RPC yet.
    Connecting,
    /// An onion RPC round-tripped — server reachable over Tor.
    Connected,
    /// Onion connect or poll timed out / failed.
    Error,
    /// Shield is on but the switch to onion failed outright (no `.onion`
    /// pinned, no `onion` feature): traffic is going DIRECT, so the network
    /// location is NOT hidden. Persists until Shield is toggled — no retry
    /// will change it.
    Unavailable,
}

impl TorStatus {
    /// Stable integer for the Slint global (0=Off,1=Connecting,2=Connected,3=Error,4=Unavailable).
    pub fn as_i32(self) -> i32 {
        match self {
            Self::Off => 0,
            Self::Connecting => 1,
            Self::Connected => 2,
            Self::Error => 3,
            Self::Unavailable => 4,
        }
    }

    fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Connecting,
            2 => Self::Connected,
            3 => Self::Error,
            4 => Self::Unavailable,
            _ => Self::Off,
        }
    }
}

/// Process-global Tor status, mirrored to the UI indicator.
///
/// A single global (like `ANON_HTTP_ACTIVE`) avoids threading an `OfflineState`
/// handle through the whole inbox provider/poller builder chain. Written from:
/// the anon toggle (`Connecting`/`Off`) and the inbox poller's onion RPC outcome
/// (`Connected` on success, `Error` on timeout). The UI bridge reads it on a
/// short timer and pushes to the Slint `TorStatus` global.
static TOR_STATUS: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// Set the process-global Tor status.
pub fn set_tor_status(status: TorStatus) {
    TOR_STATUS.store(status.as_i32() as u8, std::sync::atomic::Ordering::Release);
}

/// Read the process-global Tor status.
pub fn tor_status() -> TorStatus {
    TorStatus::from_u8(TOR_STATUS.load(std::sync::atomic::Ordering::Acquire))
}

/// Count of ntfy push SSE streams currently connected and consuming events.
///
/// The background inbox poller reads this to decide its fallback cadence: while
/// at least one push stream is live, a real wake arrives in ~250ms via
/// `provider.wake()`, so the blind safety poll can relax from 30s to minutes.
/// If every stream drops, this returns to 0 and the poller tightens back to 30s
/// on its own — no trust in push beyond what is actually connected right now.
static PUSH_STREAMS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// RAII presence for one connected push SSE stream. Increments on creation and
/// decrements on drop, so a stream that ends, errors, or panics is counted out.
#[must_use = "hold the guard for the lifetime of the connected SSE stream"]
pub struct PushStreamPresence(());

pub fn push_stream_connected() -> PushStreamPresence {
    PUSH_STREAMS.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    PushStreamPresence(())
}

impl Drop for PushStreamPresence {
    fn drop(&mut self) {
        PUSH_STREAMS.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

/// Whether any ntfy push SSE stream is currently connected.
pub fn push_is_connected() -> bool {
    PUSH_STREAMS.load(std::sync::atomic::Ordering::Acquire) > 0
}

/// Process-global app foreground/background state (true = visible/foreground).
///
/// The single source of truth for the Signal-mobile background model: driven by
/// `ChatterClient::set_app_visible`, read and awaited by every background loop
/// (inbox poll pacing, user-events stream, channel stream, recovery tickers) so
/// they go quiet when backgrounded and resume on foreground — without threading
/// a receiver through each service's constructor. Defaults visible so desktop,
/// tests, and headless startup behave as before until Android reports otherwise.
static APP_VISIBLE: std::sync::LazyLock<tokio::sync::watch::Sender<bool>> =
    std::sync::LazyLock::new(|| tokio::sync::watch::channel(true).0);

/// Set the process-global app visibility (foreground = true). Only an actual
/// edge marks the watch changed, so parked subscribers wake once per transition.
pub fn set_app_visible(visible: bool) {
    APP_VISIBLE.send_if_modified(|current| {
        if *current != visible {
            *current = visible;
            true
        } else {
            false
        }
    });
}

/// A receiver that tracks app foreground/background transitions.
pub fn subscribe_app_visible() -> tokio::sync::watch::Receiver<bool> {
    APP_VISIBLE.subscribe()
}

/// Whether the app is currently in the foreground.
pub fn app_is_visible() -> bool {
    *APP_VISIBLE.borrow()
}

/// Park the caller while the app is backgrounded; return once it is foreground.
///
/// For background recovery/retry loops (DM outbox replay, receipt recovery,
/// saved-message sync, DM-bootstrap recovery) that should go quiet when
/// backgrounded and resume on foreground. Returns `false` if the visibility
/// channel closed (the client was dropped) so the caller can exit its loop.
pub async fn await_foreground(rx: &mut tokio::sync::watch::Receiver<bool>) -> bool {
    loop {
        if *rx.borrow_and_update() {
            return true;
        }
        if rx.changed().await.is_err() {
            return false;
        }
    }
}

/// Whether the chatter server can be reached right now.
///
/// This exists because none of the three things that look like they answer that
/// question actually do:
///
/// - [`ConnectionState`] is a *session* notion. In practice it is written once,
///   by `ChatterClient::connect` during boot, and never revised.
/// - `GrpcClientPool::is_connected` only reports that a `Channel` object
///   exists, and `GrpcClientPool::channel` manufactures a lazy one on demand —
///   so it answers `true` against a server that has been down for hours.
/// - A configured server URL says nothing about whether anything is listening.
///
/// So this is the answer to "would an RPC get through right now", and it is
/// only ever written from something that actually happened on the wire: a
/// completed dial, a completed RPC, or a transport error. Nothing here is
/// inferred, and [`Unknown`](Self::Unknown) is a real state rather than an
/// optimistic default — before the first dial the honest answer is that we do
/// not know, and a UI that says "offline" then would be guessing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ServerStatus {
    /// Nothing has been attempted yet. No claim either way.
    #[default]
    Unknown,
    /// A dial or an RPC completed against the server.
    Reachable,
    /// A dial or an RPC failed at the transport level.
    Unreachable,
}

impl ServerStatus {
    /// Stable discriminant (0=Unknown, 1=Reachable, 2=Unreachable), for the
    /// any FFI that has to carry this across a boundary.
    ///
    /// The UI does **not** read this: reachability reaches Slint through
    /// [`crate::state::OfflineState`], which the connection banner already
    /// subscribes to, rather than through a second parallel channel that could
    /// disagree with the first.
    pub fn as_i32(self) -> i32 {
        match self {
            Self::Unknown => 0,
            Self::Reachable => 1,
            Self::Unreachable => 2,
        }
    }
}

// Serialize status, monotonic timestamp and revision as one publication. The
// revision also lets a synthetic dial discard its result after newer RPC evidence.
#[derive(Default)]
struct Evidence {
    at_ms: u64,
    revision: u64,
}

static EVIDENCE: std::sync::Mutex<Evidence> = std::sync::Mutex::new(Evidence {
    at_ms: 0,
    revision: 0,
});

/// Monotonic origin for the serialized evidence timestamp.
///
/// An `Instant` rather than the wall clock on purpose: a phone that picks up NTP
/// (or crosses a timezone, or has its clock set by hand) can move `SystemTime`
/// backwards, and a negative age saturates to zero — which would read as "the
/// wire reported just now" and silently park the reachability monitor. This app
/// is meant to work on devices that have been off the network for a while, so
/// that is not a hypothetical.
static EPOCH: std::sync::LazyLock<std::time::Instant> =
    std::sync::LazyLock::new(std::time::Instant::now);

fn now_ms() -> u64 {
    // +1 so the very first write is never mistaken for the "never written"
    // sentinel of zero.
    EPOCH.elapsed().as_millis() as u64 + 1
}

/// Record what the wire just said about the server.
///
/// Call this only from a real outcome — a finished dial, a finished RPC, a
/// transport error. Calling it from a guess is the one thing that would make
/// the indicator it feeds dishonest.
pub fn set_server_status(status: ServerStatus) {
    let mut evidence = EVIDENCE.lock().unwrap();
    record_evidence(&mut evidence, server_status_watch(), status);
}

fn record_evidence(
    evidence: &mut Evidence,
    tx: &tokio::sync::watch::Sender<ServerStatus>,
    status: ServerStatus,
) {
    evidence.at_ms = now_ms().max(evidence.at_ms);
    evidence.revision += 1;
    // `send_if_modified` wakes only when the value changes, so a successful RPC
    // that reports Reachable again does not poke waiters. Subscribers see
    // the offline→online edge the banner already shows via OfflineState.
    publish_server_status(tx, status);
}

fn publish_server_status(tx: &tokio::sync::watch::Sender<ServerStatus>, status: ServerStatus) {
    tx.send_if_modified(|current| {
        if *current == status {
            return false;
        }
        *current = status;
        true
    });
}

// One retained authoritative value; a separate atomic status and watch could
// disagree when concurrent RPCs publish different outcomes.
fn server_status_watch() -> &'static tokio::sync::watch::Sender<ServerStatus> {
    static TX: std::sync::LazyLock<tokio::sync::watch::Sender<ServerStatus>> =
        std::sync::LazyLock::new(|| {
            let (tx, _rx) = tokio::sync::watch::channel(ServerStatus::Unknown);
            tx
        });
    &TX
}

/// Capture the evidence revision before starting a synthetic dial.
pub(crate) fn server_evidence_revision() -> u64 {
    EVIDENCE.lock().unwrap().revision
}

/// Publish a dial only if no newer wire evidence has arrived in the meantime.
pub(crate) fn set_server_status_if_current(status: ServerStatus, revision: u64) -> bool {
    let mut evidence = EVIDENCE.lock().unwrap();
    record_if_current(&mut evidence, server_status_watch(), status, revision)
}

fn record_if_current(
    evidence: &mut Evidence,
    tx: &tokio::sync::watch::Sender<ServerStatus>,
    status: ServerStatus,
    revision: u64,
) -> bool {
    if evidence.revision != revision {
        return false;
    }
    record_evidence(evidence, tx, status);
    true
}

/// Subscribe to server reachability.
///
/// The latest status is available from the receiver immediately. `changed`
/// resolves only when [`set_server_status`] stores a different value — the
/// same transition `server_monitor` mirrors into `OfflineState` for the
/// "No server" banner. The sync engine retries a failed cycle on the
/// offline→online edge of this watch.
pub fn subscribe_server_status() -> tokio::sync::watch::Receiver<ServerStatus> {
    server_status_watch().subscribe()
}

/// Read the process-global server reachability.
pub fn server_status() -> ServerStatus {
    *server_status_watch().borrow()
}

/// How long ago the wire last told us anything, or `None` if it never has.
pub fn server_status_age() -> Option<std::time::Duration> {
    let at = EVIDENCE.lock().unwrap().at_ms;
    if at == 0 {
        return None;
    }
    Some(std::time::Duration::from_millis(
        now_ms().saturating_sub(at),
    ))
}

/// Connection state for the state machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ConnectionState {
    /// Not connected to server.
    #[default]
    Disconnected,
    /// Attempting initial connection.
    Connecting,
    /// Successfully connected and authenticated.
    Connected,
    /// Reconnecting after connection loss.
    Reconnecting {
        /// Current retry attempt number (1-indexed).
        attempt: u32,
    },
    /// Terminal failure state requiring user action.
    Failed {
        /// Reason for the failure.
        reason: FailReason,
    },
}

impl ConnectionState {
    /// Check if this is a terminal failure state.
    pub fn is_failed(&self) -> bool {
        matches!(self, Self::Failed { .. })
    }

    /// Check if currently connected.
    pub fn is_connected(&self) -> bool {
        matches!(self, Self::Connected)
    }

    /// Check if in any connecting state (initial or reconnecting).
    pub fn is_connecting(&self) -> bool {
        matches!(self, Self::Connecting | Self::Reconnecting { .. })
    }

    /// The server is settled-unreachable, as opposed to still being tried.
    ///
    /// Not simply `!is_connected()`: [`Connecting`](Self::Connecting) is neither
    /// reachable nor unreachable, and the difference decides whether the UI is
    /// entitled to tell somebody that signing in cannot work. Announcing that
    /// mid-attempt would be wrong more often than right.
    ///
    /// [`Reconnecting`](Self::Reconnecting) counts as unreachable: a reconnect is
    /// only ever entered *after* a connection was lost, so the server is known to
    /// be gone right now even though we are trying again.
    pub fn is_unreachable(&self) -> bool {
        matches!(
            self,
            Self::Disconnected | Self::Failed { .. } | Self::Reconnecting { .. }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_rpc_evidence_rejects_old_probe_even_without_status_edge() {
        let (tx, _rx) = tokio::sync::watch::channel(ServerStatus::Reachable);
        let mut evidence = Evidence::default();
        let probe_revision = evidence.revision;
        record_evidence(&mut evidence, &tx, ServerStatus::Reachable);
        let at = evidence.at_ms;
        assert!(!record_if_current(
            &mut evidence,
            &tx,
            ServerStatus::Unreachable,
            probe_revision
        ));
        assert_eq!(*tx.borrow(), ServerStatus::Reachable);
        assert_eq!(evidence.at_ms, at);
        let current = evidence.revision;
        assert!(record_if_current(
            &mut evidence,
            &tx,
            ServerStatus::Unreachable,
            current
        ));
        assert!(evidence.at_ms >= at);
        assert_eq!(*tx.borrow(), ServerStatus::Unreachable);
    }

    #[test]
    fn concurrent_publications_keep_monotonic_evidence_and_matching_status() {
        let (tx, _rx) = tokio::sync::watch::channel(ServerStatus::Unknown);
        let evidence = std::sync::Mutex::new(Evidence::default());
        std::thread::scope(|scope| {
            for status in [ServerStatus::Reachable, ServerStatus::Unreachable] {
                let evidence = &evidence;
                let tx = &tx;
                scope.spawn(move || {
                    for _ in 0..100 {
                        let mut current = evidence.lock().unwrap();
                        let before = current.at_ms;
                        record_evidence(&mut current, tx, status);
                        assert!(current.at_ms >= before);
                        assert_eq!(*tx.borrow(), status);
                    }
                });
            }
        });
        assert_eq!(evidence.lock().unwrap().revision, 200);
    }

    #[tokio::test]
    async fn status_retained_without_receivers_and_duplicate_does_not_wake() {
        let (tx, rx) = tokio::sync::watch::channel(ServerStatus::Unknown);
        drop(rx);
        publish_server_status(&tx, ServerStatus::Unreachable);
        let mut rx = tx.subscribe();
        assert_eq!(*rx.borrow_and_update(), ServerStatus::Unreachable);
        publish_server_status(&tx, ServerStatus::Reachable);
        rx.changed().await.unwrap();
        assert_eq!(*rx.borrow_and_update(), ServerStatus::Reachable);
        publish_server_status(&tx, ServerStatus::Reachable);
        assert!(!rx.has_changed().unwrap());
    }

    #[test]
    fn test_state_helper_methods() {
        assert!(ConnectionState::Connected.is_connected());
        assert!(!ConnectionState::Disconnected.is_connected());

        assert!(ConnectionState::Connecting.is_connecting());
        assert!(ConnectionState::Reconnecting { attempt: 1 }.is_connecting());
        assert!(!ConnectionState::Connected.is_connecting());

        assert!(
            ConnectionState::Failed {
                reason: FailReason::AuthExpired
            }
            .is_failed()
        );
        assert!(!ConnectionState::Connected.is_failed());
    }
}

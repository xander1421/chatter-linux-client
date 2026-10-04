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
    /// atomic below and for any FFI that has to carry this across a boundary.
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

    fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Reachable,
            2 => Self::Unreachable,
            _ => Self::Unknown,
        }
    }
}

/// Process-global server reachability, for the same reason `TOR_STATUS` is one:
/// the layer that learns the answer is [`crate::network::grpc::GrpcClientPool`],
/// which holds no `OfflineState` handle and would have to have one threaded
/// through every client factory to report upwards.
static SERVER_STATUS: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// Millis since [`EPOCH`] at which [`SERVER_STATUS`] was last written. Zero
/// means never. Read by the reachability monitor so a client whose own RPCs are
/// already reporting does not also make synthetic dials.
static SERVER_STATUS_AT_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Monotonic origin for [`SERVER_STATUS_AT_MS`].
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
    SERVER_STATUS.store(status.as_i32() as u8, std::sync::atomic::Ordering::Release);
    SERVER_STATUS_AT_MS.store(now_ms(), std::sync::atomic::Ordering::Release);
    // `watch::send` wakes only when the value changes, so a successful RPC
    // that reports Reachable again does not poke waiters. Subscribers see
    // the offline→online edge the banner already shows via OfflineState.
    let _ = server_status_watch().send(status);
}

fn server_status_watch() -> &'static tokio::sync::watch::Sender<ServerStatus> {
    static TX: std::sync::LazyLock<tokio::sync::watch::Sender<ServerStatus>> =
        std::sync::LazyLock::new(|| {
            let (tx, _rx) = tokio::sync::watch::channel(ServerStatus::Unknown);
            tx
        });
    &TX
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
    ServerStatus::from_u8(SERVER_STATUS.load(std::sync::atomic::Ordering::Acquire))
}

/// How long ago the wire last told us anything, or `None` if it never has.
pub fn server_status_age() -> Option<std::time::Duration> {
    let at = SERVER_STATUS_AT_MS.load(std::sync::atomic::Ordering::Acquire);
    if at == 0 {
        return None;
    }
    Some(std::time::Duration::from_millis(now_ms().saturating_sub(at)))
}

/// Connection state for the state machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[derive(Default)]
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
    fn test_state_helper_methods() {
        assert!(ConnectionState::Connected.is_connected());
        assert!(!ConnectionState::Disconnected.is_connected());

        assert!(ConnectionState::Connecting.is_connecting());
        assert!(ConnectionState::Reconnecting { attempt: 1 }.is_connecting());
        assert!(!ConnectionState::Connected.is_connecting());

        assert!(ConnectionState::Failed {
            reason: FailReason::AuthExpired
        }
        .is_failed());
        assert!(!ConnectionState::Connected.is_failed());
    }
}

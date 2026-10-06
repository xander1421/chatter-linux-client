//! Server reachability monitor.
//!
//! One background task whose whole job is keeping the answer to "is the chatter
//! server reachable" true, and mirroring it into [`OfflineState`] so the UI can
//! see it through the store it already subscribes to.
//!
//! # Why this has to exist
//!
//! Before it, reachability was written exactly twice — both times inside
//! `ChatterClient::connect` during boot — and then never revised. A client that
//! booted while the server was up said "connected" for the rest of the process,
//! and a client that booted while the server was down said "disconnected" even
//! after the server came back. Neither is a state; both are a stale reading of
//! one moment, and an indicator wired to that would be decoration.
//!
//! # What it will and will not claim
//!
//! It reports reachability, and only reachability: a TCP + HTTP/2 handshake the
//! server answered. That is deliberately weaker than "logged in", "authorised",
//! or "your messages are going through" — a reachable server can still reject
//! every RPC — and the wording downstream must not upgrade it.
//!
//! # Why it mostly does nothing
//!
//! Real RPCs are better evidence than a synthetic dial, and
//! the shared `GrpcTransport` service records response and transport outcomes.
//! The task only dials when nothing on the wire has reported within
//! [`PROBE_INTERVAL`]. Signed-out startup defers contact; after an explicit
//! server action, idle Direct clients can recover using a synthetic dial.

use std::sync::Arc;
use std::time::Duration;

use crate::network::connection::{ConnectionState, ServerStatus, server_status, server_status_age};
use crate::network::grpc::GrpcClientPool;
use crate::state::OfflineState;

/// How stale the wire's last word may get before the monitor dials itself.
///
/// A compromise, and worth naming as one: shorter means the indicator notices a
/// server coming back sooner, longer means fewer radio wakeups on a phone whose
/// whole reason for running this app right now may be that there is no
/// infrastructure left to talk to. Anything the user actively tries — register,
/// log in, send — reports immediately through `GrpcTransport` regardless, so
/// this interval only governs the idle case.
const PROBE_INTERVAL: Duration = Duration::from_secs(30);

/// How often the task wakes to check whether a probe is due.
const TICK: Duration = Duration::from_secs(5);

/// Run the reachability monitor.
///
/// Started by `ChatterClient::start_server_monitor`, which is called
/// after restored signed-in startup or the first explicit server action.
/// Signed-out startup defers contact and does not start this task. The handle
/// is registered with the client's task list and aborted by
/// `ChatterClient::shutdown`, which is why there is no exit condition here.
pub async fn run(pool: Arc<GrpcClientPool>, offline_state: OfflineState) {
    // Mirror whatever is already known before the first tick, so a UI that
    // subscribes at startup is not left reading a default.
    let mut status_rx = crate::network::connection::subscribe_server_status();
    mirror(&offline_state);
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    tick.tick().await;

    loop {
        tokio::select! {
            result = status_rx.changed() => {
                if result.is_err() { return; }
                mirror(&offline_state);
                continue;
            }
            _ = tick.tick() => {}
        }

        let stale = server_status_age().is_none_or(|age| age >= PROBE_INTERVAL);
        if stale && probe_is_worth_it(&pool) {
            // The result is recorded by `probe` itself; the error is only worth
            // a debug line, because "the server is down" is an ordinary and
            // expected condition for this app rather than a fault.
            if let Err(error) = pool.probe().await {
                tracing::debug!(%error, "server monitor: probe failed");
            }
        }

        mirror(&offline_state);
    }
}

/// Whether a synthetic dial is a reasonable thing to spend right now.
///
/// It is not, under onion transport: every probe would build a fresh Tor circuit,
/// and doing that twice a minute forever costs far more than the indicator is
/// worth. Anon mode is also the case that needs it least — it implies a signed-in
/// session, whose own RPCs report reachability through `GrpcTransport` for
/// free. So under Tor this reports whatever the real traffic last said and stops
/// there, which is a staler answer but never a fabricated one.
fn probe_is_worth_it(pool: &GrpcClientPool) -> bool {
    use crate::network::grpc::client_pool::TransportMode;
    pool.transport_mode() == TransportMode::Direct
}

/// Publish the wire's answer into the store the UI subscribes to.
///
/// `Unknown` deliberately does not write anything: the store's own default is
/// `Disconnected`, and turning "we have not looked yet" into "offline" would be
/// the one guess this whole module exists to avoid.
fn mirror(offline_state: &OfflineState) {
    let next = match server_status() {
        ServerStatus::Reachable => ConnectionState::Connected,
        ServerStatus::Unreachable => ConnectionState::Disconnected,
        ServerStatus::Unknown => return,
    };
    // `set_connection_state` broadcasts on every call, so only write on a real
    // change — otherwise the UI thread would be woken every tick forever.
    if offline_state.connection_state() != next {
        offline_state.set_connection_state(next);
    }
}

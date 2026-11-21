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
//! It reports reachability: a handshake or a response from the server. That is deliberately weaker than "logged in", "authorised",
//! or "your messages are going through" — a reachable server can still reject
//! every RPC — and the wording downstream must not upgrade it.
//!
//! # Why it mostly does nothing
//!
//! Real RPCs are better evidence than a synthetic dial, and the shared
//! `GrpcTransport` records their outcomes. In Direct mode the monitor probes
//! to confirm a suspected outage or after [`PROBE_INTERVAL`] of wire silence.
//! Signed-out startup defers contact until an explicit server action.

use std::sync::Arc;
use std::time::Duration;

use crate::network::connection::{
    ConnectionState, ServerStatus, monitor_evidence, server_status_age, subscribe_server_status,
};
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

/// Give a fresh handshake or a real RPC time to disprove a transient failure,
/// while still showing a real outage promptly (including on onion).
const OUTAGE_CONFIRMATION: Duration = Duration::from_secs(4);

/// Run the reachability monitor until the client aborts its task.
pub async fn run(pool: Arc<GrpcClientPool>, offline_state: OfflineState) {
    run_with_timing(
        pool,
        offline_state,
        PROBE_INTERVAL,
        TICK,
        OUTAGE_CONFIRMATION,
    )
    .await;
}

pub(crate) async fn run_with_timing(
    pool: Arc<GrpcClientPool>,
    offline_state: OfflineState,
    probe_interval: Duration,
    tick_interval: Duration,
    confirmation: Duration,
) {
    let mut status_rx = subscribe_server_status();
    let mut tick = tokio::time::interval(tick_interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    tick.tick().await;

    let pool = &*pool;
    let probe = |confirm| {
        Box::pin(async move {
            if confirm {
                pool.probe_for_confirmation().await
            } else {
                pool.probe().await
            }
        })
    };
    let mut in_flight = None;
    let mut deadline = None;
    let mut last_reachable = 0;

    loop {
        // Read the evidence together: a watch can coalesce a successful RPC
        // followed immediately by another failure into an Unreachable reading.
        let (status, reachable_revision) = monitor_evidence();
        match status {
            ServerStatus::Reachable => {
                if deadline.take().is_some() {
                    in_flight = None;
                }
                publish(&offline_state, ConnectionState::Connected);
            }
            ServerStatus::Unreachable
                if offline_state.connection_state() == ConnectionState::Connected =>
            {
                if deadline.is_none() || reachable_revision != last_reachable {
                    deadline = Some(tokio::time::Instant::now() + confirmation);
                    in_flight = probe_is_worth_it(pool).then(|| probe(true));
                }
                if deadline.is_some_and(|at| tokio::time::Instant::now() >= at) {
                    deadline = None;
                    in_flight = None;
                    publish(&offline_state, ConnectionState::Disconnected);
                }
            }
            ServerStatus::Unreachable => publish(&offline_state, ConnectionState::Disconnected),
            ServerStatus::Unknown => {
                if deadline.take().is_some() {
                    in_flight = None;
                }
            }
        }
        last_reachable = reachable_revision;

        tokio::select! {
            result = status_rx.changed() => {
                if result.is_err() { return; }
            }
            () = async { tokio::time::sleep_until(deadline.unwrap()).await }, if deadline.is_some() => {}
            result = async { in_flight.as_mut().unwrap().await }, if in_flight.is_some() => {
                in_flight = None;
                if let Err(error) = result {
                    tracing::debug!(%error, "server monitor: probe failed");
                }
            }
            _ = tick.tick() => {
                let stale = server_status_age().is_none_or(|age| age >= probe_interval);
                if deadline.is_none() && in_flight.is_none() && stale && probe_is_worth_it(pool) {
                    in_flight = Some(probe(false));
                }
            }
        }
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

/// Mirror the startup dial synchronously so sign-in can read its result as
/// soon as the attempt returns. An already-connected store belongs to the
/// monitor's confirmation path when the wire reports a failure.
pub(crate) fn mirror_initial(offline_state: &OfflineState, first_contact: bool) {
    let status = subscribe_server_status();
    // Hold the watch read while publishing so newer wire evidence cannot be
    // published between reading this result and updating OfflineState.
    match *status.borrow() {
        ServerStatus::Reachable => publish(offline_state, ConnectionState::Connected),
        ServerStatus::Unreachable if first_contact => {
            // A reused client may still display Connected. Always publish
            // this first-contact result, even if already Disconnected.
            offline_state.set_connection_state(ConnectionState::Disconnected);
        }
        ServerStatus::Unreachable
            if offline_state.connection_state() != ConnectionState::Connected =>
        {
            publish(offline_state, ConnectionState::Disconnected);
        }
        _ => {}
    }
}

fn publish(offline_state: &OfflineState, next: ConnectionState) {
    // Store writes broadcast even if the value hasn't changed.
    if offline_state.connection_state() != next {
        offline_state.set_connection_state(next);
    }
}

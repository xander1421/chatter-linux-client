//! Bridge: call orchestrator events → the local `call_history` table.
//!
//! The orchestrator is the one place that sees *every* call, on every
//! transport, from ring to hangup — DM, anon DM, anon group and group
//! channel all funnel through the same `CallOrchestratorEvent` stream. So
//! that stream, not the per-transport signalling paths, is where history is
//! written: one recorder covers every conversation type that can host a
//! call, and a new transport gets history for free the moment it emits
//! orchestrator events.
//!
//! Outgoing calls are the one thing the orchestrator does not announce
//! (nothing calls `orch_register_outgoing`), so `ChatterClient::start_call`
//! writes the opening row for those itself. Both sides converge on the same
//! `id` — the orchestrator's `CallHandle` string — so the terminal events
//! below finish either row.
//
// ponytail: a *DM* call that ends before it connects (peer declines, or we
// cancel while it rings) emits no terminal event — the worker derives its
// handle from `active_call`, which is still empty at that point. The row is
// written and readable, but it stays open and reads "No answer" rather than
// "Declined"/"Cancelled". (Anon calls are fine: they go through
// `orch_mark_ended`, which now forwards untracked terminals.) The fix is to
// register outgoing calls with the orchestrator — `orch_register_outgoing`
// exists and has no callers — which also unsticks the "Calling…" view on the
// same path; do that when the outgoing-call lifecycle is next touched.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::broadcast::error::RecvError;

use crate::domain::call::{CallDirection, CallHistoryEntry, CallResult};
use crate::services::call_worker::{
    CallEndReason, CallHandle, CallOrchestratorEvent, CallWorkerHandle,
};
use crate::storage::repositories::{CallHistoryRepository, UnifiedConversationRepository};
use crate::storage::Database;

/// Spawn the call-history recorder. Exits when the orchestrator's broadcast
/// channel closes.
///
/// Its own thread, not a tokio task, for the same reason `RouterHandle` and
/// the call worker itself have one: every line of the body is a synchronous
/// SQLite call, so there is no async work here to interleave with anything.
/// A tokio task would have to hand each individual write to `spawn_blocking`
/// — the same blocking, with an await point and a closure around every
/// statement. `blocking_recv` is exactly the blocking take this loop wants.
pub fn spawn_call_history_recorder(call: &CallWorkerHandle, database: Arc<Database>) {
    let mut rx = call.subscribe_call_events();
    let history = CallHistoryRepository::new(database.clone());
    let conversations = UnifiedConversationRepository::new(database);

    std::thread::Builder::new()
        .name("call-history".into())
        .spawn(move || {
            // Calls that reached the connected state, and when. Terminal
            // events carry only a handle and a reason, so this is what
            // separates "hung up after talking" from "nobody answered".
            let mut connected: HashMap<CallHandle, chrono::DateTime<chrono::Utc>> = HashMap::new();

            loop {
                match rx.blocking_recv() {
                    // A too-late offer never rings but writes the same row:
                    // missed, which is what a presented call starts as too.
                    Ok(CallOrchestratorEvent::IncomingPresented {
                        call,
                        kind,
                        peer_name,
                        is_video,
                        conversation_id,
                        ..
                    })
                    | Ok(CallOrchestratorEvent::IncomingMissed {
                        call,
                        kind,
                        peer_name,
                        is_video,
                        conversation_id,
                    }) => {
                        let conversation_type = kind.conversation_type();
                        // The conversation's own name beats the caller's
                        // self-reported one — it is what the sidebar shows,
                        // and for a group call the caller's name is not the
                        // subject of the row at all.
                        let name = conversations
                            .get_conversation(&conversation_id, &conversation_type)
                            .ok()
                            .flatten()
                            .map(|c| c.name)
                            .filter(|n| !n.is_empty())
                            .unwrap_or(peer_name);
                        let entry = CallHistoryEntry {
                            id: call.as_str().to_string(),
                            conversation_id,
                            conversation_type,
                            peer_name: name,
                            started_at: chrono::Utc::now(),
                            ended_at: None,
                            direction: CallDirection::Incoming,
                            // Pessimistic on purpose: a ringing call that is
                            // never resolved (app killed mid-ring) is a
                            // missed call, and that is what the row says.
                            result: CallResult::Missed,
                            duration_secs: None,
                            has_video: is_video,
                        };
                        if let Err(e) = history.insert_entry(&entry) {
                            tracing::warn!(error = %e, "call history: incoming insert failed");
                        }
                    }

                    Ok(CallOrchestratorEvent::ActiveConnected { call, .. })
                    | Ok(CallOrchestratorEvent::AnonCallConnected { call, .. }) => {
                        connected.insert(call, chrono::Utc::now());
                    }

                    Ok(CallOrchestratorEvent::ActiveEnded { call, reason })
                    | Ok(CallOrchestratorEvent::IncomingWithdrawn { call, reason }) => {
                        finish(&history, &call, connected.remove(&call), &reason);
                    }

                    Ok(_) => {}
                    Err(RecvError::Lagged(n)) => {
                        tracing::warn!(dropped_count = n, "call history recorder lagged");
                    }
                    Err(RecvError::Closed) => {
                        tracing::debug!("call history recorder: channel closed, exiting");
                        break;
                    }
                }
            }
        })
        .expect("Failed to spawn call history thread");
}

/// Close out a history row. `connected_at` is `None` for a call that never
/// reached the connected state, which is what turns a hangup into a missed
/// call rather than a zero-second completed one.
fn finish(
    history: &CallHistoryRepository,
    call: &CallHandle,
    connected_at: Option<chrono::DateTime<chrono::Utc>>,
    reason: &CallEndReason,
) {
    let Some(connected_at) = connected_at else {
        // Never connected. `Declined` is an explicit "no" from either side;
        // everything else (caller gave up, ring timed out, setup blew up)
        // reads as missed. `mark_ended` refuses to overwrite a terminal
        // result, so stamp it directly.
        let result = match reason {
            CallEndReason::Declined => CallResult::Declined,
            // Signal files it as an answered incoming call, not a missed one.
            CallEndReason::AnsweredElsewhere => CallResult::Completed,
            CallEndReason::Error(_) => CallResult::Failed,
            _ => CallResult::Missed,
        };
        if let Err(e) = history.mark_unconnected(call.as_str(), result) {
            tracing::debug!(error = %e, call = %call.as_str(), "call history: no row to close");
        }
        return;
    };

    let duration = (chrono::Utc::now() - connected_at).num_seconds().max(0);
    if let Err(e) = history.mark_ended(call.as_str(), duration) {
        tracing::debug!(error = %e, call = %call.as_str(), "call history: no row to end");
    }
}

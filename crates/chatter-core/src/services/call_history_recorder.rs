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
//! Outgoing calls are not all announced — only a DM call is registered
//! while it rings (`orch_register_outgoing`, `OutgoingStarted`), and that
//! event carries no conversation — so `ChatterClient::start_call` writes the
//! opening row for every outgoing call itself. Both sides converge on the
//! same `id` — the orchestrator's `CallHandle` string — so the terminal
//! events below finish either row. A DM call cancelled while it rings, or
//! rung out (`NoAnswer`), ends with the leave's `ActiveEnded` like any other.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::broadcast::error::RecvError;

use crate::domain::call::{CallDirection, CallHistoryEntry, CallResult};
use crate::domain::ids::{ChannelId, MessageId, UserId};
use crate::domain::message::{CallMessage, CallMessageType, ConversationType, Message, MessageContent};
use crate::events::DomainEvent;
use crate::services::call_worker::{
    CallEndReason, CallHandle, CallOrchestratorEvent, CallWorkerHandle,
};
use crate::storage::repositories::{CallHistoryRepository, UnifiedConversationRepository};
use crate::services::message::MessageService;
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
///
/// It also files the Call bubble a finished 1:1 call leaves in the DM
/// (`file_dm_bubble`): the terminal events here are the one point every end
/// of a DM call passes — our hangup, the peer's, the peer leaving the room,
/// a decline, a miss — on both sides.
pub fn spawn_call_history_recorder(
    call: &CallWorkerHandle,
    database: Arc<Database>,
    messages: Arc<MessageService>,
) {
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
                let event = match rx.blocking_recv() {
                    Ok(event) => event,
                    Err(RecvError::Lagged(n)) => {
                        tracing::warn!(dropped_count = n, "call history recorder lagged");
                        continue;
                    }
                    Err(RecvError::Closed) => {
                        tracing::debug!("call history recorder: channel closed, exiting");
                        break;
                    }
                };
                // A too-late offer is over as it arrives: nothing else will
                // close it, so its bubble is filed with the row.
                let missed_on_arrival = matches!(event, CallOrchestratorEvent::IncomingMissed { .. });
                match event {
                    // A too-late offer never rings but writes the same row:
                    // missed, which is what a presented call starts as too.
                    CallOrchestratorEvent::IncomingPresented {
                        call,
                        kind,
                        peer_name,
                        is_video,
                        conversation_id,
                        ..
                    }
                    | CallOrchestratorEvent::IncomingMissed {
                        call,
                        kind,
                        peer_name,
                        is_video,
                        conversation_id,
                    } => {
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
                        if missed_on_arrival {
                            file_dm_bubble(&history, &messages, &call);
                        }
                    }

                    CallOrchestratorEvent::ActiveConnected { call, .. }
                    | CallOrchestratorEvent::AnonCallConnected { call, .. } => {
                        connected.insert(call, chrono::Utc::now());
                    }

                    CallOrchestratorEvent::ActiveEnded { call, reason }
                    | CallOrchestratorEvent::IncomingWithdrawn { call, reason } => {
                        finish(&history, &call, connected.remove(&call), &reason);
                        // Answered on another of our devices: that device has
                        // the call; here it is only a Calls-screen row.
                        if reason != CallEndReason::AnsweredElsewhere {
                            file_dm_bubble(&history, &messages, &call);
                        }
                    }

                    _ => {}
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

/// File the Call bubble for a finished call, when it was a 1:1 DM call, and
/// show it in the open chat. Built from the closed history row, so the bubble
/// and the Calls screen can never disagree.
fn file_dm_bubble(history: &CallHistoryRepository, messages: &MessageService, call: &CallHandle) {
    let entry = match history.get(call.as_str()) {
        Ok(Some(entry)) => entry,
        Ok(None) => return,
        Err(e) => {
            tracing::warn!(error = %e, call = %call.as_str(), "call bubble: history read failed");
            return;
        }
    };
    let Ok(me) = messages.current_user_id() else { return };
    let Some(bubble) = dm_call_bubble(&entry, me) else { return };
    // The id is the call's, so a second terminal event for the same call
    // files nothing and says nothing.
    match messages.store_message_local_once(&bubble) {
        Ok(true) => {
            messages.event_bus.emit_domain(DomainEvent::MessageReceived { message: bubble });
        }
        Ok(false) => {}
        Err(e) => tracing::warn!(error = %e, call = %call.as_str(), "call bubble not filed"),
    }
}

/// The Call bubble a closed history row leaves in the DM with the peer, or
/// `None` for a call that is not a 1:1 DM call (group and anon calls file
/// their own).
///
/// * Filed under `ChannelId::from(peer)` as `ServerDm` — the key and bucket
///   the DM's own messages use.
/// * `initiator_id` decides "Incoming" / "Outgoing" in the chat; only an
///   incoming call nobody answered reads as missed. Our own unanswered or
///   declined call is still an outgoing call, as in Signal.
/// * `sender_id` is the peer, as for the DM's failure markers: the chat and
///   sidebar route a `ServerDm` message by its sender. A Call record raises
///   no notification (`should_notify_message`) and is filed read.
fn dm_call_bubble(entry: &CallHistoryEntry, me: UserId) -> Option<Message> {
    if entry.conversation_type != ConversationType::ServerDm {
        return None;
    }
    let peer: UserId = entry.conversation_id.parse().ok()?;
    let incoming = entry.direction == CallDirection::Incoming;
    let call_type = if incoming && entry.result == CallResult::Missed {
        CallMessageType::Missed
    } else {
        CallMessageType::Ended
    };
    let now = chrono::Utc::now();
    let mut bubble = Message::new_text(
        ChannelId::from(peer),
        ConversationType::ServerDm,
        Some(peer),
        String::new(),
        crate::util::time::TimestampNanos::now().as_u64(),
    );
    if let Ok(id) = entry.id.parse::<uuid::Uuid>() {
        bubble.id = MessageId::from_uuid(id);
    }
    bubble.is_read = true;
    bubble.content = MessageContent::Call(CallMessage {
        call_id: entry.id.clone(),
        call_type,
        initiator_id: if incoming { peer } else { me },
        initiator_name: if incoming { entry.peer_name.clone() } else { String::new() },
        started_at: Some(entry.started_at),
        ended_at: Some(entry.ended_at.unwrap_or(now)),
        duration: entry
            .duration_secs
            .filter(|s| *s > 0)
            .map(chrono::Duration::seconds),
        participant_count: 2,
    });
    Some(bubble)
}

#[cfg(test)]
mod dm_call_bubble_tests {
    use super::*;

    fn row(peer: UserId, direction: CallDirection, result: CallResult, duration: Option<i64>) -> CallHistoryEntry {
        CallHistoryEntry {
            id: uuid::Uuid::new_v4().to_string(),
            conversation_id: peer.to_string(),
            conversation_type: ConversationType::ServerDm,
            peer_name: "Peer".into(),
            started_at: chrono::Utc::now(),
            ended_at: Some(chrono::Utc::now()),
            direction,
            result,
            duration_secs: duration,
            has_video: false,
        }
    }

    fn call(bubble: &Message) -> &CallMessage {
        match &bubble.content {
            MessageContent::Call(c) => c,
            other => panic!("not a call bubble: {other:?}"),
        }
    }

    #[test]
    fn both_sides_file_the_call_in_the_dm_with_the_other() {
        let (caller, callee) = (UserId::new(), UserId::new());

        // The caller's row names the callee; the callee's names the caller.
        let caller_row = row(callee, CallDirection::Outgoing, CallResult::Completed, Some(10));
        let mut callee_row = row(caller, CallDirection::Incoming, CallResult::Completed, Some(10));
        callee_row.id = caller_row.id.clone();

        let out = dm_call_bubble(&caller_row, caller).expect("caller bubble");
        let inc = dm_call_bubble(&callee_row, callee).expect("callee bubble");

        assert_eq!(out.conversation_id, ChannelId::from(callee));
        assert_eq!(inc.conversation_id, ChannelId::from(caller));
        for (b, me) in [(&out, caller), (&inc, callee)] {
            assert_eq!(b.conversation_type, ConversationType::ServerDm);
            let peer = if me == caller { callee } else { caller };
            assert_eq!(b.sender_id, Some(peer), "routed like the peer's messages");
            assert_eq!(b.id.to_string(), caller_row.id, "one bubble per call");
            assert_eq!(call(b).call_type, CallMessageType::Ended);
            assert_eq!(call(b).duration, Some(chrono::Duration::seconds(10)));
        }
        assert_eq!(call(&out).initiator_id, caller, "outgoing on the caller's side");
        assert_eq!(call(&inc).initiator_id, caller, "incoming on the callee's side");
    }

    #[test]
    fn only_an_unanswered_incoming_call_reads_as_missed() {
        let (me, peer) = (UserId::new(), UserId::new());
        let kind = |d, r| call(&dm_call_bubble(&row(peer, d, r, None), me).unwrap()).call_type.clone();
        assert_eq!(kind(CallDirection::Incoming, CallResult::Missed), CallMessageType::Missed);
        assert_eq!(kind(CallDirection::Incoming, CallResult::Declined), CallMessageType::Ended);
        // Our call nobody picked up is still an outgoing call.
        assert_eq!(kind(CallDirection::Outgoing, CallResult::Missed), CallMessageType::Ended);
        assert_eq!(kind(CallDirection::Outgoing, CallResult::Declined), CallMessageType::Ended);
    }

    #[test]
    fn group_and_anon_calls_are_not_dm_bubbles() {
        let (me, peer) = (UserId::new(), UserId::new());
        for ct in [ConversationType::Channel, ConversationType::AnonDm, ConversationType::AnonGroup] {
            let mut r = row(peer, CallDirection::Incoming, CallResult::Completed, Some(3));
            r.conversation_type = ct;
            assert!(dm_call_bubble(&r, me).is_none());
        }
    }
}

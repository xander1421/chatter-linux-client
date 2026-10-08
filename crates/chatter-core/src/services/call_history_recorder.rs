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
//! Outgoing calls are not all announced — only DM and anon calls are
//! registered while they ring (`orch_register_outgoing`, `OutgoingStarted`),
//! and that event carries no conversation — so `ChatterClient::start_call`
//! writes the opening row for every outgoing call itself. Both sides converge
//! on the same `id` — the orchestrator's `CallHandle` string — so the terminal
//! events below finish either row. A DM or anon call cancelled while it
//! rings, or rung out (`NoAnswer`), ends with an `ActiveEnded` like any other.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::broadcast::error::RecvError;

use crate::domain::call::{CallDirection, CallHistoryEntry, CallResult};
use crate::domain::ids::{ChannelId, MessageId, UserId};
use crate::domain::message::{
    CallMessage, CallMessageType, ConversationType, Message, MessageContent,
};
use crate::events::DomainEvent;
use crate::services::call_worker::{
    CallEndReason, CallHandle, CallOrchestratorEvent, CallWorkerHandle,
};
use crate::services::message::MessageService;
use crate::storage::Database;
use crate::storage::repositories::{CallHistoryRepository, UnifiedConversationRepository};

/// When this process began recording calls. An open history row (no
/// `ended_at`) that started earlier was left by an earlier run — killed
/// mid-call — and nothing will ever close it; one that started later may be a
/// live call. Calls-screen deletes keep the latter (`CallHistoryRepository::
/// delete_ids`). Unset (no recorder) leaves every open row protected.
pub(crate) static LIVE_SINCE: std::sync::OnceLock<chrono::DateTime<chrono::Utc>> =
    std::sync::OnceLock::new();

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
/// It also files the Call bubble a finished 1:1 call leaves in the DM or
/// anon DM (`file_dm_bubble`): the terminal events here are the one point
/// every end of such a call passes — our hangup, the peer's, the peer leaving
/// the room, a decline, a miss — on both sides.
pub fn spawn_call_history_recorder(
    call: &CallWorkerHandle,
    database: Arc<Database>,
    messages: Arc<MessageService>,
) {
    LIVE_SINCE.get_or_init(chrono::Utc::now);
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
                let missed_on_arrival =
                    matches!(event, CallOrchestratorEvent::IncomingMissed { .. });
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
                            announce_if_missed(&history, &messages, &call);
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
                        announce_if_missed(&history, &messages, &call);
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
        // reads as missed. Stamped directly: `mark_completed` is only for a
        // call that connected.
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
    if let Err(e) = history.mark_completed(call.as_str(), duration) {
        tracing::debug!(error = %e, call = %call.as_str(), "call history: no row to end");
    }
}

/// Tell the UI the Calls badge moved, when `call`'s row — already written, so
/// the badge read that follows sees it — is a missed incoming call. A ringing
/// row is not announced: it only counts once the call is over.
fn announce_if_missed(
    history: &CallHistoryRepository,
    messages: &MessageService,
    call: &CallHandle,
) {
    let Ok(Some(entry)) = history.get(call.as_str()) else {
        return;
    };
    if entry.direction == CallDirection::Incoming && entry.result == CallResult::Missed {
        messages
            .event_bus
            .emit_domain(DomainEvent::MissedCallsChanged);
    }
}

/// File the Call bubble for a finished call, when it was a 1:1 call, and
/// show it in the open chat. Built from the closed history row, so the bubble
/// and the Calls screen can never disagree.
pub(super) fn file_dm_bubble(
    history: &CallHistoryRepository,
    messages: &MessageService,
    call: &CallHandle,
) {
    let entry = match history.get(call.as_str()) {
        Ok(Some(entry)) => entry,
        Ok(None) => return,
        Err(e) => {
            tracing::warn!(error = %e, call = %call.as_str(), "call bubble: history read failed");
            return;
        }
    };
    let Ok(me) = messages.current_user_id() else {
        return;
    };
    let Some(bubble) = dm_call_bubble(&entry, me) else {
        return;
    };
    // The id is the call's, so a second terminal event for the same call
    // files nothing and says nothing.
    match messages.store_message_local_once(&bubble) {
        Ok(true) => {
            messages
                .event_bus
                .emit_domain(DomainEvent::MessageReceived { message: bubble });
        }
        Ok(false) => {}
        Err(e) => tracing::warn!(error = %e, call = %call.as_str(), "call bubble not filed"),
    }
}

/// The Call bubble a closed history row leaves in the 1:1 conversation with
/// the peer — a friend DM or an anon DM — or `None` for a group call (they
/// file their own).
///
/// * Filed under the conversation's `ChannelId` and type — the key and
///   bucket its own messages use: `ChannelId::from(peer)` for a friend DM,
///   the anon DM's id for an anon one.
/// * `initiator_id` decides "Incoming" / "Outgoing" in the chat; only an
///   incoming call nobody answered reads as missed. Our own unanswered call
///   is still an outgoing call, as in Signal. A declined call reads
///   "Declined" on both sides, as the Calls screen says.
/// * `sender_id` is the peer, as for the DM's failure markers: the chat and
///   sidebar route a `ServerDm` message by its sender. An anon peer has no
///   `UserId` (the orchestrator rings it as nil too) and the anon chat routes
///   by conversation, so there it is nil. A Call record raises no
///   notification (`should_notify_message`) and is filed read.
/// * The id is the call's: the handle's UUID (an anon handle is
///   `anon-<uuid>`), so every terminal event of one call names one bubble.
pub(super) fn dm_call_bubble(entry: &CallHistoryEntry, me: UserId) -> Option<Message> {
    let (channel, peer) = match entry.conversation_type {
        ConversationType::ServerDm => {
            let peer: UserId = entry.conversation_id.parse().ok()?;
            (ChannelId::from(peer), peer)
        }
        ConversationType::AnonDm => {
            let anon: crate::domain::anon_dm::AnonDmId = entry.conversation_id.parse().ok()?;
            (ChannelId::from(anon), UserId(uuid::Uuid::nil()))
        }
        _ => return None,
    };
    let incoming = entry.direction == CallDirection::Incoming;
    let call_type = match entry.result {
        CallResult::Missed if incoming => CallMessageType::Missed,
        CallResult::Declined => CallMessageType::Declined,
        _ => CallMessageType::Ended,
    };
    let now = chrono::Utc::now();
    let mut bubble = Message::new_text(
        channel,
        entry.conversation_type.clone(),
        Some(peer),
        String::new(),
        crate::util::time::TimestampNanos::now().as_u64(),
    );
    bubble.id = MessageId::from_uuid(entry.id.trim_start_matches("anon-").parse().ok()?);
    bubble.is_read = true;
    bubble.content = MessageContent::Call(CallMessage {
        call_id: entry.id.clone(),
        call_type,
        initiator_id: if incoming { peer } else { me },
        initiator_name: if incoming {
            entry.peer_name.clone()
        } else {
            String::new()
        },
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

    fn row(
        peer: UserId,
        direction: CallDirection,
        result: CallResult,
        duration: Option<i64>,
    ) -> CallHistoryEntry {
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
        let caller_row = row(
            callee,
            CallDirection::Outgoing,
            CallResult::Completed,
            Some(10),
        );
        let mut callee_row = row(
            caller,
            CallDirection::Incoming,
            CallResult::Completed,
            Some(10),
        );
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
        assert_eq!(
            call(&out).initiator_id,
            caller,
            "outgoing on the caller's side"
        );
        assert_eq!(
            call(&inc).initiator_id,
            caller,
            "incoming on the callee's side"
        );
    }

    #[test]
    fn only_an_incoming_call_reads_as_missed_but_both_sides_read_declined() {
        let (me, peer) = (UserId::new(), UserId::new());
        let kind = |d, r| {
            call(&dm_call_bubble(&row(peer, d, r, None), me).unwrap())
                .call_type
                .clone()
        };
        assert_eq!(
            kind(CallDirection::Incoming, CallResult::Missed),
            CallMessageType::Missed
        );
        assert_eq!(
            kind(CallDirection::Incoming, CallResult::Declined),
            CallMessageType::Declined
        );
        // Our call nobody picked up is still an outgoing call.
        assert_eq!(
            kind(CallDirection::Outgoing, CallResult::Missed),
            CallMessageType::Ended
        );
        // The peer said no: the bubble agrees with the Calls screen's "Declined".
        assert_eq!(
            kind(CallDirection::Outgoing, CallResult::Declined),
            CallMessageType::Declined
        );
    }

    #[test]
    fn an_anon_dm_call_is_filed_in_the_anon_conversation() {
        let me = UserId::new();
        let anon = crate::domain::anon_dm::AnonDmId::new();
        let room = uuid::Uuid::new_v4();
        let mut r = row(me, CallDirection::Incoming, CallResult::Missed, None);
        r.id = format!("anon-{room}");
        r.conversation_id = anon.to_string();
        r.conversation_type = ConversationType::AnonDm;

        let b = dm_call_bubble(&r, me).expect("anon bubble");
        assert_eq!(b.conversation_id, ChannelId::from(anon));
        assert_eq!(b.conversation_type, ConversationType::AnonDm);
        assert_eq!(b.id, MessageId::from_uuid(room), "one bubble per call");
        assert_eq!(call(&b).call_type, CallMessageType::Missed);
        assert_ne!(call(&b).initiator_id, me, "incoming on the callee's side");

        r.direction = CallDirection::Outgoing;
        assert_eq!(call(&dm_call_bubble(&r, me).unwrap()).initiator_id, me);
    }

    #[test]
    fn group_calls_are_not_dm_bubbles() {
        let (me, peer) = (UserId::new(), UserId::new());
        for ct in [ConversationType::Channel, ConversationType::AnonGroup] {
            let mut r = row(
                peer,
                CallDirection::Incoming,
                CallResult::Completed,
                Some(3),
            );
            r.conversation_type = ct;
            assert!(dm_call_bubble(&r, me).is_none());
        }
    }
}

#[cfg(test)]
mod incoming_expiry_history_tests {
    use super::*;
    use crate::services::call_worker::OrchestratedCallKind;

    #[test]
    fn incoming_expiry_no_answer_finishes_one_missed_history_row() {
        let directory = tempfile::tempdir().unwrap();
        let db = Arc::new(crate::storage::Database::open(&directory.path().join("expiry.db"), "fixture-passphrase").unwrap());
        db.run_migrations().unwrap();
        let history = CallHistoryRepository::new(db);
        for kind in [OrchestratedCallKind::DmStandard, OrchestratedCallKind::AnonDm, OrchestratedCallKind::AnonGroup] {
            let call = CallHandle::new(uuid::Uuid::new_v4().to_string());
            history.insert_entry(&CallHistoryEntry {
                id: call.as_str().into(), conversation_id: "fixture-peer".into(),
                conversation_type: kind.conversation_type(), peer_name: "Fixture".into(),
                started_at: chrono::Utc::now(), ended_at: None,
                direction: CallDirection::Incoming, result: CallResult::Missed,
                duration_secs: None, has_video: false,
            }).unwrap();
            finish(&history, &call, None, &CallEndReason::NoAnswer);
            let row = history.get(call.as_str()).unwrap().unwrap();
            assert_eq!(row.result, CallResult::Missed);
            assert!(row.ended_at.is_some());
            assert_eq!(row.duration_secs, None);
        }
    }
}

//! Liveness of the long-lived server subscription streams.
//!
//! The server ends a stream that writes nothing for 5 minutes
//! (`internal/server/stream_deadlines.go`; HTTP/2 and QUIC pings do not count),
//! so every subscription it serves carries a `chat.v1.StreamHeartbeat`: one as
//! soon as the subscription is established, then every 60 s ± 10%
//! (`internal/utils/streamheartbeat`). Before that existed, every quiet stream
//! was cut every ~5:05 and the client sat out its reconnect backoff with live
//! events delayed or lost (2026-10-03).
//!
//! On this side a heartbeat is never data: each consumer drops it before its
//! dispatch ([`IsHeartbeat`]), and a stream that delivers nothing, not even a
//! heartbeat, for [`STREAM_SILENCE_LIMIT`] is treated as dead and reconnected
//! ([`next_or_silent`]) instead of being trusted until the transport notices.

use std::time::Duration;

use futures::{Stream, StreamExt};

use crate::proto::chat_v1;

/// The server's nominal heartbeat interval (`streamheartbeat.Interval`).
pub const SERVER_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(60);

/// A subscription silent for longer than this is dead. More than two
/// heartbeat intervals, plus the server's ±10% jitter.
pub const STREAM_SILENCE_LIMIT: Duration = Duration::from_secs(150);

/// A stream that stayed up at least this long was healthy, so its loss is the
/// ordinary "server closed it" case and the reconnect starts from the shortest
/// backoff rather than wherever earlier failures had left it.
pub const HEALTHY_STREAM_AFTER: Duration = SERVER_HEARTBEAT_INTERVAL;

/// What waiting on a subscription produced.
#[derive(Debug)]
pub enum Next<T> {
    Item(T),
    /// The server ended the stream.
    Ended,
    /// Nothing at all for `limit`: the stream is dead even if the transport
    /// has not noticed yet.
    Silent,
}

/// Wait for the next item, giving up after `limit` of silence. Cancel-safe
/// (it only wraps `StreamExt::next`), so it can sit in a `select!`.
pub async fn next_or_silent<S: Stream + Unpin>(stream: &mut S, limit: Duration) -> Next<S::Item> {
    match tokio::time::timeout(limit, stream.next()).await {
        Ok(Some(item)) => Next::Item(item),
        Ok(None) => Next::Ended,
        Err(_) => Next::Silent,
    }
}

/// A subscription stream item that may be the server's heartbeat.
pub trait IsHeartbeat {
    fn is_heartbeat(&self) -> bool;
}

impl IsHeartbeat for chat_v1::MessageEvent {
    fn is_heartbeat(&self) -> bool {
        matches!(self.event, Some(chat_v1::message_event::Event::Heartbeat(_)))
    }
}

impl IsHeartbeat for chat_v1::StreamMessagesResponse {
    fn is_heartbeat(&self) -> bool {
        matches!(self.item, Some(chat_v1::stream_messages_response::Item::Heartbeat(_)))
    }
}

impl IsHeartbeat for chat_v1::ChannelEvent {
    fn is_heartbeat(&self) -> bool {
        matches!(self.event, Some(chat_v1::channel_event::Event::Heartbeat(_)))
    }
}

impl IsHeartbeat for chat_v1::UserEvent {
    fn is_heartbeat(&self) -> bool {
        matches!(self.event, Some(chat_v1::user_event::Event::Heartbeat(_)))
    }
}

impl IsHeartbeat for chat_v1::CallEvent {
    fn is_heartbeat(&self) -> bool {
        matches!(self.data, Some(chat_v1::call_event::Data::Heartbeat(_)))
    }
}

impl IsHeartbeat for chat_v1::StreamNotificationsResponse {
    fn is_heartbeat(&self) -> bool {
        matches!(self.item, Some(chat_v1::stream_notifications_response::Item::Heartbeat(_)))
    }
}

impl IsHeartbeat for chat_v1::StreamIntegrationEventsResponse {
    fn is_heartbeat(&self) -> bool {
        matches!(self.item, Some(chat_v1::stream_integration_events_response::Item::Heartbeat(_)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chat_v1::StreamHeartbeat as Hb;

    #[test]
    fn the_silence_limit_spans_more_than_two_jittered_intervals() {
        // The server's longest gap is interval + 10%.
        assert!(STREAM_SILENCE_LIMIT > SERVER_HEARTBEAT_INTERVAL * 2 + SERVER_HEARTBEAT_INTERVAL / 10 * 2);
        // And it must expire before the server's own 5-minute idle cut would.
        assert!(STREAM_SILENCE_LIMIT < Duration::from_secs(300));
    }

    #[test]
    fn every_subscription_type_recognises_its_heartbeat_and_only_it() {
        use chat_v1::*;
        assert!(MessageEvent { event: Some(message_event::Event::Heartbeat(Hb {})), ..Default::default() }.is_heartbeat());
        assert!(!MessageEvent { event: Some(message_event::Event::MessageDeletedId("x".into())), ..Default::default() }.is_heartbeat());
        assert!(!MessageEvent::default().is_heartbeat());

        assert!(StreamMessagesResponse { item: Some(stream_messages_response::Item::Heartbeat(Hb {})) }.is_heartbeat());
        assert!(!StreamMessagesResponse { item: Some(stream_messages_response::Item::Message(Message::default())) }.is_heartbeat());

        assert!(ChannelEvent { event: Some(channel_event::Event::Heartbeat(Hb {})), ..Default::default() }.is_heartbeat());
        assert!(!ChannelEvent { event: Some(channel_event::Event::ChannelDeleted("x".into())), ..Default::default() }.is_heartbeat());

        assert!(UserEvent { event: Some(user_event::Event::Heartbeat(Hb {})), ..Default::default() }.is_heartbeat());
        assert!(!UserEvent::default().is_heartbeat());

        assert!(CallEvent { data: Some(call_event::Data::Heartbeat(Hb {})), ..Default::default() }.is_heartbeat());
        assert!(!CallEvent::default().is_heartbeat());

        assert!(StreamNotificationsResponse { item: Some(stream_notifications_response::Item::Heartbeat(Hb {})) }.is_heartbeat());
        assert!(!StreamNotificationsResponse { item: Some(stream_notifications_response::Item::Notification(Notification::default())) }.is_heartbeat());

        assert!(StreamIntegrationEventsResponse { item: Some(stream_integration_events_response::Item::Heartbeat(Hb {})) }.is_heartbeat());
        assert!(!StreamIntegrationEventsResponse { item: Some(stream_integration_events_response::Item::Event(IntegrationEvent::default())) }.is_heartbeat());
    }

    #[tokio::test(start_paused = true)]
    async fn a_silent_stream_is_reported_dead_and_a_beating_one_is_not() {
        // Silent: nothing ever arrives.
        let mut silent = futures::stream::pending::<u8>();
        assert!(matches!(next_or_silent(&mut silent, STREAM_SILENCE_LIMIT).await, Next::Silent));

        // Beating: an item arrives one interval in.
        let mut beating = Box::pin(futures::stream::once(async {
            tokio::time::sleep(SERVER_HEARTBEAT_INTERVAL).await;
            7u8
        }));
        assert!(matches!(next_or_silent(&mut beating, STREAM_SILENCE_LIMIT).await, Next::Item(7)));
        assert!(matches!(next_or_silent(&mut beating, STREAM_SILENCE_LIMIT).await, Next::Ended));
    }
}

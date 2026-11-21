//! Release diagnostics use only closed, compile-time targets.

use std::error::Error;
use std::io;

#[derive(Clone, Copy)]
pub(super) enum Route {
    H2,
    #[cfg(feature = "quic")]
    H3,
    #[cfg(feature = "onion")]
    Tor,
}

#[derive(Clone, Copy)]
pub(super) enum Phase {
    Ready,
    Call,
    Body,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Class {
    Connect,
    Timeout,
    GoAway,
    Reset,
    Io,
    #[cfg(feature = "quic")]
    ConnectionClosed,
    #[cfg(feature = "quic")]
    Stream,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Family {
    MailboxChallenge,
    MailboxPoll,
    Message,
    StreamUserEvents,
    StreamChannel,
    Presence,
    Other,
}

impl Family {
    pub(super) fn from_path(path: &str) -> Self {
        match path {
            "/chat.v1.mailbox.EphemeralMailboxService/GetMailboxChallenge" => {
                Self::MailboxChallenge
            }
            "/chat.v1.mailbox.EphemeralMailboxService/PollEphemeralMailbox" => Self::MailboxPoll,
            "/chat.v1.UserService/StreamUserEvents" => Self::StreamUserEvents,
            "/chat.v1.ChannelService/StreamChannelEvents"
            | "/chat.v1.MessageService/StreamGlobalEvents"
            | "/chat.v1.MessageService/StreamMessageEvents"
            | "/chat.v1.MessageService/StreamMessages" => Self::StreamChannel,
            "/chat.v1.UserService/UpdatePresence" => Self::Presence,
            _ if path.starts_with("/chat.v1.MessageService/") => Self::Message,
            _ => Self::Other,
        }
    }
}

fn classify(route: Route, error: &(dyn Error + 'static)) -> Class {
    #[cfg(not(feature = "quic"))]
    let _ = route;

    let mut current = Some(error);
    let mut fallback = Class::Other;
    while let Some(cause) = current {
        #[cfg(feature = "quic")]
        if matches!(route, Route::H3) {
            if cause.is::<super::quic_lifecycle::ConnectFailure>()
                || cause.is::<quinn::ConnectError>()
            {
                return Class::Connect;
            }
            if let Some(error) = cause.downcast_ref::<quinn::ConnectionError>() {
                return if matches!(error, quinn::ConnectionError::TimedOut) {
                    Class::Timeout
                } else {
                    Class::ConnectionClosed
                };
            }
            if let Some(error) = cause.downcast_ref::<h3::error::ConnectionError>() {
                return classify_h3_connection(error);
            }
            if let Some(error) = cause.downcast_ref::<h3::error::StreamError>() {
                // h3's StreamError implements Error with no source(), including
                // for variants that carry a more specific connection error.
                match error {
                    h3::error::StreamError::ConnectionError(error) => {
                        return classify_h3_connection(error);
                    }
                    h3::error::StreamError::RemoteClosing => return Class::ConnectionClosed,
                    h3::error::StreamError::Undefined(error) => {
                        current = Some(error.as_ref());
                        fallback = Class::Stream;
                        continue;
                    }
                    _ => return Class::Stream,
                }
            }
        }
        if cause.is::<tonic::ConnectError>()
            || cause
                .downcast_ref::<hyper_util::client::legacy::Error>()
                .is_some_and(|error| error.is_connect())
        {
            return Class::Connect;
        }
        if let Some(error) = cause.downcast_ref::<h2::Error>() {
            if error.is_go_away() {
                return Class::GoAway;
            }
            if error.is_reset() {
                return Class::Reset;
            }
            if let Some(error) = error.get_io() {
                if error.kind() == io::ErrorKind::TimedOut {
                    return Class::Timeout;
                }
                fallback = Class::Io;
            }
        }
        if cause.is::<tonic::TimeoutExpired>()
            || cause
                .downcast_ref::<hyper::Error>()
                .is_some_and(|error| error.is_timeout())
        {
            return Class::Timeout;
        }
        if let Some(error) = cause.downcast_ref::<io::Error>() {
            if error.kind() == io::ErrorKind::TimedOut {
                return Class::Timeout;
            }
            fallback = Class::Io;
            // io::Error::source can skip the wrapped error itself.
            if let Some(inner) = error.get_ref() {
                current = Some(inner);
                continue;
            }
        }
        current = cause.source();
    }
    fallback
}

#[cfg(feature = "quic")]
fn classify_h3_connection(error: &h3::error::ConnectionError) -> Class {
    use h3::{error::ConnectionError, quic::ConnectionErrorIncoming};
    match error {
        ConnectionError::Timeout | ConnectionError::Remote(ConnectionErrorIncoming::Timeout) => {
            Class::Timeout
        }
        _ => Class::ConnectionClosed,
    }
}

pub(super) fn failure(route: Route, phase: Phase, family: Family, error: &(dyn Error + 'static)) {
    macro_rules! log_failure {
        ($route:literal, $class:literal, $phase:literal, $family:literal) => {
            tracing::event!(
                target: concat!("network_transport::fail_", $route, "_", $class, "_", $phase, "__", $family),
                tracing::Level::INFO,
                {}
            )
        };
    }
    macro_rules! rpc {
        ($route:literal, $class:literal, $phase:literal) => {
            match family {
                Family::MailboxChallenge => {
                    log_failure!($route, $class, $phase, "mailbox_challenge")
                }
                Family::MailboxPoll => log_failure!($route, $class, $phase, "mailbox_poll"),
                Family::Message => log_failure!($route, $class, $phase, "message"),
                Family::StreamUserEvents => {
                    log_failure!($route, $class, $phase, "stream_user_events")
                }
                Family::StreamChannel => log_failure!($route, $class, $phase, "stream_channel"),
                Family::Presence => log_failure!($route, $class, $phase, "presence"),
                Family::Other => log_failure!($route, $class, $phase, "other"),
            }
        };
    }
    macro_rules! event {
        ($route:literal, $class:literal) => {
            match phase {
                Phase::Ready => log_failure!($route, $class, "ready", "other"),
                Phase::Call => rpc!($route, $class, "call"),
                Phase::Body => rpc!($route, $class, "body"),
            }
        };
    }
    macro_rules! h2_classes {
        ($route:literal, $class:expr) => {
            match $class {
                Class::Connect => event!($route, "connect"),
                Class::Timeout => event!($route, "timeout"),
                Class::GoAway => event!($route, "goaway"),
                Class::Reset => event!($route, "reset"),
                Class::Io => event!($route, "io"),
                _ => event!($route, "other"),
            }
        };
    }

    let class = classify(route, error);
    match route {
        Route::H2 => h2_classes!("h2", class),
        #[cfg(feature = "onion")]
        Route::Tor => h2_classes!("tor", class),
        #[cfg(feature = "quic")]
        Route::H3 => match class {
            Class::Connect => event!("h3", "connect"),
            Class::Timeout => event!("h3", "timeout"),
            Class::ConnectionClosed | Class::GoAway => event!("h3", "conn_closed"),
            Class::Stream | Class::Reset => event!("h3", "stream"),
            _ => event!("h3", "other"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostic_classifier_walks_typed_h2_sources() {
        #[derive(Debug, thiserror::Error)]
        #[error("outer error")]
        struct Wrapper(#[source] io::Error);

        let typed = Wrapper(io::Error::from(io::ErrorKind::TimedOut));
        assert_eq!(classify(Route::H2, &typed), Class::Timeout);
        let nested_io = io::Error::other(io::Error::from(io::ErrorKind::TimedOut));
        assert_eq!(classify(Route::H2, &nested_io), Class::Timeout);
        let connect = tonic::ConnectError(Box::new(typed));
        assert_eq!(classify(Route::H2, &connect), Class::Connect);
        assert_eq!(
            classify(Route::H2, &tonic::TimeoutExpired(())),
            Class::Timeout
        );
        #[cfg(feature = "onion")]
        assert_eq!(classify(Route::Tor, &connect), Class::Connect);
        assert_eq!(
            classify(Route::H2, &io::Error::from(io::ErrorKind::BrokenPipe)),
            Class::Io
        );
        assert_eq!(
            classify(Route::H2, &io::Error::other("GOAWAY, reset, timeout")),
            Class::Io
        );
        assert_eq!(
            classify(Route::H2, &tonic::Status::unavailable("reset")),
            Class::Other
        );
    }

    #[cfg(feature = "quic")]
    #[test]
    fn diagnostic_classifier_distinguishes_h3_dial_close_and_stream() {
        use super::super::quic_lifecycle::ConnectFailure;
        assert_eq!(
            classify(
                Route::H3,
                &ConnectFailure(quinn::ConnectionError::TimedOut.into())
            ),
            Class::Connect
        );
        assert_eq!(
            classify(Route::H3, &quinn::ConnectionError::TimedOut),
            Class::Timeout
        );
        assert_eq!(
            classify(Route::H3, &quinn::ConnectionError::LocallyClosed),
            Class::ConnectionClosed
        );
        let stream = h3::error::StreamError::RemoteTerminate {
            code: h3::error::Code::H3_REQUEST_CANCELLED,
        };
        assert_eq!(classify(Route::H3, &stream), Class::Stream);
        let closed = h3::error::StreamError::ConnectionError(h3::error::ConnectionError::Timeout);
        assert_eq!(classify(Route::H3, &closed), Class::Timeout);
    }

    #[test]
    fn diagnostic_classifier_emits_static_targets_without_fields() {
        use std::sync::{Arc, Mutex};
        use tracing_subscriber::layer::{Context, Layer, SubscriberExt};

        #[derive(Clone, Default)]
        struct Events(Arc<Mutex<Vec<&'static str>>>);

        impl<S: tracing::Subscriber> Layer<S> for Events {
            fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
                let metadata = event.metadata();
                assert_eq!(*metadata.level(), tracing::Level::INFO);
                assert_eq!(metadata.fields().len(), 0);
                self.0.lock().unwrap().push(metadata.target());
            }
        }

        let events = Events::default();
        tracing::subscriber::with_default(
            tracing_subscriber::registry().with(events.clone().with_filter(
                tracing_subscriber::EnvFilter::new("warn,network_transport=info"),
            )),
            || {
                failure(
                    Route::H2,
                    Phase::Call,
                    Family::MailboxPoll,
                    &tonic::ConnectError(io::Error::other("private").into()),
                );
                failure(
                    Route::H2,
                    Phase::Ready,
                    Family::Other,
                    &tonic::TimeoutExpired(()),
                );
                failure(
                    Route::H2,
                    Phase::Body,
                    Family::StreamChannel,
                    &io::Error::from(io::ErrorKind::BrokenPipe),
                );
            },
        );
        assert_eq!(
            *events.0.lock().unwrap(),
            [
                "network_transport::fail_h2_connect_call__mailbox_poll",
                "network_transport::fail_h2_timeout_ready__other",
                "network_transport::fail_h2_io_body__stream_channel",
            ]
        );
    }

    #[test]
    fn diagnostic_classifier_allows_only_known_method_families() {
        for (path, family) in [
            (
                "/chat.v1.mailbox.EphemeralMailboxService/GetMailboxChallenge",
                Family::MailboxChallenge,
            ),
            (
                "/chat.v1.mailbox.EphemeralMailboxService/PollEphemeralMailbox",
                Family::MailboxPoll,
            ),
            ("/chat.v1.MessageService/GetDeviceMessages", Family::Message),
            (
                "/chat.v1.UserService/StreamUserEvents",
                Family::StreamUserEvents,
            ),
            (
                "/chat.v1.ChannelService/StreamChannelEvents",
                Family::StreamChannel,
            ),
            ("/chat.v1.UserService/UpdatePresence", Family::Presence),
            (
                "/chat.v1.UserService/StreamUserEvents/private-id",
                Family::Other,
            ),
        ] {
            assert_eq!(Family::from_path(path), family);
        }
    }
}

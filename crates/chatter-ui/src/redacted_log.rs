//! Keep diagnostic location and severity without writing event values to logs.
//!
//! Event fields include formatted messages and errors as well as structured
//! identifiers. Formatting them before redaction can leak arbitrary content,
//! so this formatter never visits the fields at all.

use std::fmt;
use tracing::{Event, Subscriber};
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields, format::Writer};
use tracing_subscriber::registry::LookupSpan;

pub(crate) struct RedactedEventFormatter;

impl<S, N> FormatEvent<S, N> for RedactedEventFormatter
where
    S: Subscriber + for<'span> LookupSpan<'span>,
    N: for<'writer> FormatFields<'writer> + 'static,
{
    fn format_event(
        &self,
        _ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let meta = event.metadata();
        writeln!(
            writer,
            "{} {} {} [event values redacted]",
            chrono::Utc::now().to_rfc3339(),
            meta.level(),
            meta.target()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{self, Write};
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Output(Arc<Mutex<Vec<u8>>>);
    impl Write for Output {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    struct MustNotFormat;
    impl fmt::Debug for MustNotFormat {
        fn fmt(&self, _: &mut fmt::Formatter<'_>) -> fmt::Result {
            panic!("release formatter visited a private event value")
        }
    }

    #[test]
    fn setup_targets_survive_release_filter_without_visiting_any_values() {
        let output = Output::default();
        let writer = output.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_env_filter("warn,call_setup=info")
            .event_format(RedactedEventFormatter)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: "call_setup::room_connect_begin",
                error = ?MustNotFormat, token = "PRIVATE_TOKEN_SENTINEL", "PRIVATE_MESSAGE_SENTINEL");
            tracing::warn!(target: "call_setup::sdk_signal_auth",
                response = ?MustNotFormat, room = "PRIVATE_ROOM_SENTINEL", "PRIVATE_ERROR_SENTINEL");
            tracing::info!(target: "ordinary_target", "SHOULD_BE_FILTERED");
        });
        let text = String::from_utf8(output.0.lock().unwrap().clone()).unwrap();
        assert!(text.contains("INFO call_setup::room_connect_begin [event values redacted]"));
        assert!(text.contains("WARN call_setup::sdk_signal_auth [event values redacted]"));
        assert!(!text.contains("PRIVATE"));
        assert!(!text.contains("ordinary_target"));
        assert_eq!(text.lines().count(), 2);
    }
}

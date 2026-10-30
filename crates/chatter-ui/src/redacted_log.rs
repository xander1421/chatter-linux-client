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

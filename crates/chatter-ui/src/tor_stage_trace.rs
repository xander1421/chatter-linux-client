//! Opt-in, bounded Tor dial diagnostics. Never write Arti event fields verbatim:
//! they can contain onion addresses, relay identities, and circuit details.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use tracing::{Event, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;

pub(crate) struct TorStageTrace {
    file: Mutex<File>,
    lines: AtomicUsize,
    started: Instant,
}

pub(crate) fn dial_active() -> bool {
    #[cfg(feature = "onion")]
    {
        chatter_core::network::grpc::onion_transport::stage_trace_active()
    }
    #[cfg(not(feature = "onion"))]
    {
        false
    }
}

impl TorStageTrace {
    pub(crate) fn open() -> Option<Self> {
        #[cfg(target_os = "android")]
        let path = {
            let dir = super::ANDROID_DATA_DIR.get()?;
            let dir = PathBuf::from(dir);
            if !dir.join("tor-stage-trace.enable").exists() {
                return None;
            }
            dir.join("tor-stage-trace.log")
        };
        #[cfg(not(target_os = "android"))]
        let path = PathBuf::from(std::env::var_os("CHATTER_TOR_STAGE_TRACE")?);

        // A new app process starts a new capture; the private app directory on
        // Android and an explicit path on desktop make collection predictable.
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path)
            .ok()?;
        Some(Self {
            file: Mutex::new(file),
            lines: AtomicUsize::new(0),
            started: Instant::now(),
        })
    }
}

#[derive(Default)]
struct Message {
    text: String,
    elapsed_ms: Option<u64>,
    attempts: Option<u64>,
    attempt: Option<u64>,
    cache_present: Option<bool>,
    outcome: Option<&'static str>,
    error_kind: Option<String>,
}

impl tracing::field::Visit for Message {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if matches!(field.name(), "message" | "error") {
            self.text.push_str(&format!(" {value:?}"));
        } else if field.name() == "error_kind" {
            // Only keep enum-like names; never copy arbitrary Arti values.
            let formatted = format!("{value:?}");
            let value = formatted
                .strip_prefix("Some(")
                .and_then(|v| v.strip_suffix(')'))
                .unwrap_or(&formatted);
            if value.len() <= 48 && value.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                self.error_kind = Some(value.to_owned());
            }
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.text.push_str(value);
        } else if field.name() == "outcome" {
            self.outcome = match value {
                "completed" => Some("completed"),
                "failed" => Some("failed"),
                _ => None,
            };
        }
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        match field.name() {
            "elapsed_ms" => self.elapsed_ms = Some(value),
            "attempts" => self.attempts = Some(value),
            "attempt" => self.attempt = Some(value),
            _ => {}
        }
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        if field.name() == "cache_present" {
            self.cache_present = Some(value);
        }
    }

    fn record_error(
        &mut self,
        field: &tracing::field::Field,
        value: &(dyn std::error::Error + 'static),
    ) {
        if field.name() != "error" {
            return;
        }
        let mut source = Some(value);
        while let Some(error) = source {
            self.text.push_str(&format!(" {error}"));
            source = error.source();
        }
    }
}

fn stage(message: &str) -> &'static str {
    let msg = message.to_ascii_lowercase();
    if msg.contains("onion tor bootstrap") {
        "bootstrap"
    } else if msg.contains("onion hidden-service attempt") {
        "dial_attempt"
    } else if msg.contains("onion hidden-service dial") {
        "dial"
    } else if msg.contains("http status code 404") {
        "hsdir_404"
    } else if msg.contains("hs desc fetch") || msg.contains("descriptor") {
        "descriptor"
    } else if msg.contains("introduction")
        || msg.contains("intro circuit")
        || msg.contains("introduce1")
    {
        "introduction"
    } else if msg.contains("rendezvous") {
        "rendezvous"
    } else if msg.contains("circuit") {
        "circuit"
    } else if msg.contains("consensus") || msg.contains("directory") {
        "directory"
    } else {
        "other"
    }
}

impl<S: Subscriber> Layer<S> for TorStageTrace {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let target = event.metadata().target();
        let component = if target.starts_with("tor_hsclient") {
            "hsclient"
        } else if target.starts_with("tor_circmgr") {
            "circmgr"
        } else if target.starts_with("tor_dirmgr") {
            "dirmgr"
        } else if target == "chatter_core::network::grpc::onion_transport" {
            "connector"
        } else {
            return;
        };
        let mut message = Message::default();
        event.record(&mut message);
        let stage = stage(&message.text);
        if stage == "other" {
            return;
        }
        let detail = message.text.to_ascii_lowercase();
        let outcome = if detail.contains("timed out") || detail.contains("timeout") {
            "timeout"
        } else if detail.contains("failed") || detail.contains("error") {
            "failure"
        } else {
            "progress"
        };
        let outcome = message.outcome.unwrap_or(outcome);
        if self.lines.fetch_add(1, Ordering::Relaxed) >= 10_000 {
            return;
        }
        if let Ok(mut file) = self.file.lock() {
            let _ = writeln!(
                file,
                "{} elapsed_ms={} component={} level={} stage={} outcome={} phase_ms={:?} attempt={:?} attempts={:?} cache_present={:?} error_kind={:?}",
                chrono::Utc::now().to_rfc3339(),
                self.started.elapsed().as_millis(),
                component,
                event.metadata().level(),
                stage,
                outcome,
                message.elapsed_ms,
                message.attempt,
                message.attempts,
                message.cache_present,
                message.error_kind
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::stage;

    #[test]
    fn stage_labels_do_not_include_arti_details() {
        assert_eq!(
            stage("failed hsdir desc fetch: HTTP status code 404"),
            "hsdir_404"
        );
        assert_eq!(stage("IPT #2: obtaining intro circuit"), "introduction");
        assert_eq!(stage("received RENDEZVOUS2"), "rendezvous");
    }
}

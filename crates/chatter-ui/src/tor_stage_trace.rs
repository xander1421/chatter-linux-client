//! Opt-in, bounded Tor dial diagnostics. Never write Arti event fields verbatim:
//! they can contain onion addresses, relay identities, and circuit details.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use tracing::{Event, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::Layer;

pub(crate) struct TorStageTrace {
    file: Mutex<File>,
    lines: AtomicUsize,
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
        })
    }
}

#[derive(Default)]
struct Message(String);

impl tracing::field::Visit for Message {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if matches!(field.name(), "message" | "error") {
            self.0.push_str(&format!(" {value:?}"));
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
            self.0.push_str(&format!(" {error}"));
            source = error.source();
        }
    }
}

fn stage(message: &str) -> &'static str {
    let msg = message.to_ascii_lowercase();
    if msg.contains("http status code 404") {
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
        } else {
            return;
        };
        let mut message = Message::default();
        event.record(&mut message);
        let stage = stage(&message.0);
        if stage == "other" {
            return;
        }
        let detail = message.0.to_ascii_lowercase();
        let outcome = if detail.contains("timed out") || detail.contains("timeout") {
            "timeout"
        } else if detail.contains("failed") || detail.contains("error") {
            "failure"
        } else {
            "progress"
        };
        if self.lines.fetch_add(1, Ordering::Relaxed) >= 10_000 {
            return;
        }
        if let Ok(mut file) = self.file.lock() {
            let _ = writeln!(
                file,
                "{} component={} level={} stage={} outcome={}",
                chrono::Utc::now().to_rfc3339(),
                component,
                event.metadata().level(),
                stage,
                outcome
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

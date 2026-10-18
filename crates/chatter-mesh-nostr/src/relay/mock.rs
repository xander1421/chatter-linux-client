//! An in-memory relay, so no test in this crate touches the network.

use super::*;
use std::collections::VecDeque;
use std::sync::Mutex as StdMutex;
use tokio::sync::mpsc;

/// What a mock connection does when dialled.
pub enum Script {
    /// Accept, then serve these frames, then stay open until closed.
    Serve(Vec<String>),
    /// Accept, serve these frames, then close cleanly.
    ServeThenClose(Vec<String>),
    /// Refuse to connect.
    Fail(&'static str),
}

#[derive(Default)]
pub struct MockRelay {
    scripts: StdMutex<VecDeque<Script>>,
    /// Every frame the client wrote, in order.
    pub sent: Arc<StdMutex<Vec<String>>>,
    pub connect_count: Arc<StdMutex<usize>>,
    /// Feeds frames to whichever connection is currently live.
    live: StdMutex<Option<mpsc::UnboundedSender<Option<String>>>>,
}

impl MockRelay {
    pub fn new(scripts: Vec<Script>) -> Arc<Self> {
        Arc::new(Self {
            scripts: StdMutex::new(scripts.into()),
            ..Self::default()
        })
    }

    pub fn sent_frames(&self) -> Vec<String> {
        self.sent.lock().map(|s| s.clone()).unwrap_or_default()
    }

    pub fn connects(&self) -> usize {
        self.connect_count.lock().map(|c| *c).unwrap_or(0)
    }

    /// Pushes a frame into the live connection.
    pub fn push(&self, frame: &str) {
        if let Ok(live) = self.live.lock() {
            if let Some(tx) = live.as_ref() {
                let _ = tx.send(Some(frame.to_string()));
            }
        }
    }

    /// Closes the live connection from the relay's side.
    pub fn drop_connection(&self) {
        if let Ok(live) = self.live.lock() {
            if let Some(tx) = live.as_ref() {
                let _ = tx.send(None);
            }
        }
    }
}

impl RelayConnector for MockRelay {
    fn connect<'a>(&'a self, _url: &'a str) -> BoxFuture<'a, Result<RelayConnection, NostrError>> {
        Box::pin(async move {
            if let Ok(mut count) = self.connect_count.lock() {
                *count += 1;
            }
            let script = self
                .scripts
                .lock()
                .ok()
                .and_then(|mut s| s.pop_front())
                .unwrap_or(Script::Serve(Vec::new()));

            let (frames, close_after) = match script {
                Script::Fail(reason) => {
                    return Err(NostrError::Relay(reason.to_string()));
                }
                Script::Serve(frames) => (frames, false),
                Script::ServeThenClose(frames) => (frames, true),
            };

            let (tx, rx) = mpsc::unbounded_channel();
            for frame in frames {
                let _ = tx.send(Some(frame));
            }
            if close_after {
                let _ = tx.send(None);
            }
            if let Ok(mut live) = self.live.lock() {
                *live = Some(tx);
            }

            let sink: Box<dyn RelaySink> = Box::new(MockSink {
                sent: Arc::clone(&self.sent),
            });
            let stream: Box<dyn RelayStream> = Box::new(MockStream { rx });
            Ok((sink, stream))
        })
    }
}

pub struct MockSink {
    sent: Arc<StdMutex<Vec<String>>>,
}

impl RelaySink for MockSink {
    fn send(&mut self, text: String) -> BoxFuture<'_, Result<(), NostrError>> {
        if let Ok(mut sent) = self.sent.lock() {
            sent.push(text);
        }
        Box::pin(async { Ok(()) })
    }

    fn close(&mut self) -> BoxFuture<'_, Result<(), NostrError>> {
        Box::pin(async { Ok(()) })
    }
}

pub struct MockStream {
    rx: mpsc::UnboundedReceiver<Option<String>>,
}

impl RelayStream for MockStream {
    fn recv(&mut self) -> BoxFuture<'_, Result<Option<String>, NostrError>> {
        Box::pin(async move { Ok(self.rx.recv().await.flatten()) })
    }
}

/// A stream that never yields and never ends — a connection that is simply
/// quiet, which is what a healthy relay looks like most of the time.
pub struct PendingStream;

impl RelayStream for PendingStream {
    fn recv(&mut self) -> BoxFuture<'_, Result<Option<String>, NostrError>> {
        Box::pin(std::future::pending())
    }
}

/// A connector whose sink always fails, to exercise the write-failure path.
pub struct BrokenSinkRelay;

impl RelayConnector for BrokenSinkRelay {
    fn connect<'a>(&'a self, _url: &'a str) -> BoxFuture<'a, Result<RelayConnection, NostrError>> {
        Box::pin(async move {
            let sink: Box<dyn RelaySink> = Box::new(BrokenSink);
            let stream: Box<dyn RelayStream> = Box::new(PendingStream);
            Ok((sink, stream))
        })
    }
}

struct BrokenSink;

impl RelaySink for BrokenSink {
    fn send(&mut self, _text: String) -> BoxFuture<'_, Result<(), NostrError>> {
        Box::pin(async { Err(NostrError::Relay("socket is gone".to_string())) })
    }

    fn close(&mut self) -> BoxFuture<'_, Result<(), NostrError>> {
        Box::pin(async { Ok(()) })
    }
}

/// A config with millisecond backoffs and no jitter, so lifecycle tests are
/// fast and deterministic.
pub fn fast_config() -> RelayClientConfig {
    RelayClientConfig {
        initial_backoff: Duration::from_millis(1),
        max_backoff: Duration::from_millis(4),
        backoff_jitter_ratio: 0.0,
        jitter: Arc::new(|| 0.5),
        ..RelayClientConfig::default()
    }
}

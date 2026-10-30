//! Test doubles for the two IO seams: the radio and the relay socket.
//!
//! Compiled only under `cfg(test)`. Everything the mesh stack does above these
//! two traits is exercised without hardware and without a network; anything that
//! genuinely needs a radio or a relay is verified by running it, not by a test
//! that pretends.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use chatter_mesh_link::LinkError;
use chatter_mesh_link::link::{BoxFuture, LinkEvent, LinkId, MeshLink};
use chatter_mesh_nostr::NostrError;
use chatter_mesh_nostr::relay::{
    BoxFuture as RelayBoxFuture, RelayConnection, RelayConnector, RelaySink, RelayStream,
};
use parking_lot::Mutex;
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::mpsc;

/// A radio that records what was written to it and replays scripted events.
pub struct MockLink {
    /// Held so `next_event` pends instead of reporting the radio closed.
    _events_tx: mpsc::UnboundedSender<LinkEvent>,
    events_rx: AsyncMutex<mpsc::UnboundedReceiver<LinkEvent>>,
    writes: Mutex<Vec<(LinkId, Vec<u8>)>>,
    max_write: Mutex<BTreeMap<LinkId, usize>>,
    started: AtomicBool,
    connects: Mutex<Vec<String>>,
    /// Every write is rejected while set, so the backpressure path can be driven.
    reject_writes: AtomicBool,
    links_gone: AtomicBool,
    stopped_once: AtomicBool,
    connect_error: Mutex<Option<LinkError>>,
}

impl Default for MockLink {
    fn default() -> Self {
        Self::new()
    }
}

impl MockLink {
    #[must_use]
    pub fn new() -> Self {
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        Self {
            _events_tx: events_tx,
            events_rx: AsyncMutex::new(events_rx),
            writes: Mutex::new(Vec::new()),
            max_write: Mutex::new(BTreeMap::new()),
            started: AtomicBool::new(false),
            connects: Mutex::new(Vec::new()),
            reject_writes: AtomicBool::new(false),
            links_gone: AtomicBool::new(false),
            stopped_once: AtomicBool::new(false),
            connect_error: Mutex::new(None),
        }
    }

    #[must_use]
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::new())
    }

    #[must_use]
    pub fn is_started(&self) -> bool {
        self.started.load(Ordering::SeqCst)
    }

    #[must_use]
    pub fn writes(&self) -> Vec<(LinkId, Vec<u8>)> {
        self.writes.lock().clone()
    }

    #[must_use]
    pub fn write_count(&self) -> usize {
        self.writes.lock().len()
    }

    #[must_use]
    pub fn connect_attempts(&self) -> Vec<String> {
        self.connects.lock().clone()
    }

    /// Refuse writes with a *retryable* error, as a full controller queue does.
    pub fn set_reject_writes(&self, reject: bool) {
        self.reject_writes.store(reject, Ordering::SeqCst);
    }

    /// Make `connect` fail with this error, as a real radio can.
    ///
    /// A connect that always succeeds cannot exercise the scheduler's backoff, and
    /// the distinction the runtime draws between a *timeout* and an ordinary
    /// refusal — different ignore windows, deliberately — was unreachable from a
    /// test without it.
    pub fn set_connect_error(&self, error: Option<LinkError>) {
        *self.connect_error.lock() = error;
    }

    pub fn set_max_write(&self, link: &LinkId, max_write: usize) {
        self.max_write.lock().insert(link.clone(), max_write);
    }

    pub fn set_links_gone(&self, gone: bool) {
        self.links_gone.store(gone, Ordering::SeqCst);
    }
}

impl MeshLink for MockLink {
    /// Single-use, like the real link layers.
    ///
    /// `AndroidLink` refuses a restart outright — "this link layer has been
    /// stopped; construct a new AndroidLink to restart" — and this mock used to
    /// succeed instead. That leniency is what let a broken mesh switch ship: the
    /// restart test passed against a mock that would restart, while the phone in
    /// the room would not, and the failure only appeared when a human tapped the
    /// control. A mock that is easier to satisfy than production tests nothing
    /// about production.
    fn start(&self) -> BoxFuture<'_, Result<(), LinkError>> {
        if self.stopped_once.load(Ordering::SeqCst) {
            return Box::pin(async {
                Err(LinkError::Backend(
                    "this mock link layer has been stopped; construct a new one to restart"
                        .to_string(),
                ))
            });
        }
        self.started.store(true, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }

    fn stop(&self) -> BoxFuture<'_, Result<(), LinkError>> {
        self.stopped_once.store(true, Ordering::SeqCst);
        self.started.store(false, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }

    fn set_scanning(&self, _on: bool) -> BoxFuture<'_, Result<(), LinkError>> {
        Box::pin(async { Ok(()) })
    }

    fn connect(&self, address: &str) -> BoxFuture<'_, Result<LinkId, LinkError>> {
        self.connects.lock().push(address.to_string());
        if let Some(error) = self.connect_error.lock().clone() {
            return Box::pin(async move { Err(error) });
        }
        let link = LinkId::new(format!("/mock/{address}"));
        Box::pin(async move { Ok(link) })
    }

    fn disconnect(&self, _link: &LinkId) -> BoxFuture<'_, Result<(), LinkError>> {
        Box::pin(async { Ok(()) })
    }

    fn send(&self, link: &LinkId, bytes: &[u8]) -> BoxFuture<'_, Result<(), LinkError>> {
        if self.links_gone.load(Ordering::SeqCst) {
            let id = link.to_string();
            return Box::pin(async move { Err(LinkError::LinkGone(id)) });
        }
        if self.reject_writes.load(Ordering::SeqCst) {
            let id = link.to_string();
            return Box::pin(async move { Err(LinkError::WriteRejected(id)) });
        }
        // Both real link layers refuse an oversized frame — BlueZ and the Android
        // bridge each return `PayloadTooLarge` — and this used to accept anything.
        // A fragment sized past the negotiated MTU would therefore pass every test
        // and be dropped on a radio, which is the failure mode fragmentation bugs
        // actually have.
        //
        // Only enforced where a size is known for the link: an unnegotiated link
        // has no MTU to measure against, and inventing one would fail writes that
        // production would accept.
        if let Some(max) = self.max_write.lock().get(link).copied()
            && bytes.len() > max
        {
            let actual = bytes.len();
            return Box::pin(async move { Err(LinkError::PayloadTooLarge { actual, max }) });
        }
        self.writes.lock().push((link.clone(), bytes.to_vec()));
        Box::pin(async { Ok(()) })
    }

    fn max_write(&self, link: &LinkId) -> Option<usize> {
        self.max_write.lock().get(link).copied()
    }

    fn next_event(&self) -> BoxFuture<'_, Option<LinkEvent>> {
        Box::pin(async move { self.events_rx.lock().await.recv().await })
    }
}

/// A relay connector whose sockets go nowhere.
///
/// Every `connect` fails, which is exactly the state a device with no internet
/// is in — and the state `can_deliver_promptly` must report as false.
#[derive(Debug, Default)]
pub struct OfflineConnector;

impl RelayConnector for OfflineConnector {
    fn connect<'a>(
        &'a self,
        url: &'a str,
    ) -> RelayBoxFuture<'a, Result<RelayConnection, NostrError>> {
        let url = url.to_string();
        Box::pin(async move { Err(NostrError::Relay(format!("offline: {url}"))) })
    }
}

/// A relay connector backed by in-memory channels, so the relay client's frame
/// handling can be driven without a socket.
pub struct LoopbackConnector {
    /// Frames the client wrote.
    pub sent: Arc<Mutex<Vec<String>>>,
}

impl Default for LoopbackConnector {
    fn default() -> Self {
        Self::new()
    }
}

impl LoopbackConnector {
    #[must_use]
    pub fn new() -> Self {
        Self {
            sent: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

struct LoopbackSink {
    sent: Arc<Mutex<Vec<String>>>,
}

impl RelaySink for LoopbackSink {
    fn send(&mut self, text: String) -> RelayBoxFuture<'_, Result<(), NostrError>> {
        self.sent.lock().push(text);
        Box::pin(async { Ok(()) })
    }

    fn close(&mut self) -> RelayBoxFuture<'_, Result<(), NostrError>> {
        Box::pin(async { Ok(()) })
    }
}

struct LoopbackStream {
    frames: std::collections::VecDeque<String>,
}

impl RelayStream for LoopbackStream {
    fn recv(&mut self) -> RelayBoxFuture<'_, Result<Option<String>, NostrError>> {
        let next = self.frames.pop_front();
        Box::pin(async move {
            match next {
                Some(frame) => Ok(Some(frame)),
                // No more scripted frames: park forever rather than reporting a
                // clean close, so the client does not spin through its backoff.
                None => {
                    std::future::pending::<()>().await;
                    Ok(None)
                }
            }
        })
    }
}

impl RelayConnector for LoopbackConnector {
    fn connect<'a>(
        &'a self,
        _url: &'a str,
    ) -> RelayBoxFuture<'a, Result<RelayConnection, NostrError>> {
        let sink = LoopbackSink {
            sent: Arc::clone(&self.sent),
        };
        let stream = LoopbackStream {
            frames: Default::default(),
        };
        Box::pin(async move {
            Ok((
                Box::new(sink) as Box<dyn RelaySink>,
                Box::new(stream) as Box<dyn RelayStream>,
            ))
        })
    }
}

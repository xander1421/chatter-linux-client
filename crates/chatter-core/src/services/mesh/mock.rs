//! Test doubles for the two IO seams: the radio and the relay socket.
//!
//! Compiled only under `cfg(test)`. Everything the mesh stack does above these
//! two traits is exercised without hardware and without a network; anything that
//! genuinely needs a radio or a relay is verified by running it, not by a test
//! that pretends.

// A test double exposes the whole seam it stands in for, and not every test
// needs every accessor. Trimming to whatever the current tests happen to call
// would make the next test start by re-adding one.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use chatter_mesh_link::link::{BoxFuture, LinkEvent, LinkId, MeshLink};
use chatter_mesh_link::LinkError;
use chatter_mesh_nostr::NostrError;
use chatter_mesh_nostr::relay::{
    BoxFuture as RelayBoxFuture, RelayConnection, RelayConnector, RelaySink, RelayStream,
};
use parking_lot::Mutex;
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::mpsc;

/// A radio that records what was written to it and replays scripted events.
pub struct MockLink {
    events_tx: Mutex<Option<mpsc::UnboundedSender<LinkEvent>>>,
    events_rx: AsyncMutex<mpsc::UnboundedReceiver<LinkEvent>>,
    writes: Mutex<Vec<(LinkId, Vec<u8>)>>,
    max_write: Mutex<BTreeMap<LinkId, usize>>,
    started: AtomicBool,
    scanning: AtomicBool,
    scan_changes: AtomicUsize,
    connects: Mutex<Vec<String>>,
    /// Every write is rejected while set, so the backpressure path can be driven.
    reject_writes: AtomicBool,
    links_gone: AtomicBool,
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
            events_tx: Mutex::new(Some(events_tx)),
            events_rx: AsyncMutex::new(events_rx),
            writes: Mutex::new(Vec::new()),
            max_write: Mutex::new(BTreeMap::new()),
            started: AtomicBool::new(false),
            scanning: AtomicBool::new(false),
            scan_changes: AtomicUsize::new(0),
            connects: Mutex::new(Vec::new()),
            reject_writes: AtomicBool::new(false),
            links_gone: AtomicBool::new(false),
        }
    }

    #[must_use]
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::new())
    }

    /// Queue an event for the driver to observe.
    pub fn push(&self, event: LinkEvent) {
        if let LinkEvent::Connected {
            link, max_write, ..
        } = &event
        {
            self.max_write.lock().insert(link.clone(), *max_write);
        }
        if let Some(tx) = self.events_tx.lock().as_ref() {
            let _ = tx.send(event);
        }
    }

    /// Close the event stream, which is how a radio reports it has shut down
    /// for good: `next_event` resolves to `None`.
    pub fn close(&self) {
        self.events_tx.lock().take();
    }

    #[must_use]
    pub fn is_started(&self) -> bool {
        self.started.load(Ordering::SeqCst)
    }

    #[must_use]
    pub fn is_scanning(&self) -> bool {
        self.scanning.load(Ordering::SeqCst)
    }

    #[must_use]
    pub fn scan_changes(&self) -> usize {
        self.scan_changes.load(Ordering::SeqCst)
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

    /// Refuse writes with a *terminal* error, as a disconnected link does.
    ///
    /// Distinct from `set_reject_writes` because the two are handled in opposite
    /// ways and `should_buffer` is what tells them apart: a retryable refusal goes
    /// back on the queue, a terminal one takes the queue with it.
    pub fn set_links_gone(&self, gone: bool) {
        self.links_gone.store(gone, Ordering::SeqCst);
    }
}

impl MeshLink for MockLink {
    fn start(&self) -> BoxFuture<'_, Result<(), LinkError>> {
        self.started.store(true, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }

    fn stop(&self) -> BoxFuture<'_, Result<(), LinkError>> {
        self.started.store(false, Ordering::SeqCst);
        self.scanning.store(false, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }

    fn set_scanning(&self, on: bool) -> BoxFuture<'_, Result<(), LinkError>> {
        if self.scanning.swap(on, Ordering::SeqCst) != on {
            self.scan_changes.fetch_add(1, Ordering::SeqCst);
        }
        Box::pin(async { Ok(()) })
    }

    fn connect(&self, address: &str) -> BoxFuture<'_, Result<LinkId, LinkError>> {
        self.connects.lock().push(address.to_string());
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
    fn connect<'a>(&'a self, url: &'a str) -> RelayBoxFuture<'a, Result<RelayConnection, NostrError>> {
        let url = url.to_string();
        Box::pin(async move { Err(NostrError::Relay(format!("offline: {url}"))) })
    }
}

/// A relay connector backed by in-memory channels, so the relay client's frame
/// handling can be driven without a socket.
pub struct LoopbackConnector {
    /// Frames the client wrote.
    pub sent: Arc<Mutex<Vec<String>>>,
    /// Frames to feed back, in order, per connection.
    inbound: Arc<Mutex<Vec<String>>>,
    connects: AtomicUsize,
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
            inbound: Arc::new(Mutex::new(Vec::new())),
            connects: AtomicUsize::new(0),
        }
    }

    pub fn queue_inbound(&self, frame: impl Into<String>) {
        self.inbound.lock().push(frame.into());
    }

    #[must_use]
    pub fn connect_count(&self) -> usize {
        self.connects.load(Ordering::SeqCst)
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
    fn connect<'a>(&'a self, _url: &'a str) -> RelayBoxFuture<'a, Result<RelayConnection, NostrError>> {
        self.connects.fetch_add(1, Ordering::SeqCst);
        let sink = LoopbackSink {
            sent: Arc::clone(&self.sent),
        };
        let stream = LoopbackStream {
            frames: self.inbound.lock().iter().cloned().collect(),
        };
        Box::pin(async move {
            Ok((
                Box::new(sink) as Box<dyn RelaySink>,
                Box::new(stream) as Box<dyn RelayStream>,
            ))
        })
    }
}

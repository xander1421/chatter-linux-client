use super::registration::{clear_active, set_active};
use super::*;

/// The Android radio, driven through a Kotlin `MeshBleBridge`.
///
/// Runs the central and peripheral roles together, as the mesh requires. Only
/// one instance can be receiving callbacks at a time: the native method table
/// belongs to the process, so constructing a second `AndroidLink` takes the
/// callbacks over from the first.
pub struct AndroidLink {
    inner: Arc<Shared>,
}

impl AndroidLink {
    /// No JNI is touched until [`MeshLink::start`], beyond taking over the
    /// callback registration so that nothing arriving in between is lost.
    pub fn new() -> Self {
        Self::with_queue_capacity(EVENT_QUEUE_CAPACITY)
    }

    fn with_queue_capacity(capacity: usize) -> Self {
        let inner = Shared::new(capacity);
        set_active(&inner);
        Self { inner }
    }

    /// Number of links currently established. Diagnostics only.
    pub fn link_count(&self) -> usize {
        self.inner.conns().len()
    }

    /// Ask Kotlin for the live size of a link, recording whatever it says.
    ///
    /// Polling is the only way an MTU renegotiation reaches Rust: there is no
    /// `nativeOnMtuChanged`, and `onMtuChanged` routinely lands after the link
    /// is already writable. Returns the cached size when Kotlin has nothing
    /// better to offer.
    fn refresh_max_write(&self, link: &LinkId) -> Option<usize> {
        let handle = self.inner.conns().handle(link)?;
        if !bridge::is_installed() {
            return self.inner.conns().max_write(link);
        }
        match bridge::max_write(&handle) {
            Some(reported) => {
                let live = sanitise_max_write(reported);
                self.inner.conns().set_max_write(link, live);
                Some(live)
            }
            None => self.inner.conns().max_write(link),
        }
    }
}

impl Default for AndroidLink {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for AndroidLink {
    fn drop(&mut self) {
        // Leaving a dangling registration behind would make every later
        // callback log a warning about a link that no longer exists.
        clear_active(&self.inner);
    }
}

impl core::fmt::Debug for AndroidLink {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AndroidLink")
            .field("links", &self.link_count())
            .field("stopped", &self.inner.stopped())
            .finish()
    }
}

impl MeshLink for AndroidLink {
    fn start(&self) -> BoxFuture<'_, Result<(), LinkError>> {
        Box::pin(async move {
            if self.inner.stopped() {
                return Err(error::stopped());
            }
            // Reassert the registration in case another link was constructed
            // and dropped in between; callbacks must reach whoever is running.
            set_active(&self.inner);
            bridge::start()
        })
    }

    fn stop(&self) -> BoxFuture<'_, Result<(), LinkError>> {
        Box::pin(async move {
            // Best effort: the local link layer is stopping either way, and
            // reporting an error for a radio that is already unreachable would
            // only make callers handle a failure they cannot act on.
            if let Err(err) = bridge::stop() {
                error::log_warn(&format!("mesh: MeshBleBridge.stop() failed: {err}"));
            }

            // Announce the dead links while the queue is still open.
            self.inner.drop_all();
            clear_active(&self.inner);
            self.inner.close_queue();
            Ok(())
        })
    }

    fn set_scanning(&self, on: bool) -> BoxFuture<'_, Result<(), LinkError>> {
        Box::pin(async move {
            if !on && (self.inner.stopped() || !bridge::is_installed()) {
                // The duty cycle calls this unconditionally, so turning
                // scanning off with nothing running is a no-op rather than an
                // error, exactly as on Linux.
                return Ok(());
            }
            if self.inner.stopped() {
                return Err(error::stopped());
            }
            bridge::set_scanning(on)
        })
    }

    fn set_local_peer(&self, peer: [u8; 8]) {
        // Best effort: without it peers cannot recognise us across an address
        // rotation, which is a churn problem, not a broken radio. Said at warn
        // because the symptom it produces — a peer reconnecting endlessly under
        // new addresses — looks like anything but a missing advertisement field.
        let hex: String = peer.iter().map(|byte| format!("{byte:02x}")).collect();
        if let Err(error) = bridge::set_local_peer(&hex) {
            error::log_warn(&format!(
                "mesh: could not advertise our peer id; peers will not recognise us \
                 across an address rotation: {error}"
            ));
        }
    }

    fn connect(&self, address: &str) -> BoxFuture<'_, Result<LinkId, LinkError>> {
        let address = address.to_string();
        Box::pin(async move {
            if self.inner.stopped() {
                return Err(error::stopped());
            }

            let handle = bridge::connect(&address)?;
            if handle.is_empty() {
                return Err(error::connect_failed(
                    &address,
                    "connect() returned an empty handle, which names no connection",
                ));
            }

            // The id exists now; the link is not writable yet. `Connected`
            // follows when Kotlin reports the write path is up, and a failed
            // attempt still produces `Disconnected` for this id — the caller is
            // holding it either way.
            Ok(self.inner.open_central(&handle))
        })
    }

    fn disconnect(&self, link: &LinkId) -> BoxFuture<'_, Result<(), LinkError>> {
        let link = link.clone();
        Box::pin(async move {
            let removed = { self.inner.conns().remove(&link) };
            let Some(conn) = removed else {
                return Err(error::link_gone(&link));
            };
            if conn.visible {
                self.inner.emit(LinkEvent::Disconnected { link });
            }

            // Best effort, and after the event: the link is gone from our point
            // of view whatever Kotlin does with the handle, and a failure here
            // must not look like a failure to disconnect.
            if let Err(err) = bridge::disconnect(&conn.handle) {
                error::log_warn(&format!("mesh: disconnect({}) failed: {err}", conn.handle));
            }
            Ok(())
        })
    }

    fn send(&self, link: &LinkId, bytes: &[u8]) -> BoxFuture<'_, Result<(), LinkError>> {
        let link = link.clone();
        let bytes = bytes.to_vec();
        Box::pin(async move {
            let target = { self.inner.conns().target(&link) };
            let Some((handle, cached)) = target else {
                return Err(error::link_gone(&link));
            };

            // Only ask Kotlin when the cached size would reject the payload: an
            // MTU that grew after `Connected` is invisible until something asks,
            // and rejecting a packet the link could actually carry is a silent
            // throughput collapse rather than an error anyone would notice.
            let mut max = cached;
            if bytes.len() > max {
                if let Some(live) = self.refresh_max_write(&link) {
                    max = live;
                }
            }
            if bytes.len() > max {
                return Err(LinkError::PayloadTooLarge {
                    actual: bytes.len(),
                    max,
                });
            }

            match bridge::send(&handle, &bytes) {
                Ok(()) => Ok(()),
                Err(err) => {
                    if error::is_terminal(&err) {
                        self.inner.drop_link(&link);
                    }
                    Err(err)
                }
            }
        })
    }

    fn max_write(&self, link: &LinkId) -> Option<usize> {
        self.refresh_max_write(link)
    }

    fn next_event(&self) -> BoxFuture<'_, Option<LinkEvent>> {
        Box::pin(async move {
            loop {
                let available = self.inner.available.notified();
                {
                    let mut queue = self
                        .inner
                        .queue
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner);
                    if let Some(event) = queue.0.pop_front() {
                        return Some(event);
                    }
                    if queue.1 {
                        return None;
                    }
                }
                available.await;
            }
        })
    }
}

//! Many relays behind one handle: fan-out publish, deduped inbound, confirmed
//! sends, aggregated end-of-stored-events.
//!
//! Ported from the multi-relay half of `bitchat/Nostr/NostrRelayManager.swift`.
//! [`super::RelayClient`] owns one socket; everything that only makes sense across
//! several sockets lives here.
//!
//! # Why a dispatcher task
//!
//! Dedup state, EOSE trackers and send confirmations are touched on every inbound
//! frame from every relay. Sharing them behind a mutex would put a lock on the hot
//! path and invite the classic deadlock where a caller awaiting a confirmation
//! holds the lock the confirming frame needs. Instead one task owns all of it
//! exclusively and everything else talks to it over channels — no locks, no
//! ordering hazards, and the whole thing is drivable from a test with a mock
//! connector.
//!
//! # Publishing is confirmed, not fired
//!
//! [`RelayPool::publish`] is best-effort: it writes or queues, and tells you how
//! many relays it reached. [`RelayPool::publish_confirmed`] is the one to use when
//! success unlocks durable state or a delivery tick in the UI — it resolves `true`
//! only on `OK <id> true` from at least one relay, and `false` on rejection by all
//! of them, on losing every connection, or on timeout.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::error::NostrError;
use crate::types::NostrEvent;

use super::{
    DEFAULT_RELAYS, RelayClient, RelayClientConfig, RelayConnector, RelayEvent, RelayFilter,
    normalize_relay_url,
};

/// Inbound event IDs remembered before the oldest are evicted.
pub const INBOUND_DEDUP_CAP: usize = 4096;
/// How far the dedup set is trimmed back to when it overflows.
pub const INBOUND_DEDUP_TRIM_TARGET: usize = 3072;
/// How long a subscription waits for stored events before it is declared complete
/// regardless of what the relays did.
pub const EOSE_FALLBACK: Duration = Duration::from_secs(10);
/// How long a confirmed send waits for an `OK`.
pub const CONFIRMED_SEND_ACK_TIMEOUT: Duration = Duration::from_secs(10);

/// Returns `false` to drop an inbound event before it is deduped or forwarded.
///
/// **Wire signature verification here.** A relay can serve any bytes it likes,
/// including events attributed to a pubkey that never signed them; bitchat checks
/// `event.isValidSignature()` at exactly this point. This crate's transport layer
/// holds no key material and deliberately does not reach into the crypto module,
/// so the check is a parameter — but it is not optional, and the default (accept
/// everything) is correct only for a test.
///
/// Production wiring is one line:
///
/// ```ignore
/// verifier: Some(Arc::new(chatter_mesh_nostr::event::verify_event)),
/// ```
pub type Verifier = Arc<dyn Fn(&NostrEvent) -> bool + Send + Sync>;

/// Tunables for [`RelayPool`]. The defaults are bitchat's.
#[derive(Clone, Default)]
pub struct RelayPoolConfig {
    pub client: RelayClientConfig,
    pub dedup: DedupConfig,
    pub eose_fallback: Option<Duration>,
    pub confirm_timeout: Option<Duration>,
    /// See [`Verifier`]. `None` accepts every event that parses.
    pub verifier: Option<Verifier>,
}

impl RelayPoolConfig {
    fn eose_fallback(&self) -> Duration {
        self.eose_fallback.unwrap_or(EOSE_FALLBACK)
    }

    fn confirm_timeout(&self) -> Duration {
        self.confirm_timeout.unwrap_or(CONFIRMED_SEND_ACK_TIMEOUT)
    }
}

impl std::fmt::Debug for RelayPoolConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RelayPoolConfig")
            .field("client", &self.client)
            .field("dedup", &self.dedup)
            .field("eose_fallback", &self.eose_fallback)
            .field("confirm_timeout", &self.confirm_timeout)
            .field("verifier", &self.verifier.is_some())
            .finish()
    }
}

/// Bounds on the inbound dedup set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DedupConfig {
    pub cap: usize,
    pub trim_target: usize,
}

impl Default for DedupConfig {
    fn default() -> Self {
        Self {
            cap: INBOUND_DEDUP_CAP,
            trim_target: INBOUND_DEDUP_TRIM_TARGET,
        }
    }
}

/// Remembers which `(subscription, event)` pairs have already been delivered.
///
/// Keyed on the pair, not the event alone: two subscriptions asking for
/// overlapping things both want their copy, and collapsing them would silently
/// starve whichever one lost the race.
///
/// Bounded, and that bound is a security property rather than housekeeping — the
/// set grows on input a relay chooses, so an unbounded one is a memory exhaustion
/// primitive handed to anyone who can serve us events.
#[derive(Debug)]
pub struct InboundDedup {
    config: DedupConfig,
    seen: HashSet<(String, String)>,
    order: VecDeque<(String, String)>,
    duplicates: u64,
}

impl InboundDedup {
    pub fn new(config: DedupConfig) -> Self {
        Self {
            config,
            seen: HashSet::new(),
            order: VecDeque::new(),
            duplicates: 0,
        }
    }

    /// Whether this event should be delivered. Records it if so.
    ///
    /// An event with an empty ID is always admitted: there is nothing to key on,
    /// and inventing a key would let one such event suppress every other.
    pub fn admit(&mut self, subscription_id: &str, event_id: &str) -> bool {
        if event_id.is_empty() {
            return true;
        }
        let key = (subscription_id.to_string(), event_id.to_string());
        if !self.seen.insert(key.clone()) {
            self.duplicates = self.duplicates.saturating_add(1);
            return false;
        }
        self.order.push_back(key);

        if self.order.len() > self.config.cap {
            let remove = self.order.len().saturating_sub(self.config.trim_target);
            for _ in 0..remove {
                match self.order.pop_front() {
                    Some(stale) => {
                        self.seen.remove(&stale);
                    }
                    None => break,
                }
            }
        }
        true
    }

    /// Drops everything remembered for one subscription, so that re-subscribing
    /// with the same ID delivers its backlog again instead of silently swallowing
    /// it.
    pub fn forget_subscription(&mut self, subscription_id: &str) {
        self.order.retain(|(sub, _)| sub != subscription_id);
        self.seen.retain(|(sub, _)| sub != subscription_id);
    }

    pub fn len(&self) -> usize {
        self.order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    /// How many deliveries have been suppressed as duplicates.
    pub fn duplicates(&self) -> u64 {
        self.duplicates
    }
}

// --- Internal dispatcher plumbing ------------------------------------------

enum Ctl {
    TrackEose {
        subscription_id: String,
        urls: Vec<String>,
        epoch: u64,
    },
    EoseTimeout {
        subscription_id: String,
        epoch: u64,
    },
    ForgetSubscription {
        subscription_id: String,
    },
    TrackConfirm {
        event_id: String,
        urls: Vec<String>,
        token: u64,
        reply: oneshot::Sender<bool>,
    },
    ConfirmTimeout {
        event_id: String,
        token: u64,
    },
    Stop,
}

struct EoseTracker {
    epoch: u64,
    awaiting: HashSet<String>,
}

struct ConfirmTracker {
    token: u64,
    awaiting: HashSet<String>,
    reply: oneshot::Sender<bool>,
}

struct RelayEntry {
    client: Arc<RelayClient>,
    handle: JoinHandle<()>,
}

/// A set of relays, driven as one.
pub struct RelayPool {
    connector: Arc<dyn RelayConnector>,
    config: RelayPoolConfig,
    clients: Mutex<HashMap<String, RelayEntry>>,
    client_events: mpsc::UnboundedSender<RelayEvent>,
    ctl: mpsc::UnboundedSender<Ctl>,
    dispatcher: Mutex<Option<JoinHandle<()>>>,
    epochs: AtomicU64,
}

impl RelayPool {
    /// Builds a pool and returns the stream of everything it decides is worth
    /// surfacing: deduped events, connection transitions, notices, publish results.
    ///
    /// Must be called from inside a Tokio runtime — it spawns the dispatcher.
    pub fn new(
        connector: Arc<dyn RelayConnector>,
    ) -> (Arc<Self>, mpsc::UnboundedReceiver<RelayEvent>) {
        Self::with_config(connector, RelayPoolConfig::default())
    }

    pub fn with_config(
        connector: Arc<dyn RelayConnector>,
        config: RelayPoolConfig,
    ) -> (Arc<Self>, mpsc::UnboundedReceiver<RelayEvent>) {
        let (client_tx, client_rx) = mpsc::unbounded_channel();
        let (ctl_tx, ctl_rx) = mpsc::unbounded_channel();
        let (out_tx, out_rx) = mpsc::unbounded_channel();

        let dispatcher = Dispatcher {
            dedup: InboundDedup::new(config.dedup),
            verifier: config.verifier.clone(),
            eose: HashMap::new(),
            confirms: HashMap::new(),
            out: out_tx,
        };
        let handle = tokio::spawn(dispatcher.run(client_rx, ctl_rx));

        let pool = Arc::new(Self {
            connector,
            config,
            clients: Mutex::new(HashMap::new()),
            client_events: client_tx,
            ctl: ctl_tx,
            dispatcher: Mutex::new(Some(handle)),
            epochs: AtomicU64::new(0),
        });
        (pool, out_rx)
    }

    /// Adds any of `urls` we are not already handling and starts connecting.
    ///
    /// Returns the normalised URLs the pool will actually use; anything that is not
    /// a relay URL is dropped rather than failing the batch, because these lists
    /// come from a downloaded CSV where one bad row must not cost the other four
    /// hundred.
    pub async fn ensure_relays(&self, urls: &[String]) -> Vec<String> {
        let mut normalized = Vec::with_capacity(urls.len());
        let mut seen = HashSet::new();
        for raw in urls {
            if let Some(url) = normalize_relay_url(raw, Some("wss")) {
                if seen.insert(url.clone()) {
                    normalized.push(url);
                }
            }
        }

        let mut clients = self.clients.lock().await;
        for url in &normalized {
            if clients.contains_key(url) {
                continue;
            }
            let client = match RelayClient::new(
                url,
                Arc::clone(&self.connector),
                self.client_events.clone(),
                self.config.client.clone(),
            ) {
                Ok(client) => Arc::new(client),
                // Unreachable in practice: the URL already normalised above.
                Err(_) => continue,
            };
            let handle = tokio::spawn(Arc::clone(&client).run());
            clients.insert(url.clone(), RelayEntry { client, handle });
        }
        normalized
    }

    pub async fn relay_urls(&self) -> Vec<String> {
        let mut urls: Vec<String> = self.clients.lock().await.keys().cloned().collect();
        urls.sort();
        urls
    }

    pub async fn connected_relays(&self) -> Vec<String> {
        let clients: Vec<(String, Arc<RelayClient>)> = self
            .clients
            .lock()
            .await
            .iter()
            .map(|(url, entry)| (url.clone(), Arc::clone(&entry.client)))
            .collect();

        let mut connected = Vec::new();
        for (url, client) in clients {
            if client.is_connected().await {
                connected.push(url);
            }
        }
        connected.sort();
        connected
    }

    /// Subscribes on every target relay and starts tracking their EOSEs.
    ///
    /// An empty `urls` means the default DM relay set, matching bitchat's
    /// `relayUrls ?? defaultRelays`.
    ///
    /// Note for ephemeral kinds (20000/20001): relays store none of them, so a
    /// `since` in the past yields an immediate EOSE with nothing before it. That is
    /// the subscription working.
    pub async fn subscribe(
        &self,
        id: &str,
        filters: &[RelayFilter],
        urls: &[String],
    ) -> Result<(), NostrError> {
        if filters.is_empty() {
            return Err(NostrError::Relay(
                "a subscription needs at least one filter".to_string(),
            ));
        }
        let targets = self.ensure_relays(&self.targets_or_default(urls)).await;
        if targets.is_empty() {
            return Err(NostrError::Relay(
                "no usable relay URL in the target list".to_string(),
            ));
        }

        let epoch = self.epochs.fetch_add(1, Ordering::Relaxed);
        let _ = self.ctl.send(Ctl::TrackEose {
            subscription_id: id.to_string(),
            urls: targets.clone(),
            epoch,
        });
        self.schedule(
            self.config.eose_fallback(),
            Ctl::EoseTimeout {
                subscription_id: id.to_string(),
                epoch,
            },
        );

        for client in self.clients_for(&targets).await {
            client.subscribe(id, filters).await?;
        }
        Ok(())
    }

    /// Closes a subscription everywhere and forgets its dedup history.
    pub async fn unsubscribe(&self, id: &str) {
        let clients: Vec<Arc<RelayClient>> = self
            .clients
            .lock()
            .await
            .values()
            .map(|entry| Arc::clone(&entry.client))
            .collect();
        for client in clients {
            client.unsubscribe(id).await;
        }
        let _ = self.ctl.send(Ctl::ForgetSubscription {
            subscription_id: id.to_string(),
        });
    }

    /// Fans an event out to every target relay, queueing where a relay is down.
    ///
    /// Returns how many relays the event was handed to. **That is not a delivery
    /// count** — see [`Self::publish_confirmed`].
    pub async fn publish(&self, event: &NostrEvent, urls: &[String]) -> Result<usize, NostrError> {
        let targets = self.ensure_relays(&self.targets_or_default(urls)).await;
        let clients = self.clients_for(&targets).await;
        let mut count = 0;
        for client in clients {
            client.publish(event).await?;
            count += 1;
        }
        Ok(count)
    }

    /// Publishes and waits for a relay to say it kept the event.
    ///
    /// `true` on the first `OK <id> true`. `false` if every target rejected it, if
    /// every target's connection dropped first, if no target is connected at all,
    /// or if nothing answered inside the timeout. A `true` here is the only thing
    /// that justifies telling a user their message was sent.
    ///
    /// Only *currently connected* relays are targeted, exactly as
    /// `sendEventImmediately` does: an event parked in a pending queue may be sent
    /// minutes from now, and reporting on it would either block for those minutes
    /// or lie.
    pub async fn publish_confirmed(
        &self,
        event: &NostrEvent,
        urls: &[String],
    ) -> Result<bool, NostrError> {
        // Confirmation is keyed on the event ID. An unsigned draft has none, and two
        // concurrent sends would then answer each other's `OK`.
        if event.id.is_empty() {
            return Err(NostrError::InvalidEvent(
                "cannot confirm an event with no id",
            ));
        }
        let targets = self.ensure_relays(&self.targets_or_default(urls)).await;
        let connected: Vec<String> = {
            let all = self.connected_relays().await;
            let wanted: HashSet<&String> = targets.iter().collect();
            all.into_iter().filter(|url| wanted.contains(url)).collect()
        };
        if connected.is_empty() {
            return Ok(false);
        }

        let token = self.epochs.fetch_add(1, Ordering::Relaxed);
        let (reply_tx, reply_rx) = oneshot::channel();
        let _ = self.ctl.send(Ctl::TrackConfirm {
            event_id: event.id.clone(),
            urls: connected.clone(),
            token,
            reply: reply_tx,
        });
        self.schedule(
            self.config.confirm_timeout(),
            Ctl::ConfirmTimeout {
                event_id: event.id.clone(),
                token,
            },
        );

        for client in self.clients_for(&connected).await {
            client.publish(event).await?;
        }

        // The dispatcher always answers — on OK, on the last rejection, on the last
        // disconnect, or on the timeout it was just handed. A dropped sender can
        // only mean the pool is shutting down, which is not a delivery either.
        Ok(reply_rx.await.unwrap_or(false))
    }

    /// Stops every relay and the dispatcher.
    pub async fn shutdown(&self) {
        let entries: Vec<RelayEntry> = self.clients.lock().await.drain().map(|(_, e)| e).collect();
        for entry in entries {
            entry.client.shutdown().await;
            entry.handle.abort();
        }
        let _ = self.ctl.send(Ctl::Stop);
        if let Some(handle) = self.dispatcher.lock().await.take() {
            handle.abort();
        }
    }

    fn targets_or_default(&self, urls: &[String]) -> Vec<String> {
        if urls.is_empty() {
            DEFAULT_RELAYS.iter().map(|u| (*u).to_string()).collect()
        } else {
            urls.to_vec()
        }
    }

    async fn clients_for(&self, urls: &[String]) -> Vec<Arc<RelayClient>> {
        let clients = self.clients.lock().await;
        urls.iter()
            .filter_map(|url| clients.get(url).map(|entry| Arc::clone(&entry.client)))
            .collect()
    }

    /// Fires a control message after a delay. The dispatcher decides whether it is
    /// still relevant, which is why the message carries an epoch or a token.
    fn schedule(&self, delay: Duration, message: Ctl) {
        let ctl = self.ctl.clone();
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let _ = ctl.send(message);
        });
    }
}

impl std::fmt::Debug for RelayPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RelayPool").finish_non_exhaustive()
    }
}

// --- The dispatcher --------------------------------------------------------

struct Dispatcher {
    dedup: InboundDedup,
    verifier: Option<Verifier>,
    eose: HashMap<String, EoseTracker>,
    confirms: HashMap<String, ConfirmTracker>,
    out: mpsc::UnboundedSender<RelayEvent>,
}

impl Dispatcher {
    async fn run(
        mut self,
        mut client_rx: mpsc::UnboundedReceiver<RelayEvent>,
        mut ctl_rx: mpsc::UnboundedReceiver<Ctl>,
    ) {
        loop {
            tokio::select! {
                // Biased, and it matters: a caller registers its tracker on `ctl`
                // *before* the frame that resolves it can exist on the wire. FIFO
                // within each channel is not enough — a random pick between two
                // ready channels could take the relay's `OK` before the
                // registration it belongs to, and the send would time out instead
                // of confirming.
                biased;
                Some(message) = ctl_rx.recv() => {
                    if matches!(message, Ctl::Stop) {
                        return;
                    }
                    self.on_control(message);
                }
                Some(event) = client_rx.recv() => self.on_relay_event(event),
                else => return,
            }
        }
    }

    fn on_relay_event(&mut self, event: RelayEvent) {
        match event {
            RelayEvent::Event {
                url,
                subscription_id,
                event,
            } => {
                // Verify before dedup: an unverifiable event must not occupy a slot
                // in the dedup set, or a forger could evict real entries.
                if let Some(verifier) = &self.verifier {
                    if !verifier(&event) {
                        return;
                    }
                }
                if !self.dedup.admit(&subscription_id, &event.id) {
                    return;
                }
                let _ = self.out.send(RelayEvent::Event {
                    url,
                    subscription_id,
                    event,
                });
            }
            RelayEvent::Eose {
                url,
                subscription_id,
            } => {
                self.settle_eose(&subscription_id, &url);
                let _ = self.out.send(RelayEvent::Eose {
                    url,
                    subscription_id,
                });
            }
            RelayEvent::Ok {
                url,
                event_id,
                accepted,
                message,
            } => {
                self.settle_confirm(&event_id, &url, accepted);
                let _ = self.out.send(RelayEvent::Ok {
                    url,
                    event_id,
                    accepted,
                    message,
                });
            }
            RelayEvent::Closed {
                url,
                subscription_id,
                message,
            } => {
                // The relay will send no EOSE for a subscription it just closed.
                self.settle_eose(&subscription_id, &url);
                let _ = self.out.send(RelayEvent::Closed {
                    url,
                    subscription_id,
                    message,
                });
            }
            RelayEvent::Disconnected { url, reason } => {
                self.drop_relay(&url);
                let _ = self.out.send(RelayEvent::Disconnected { url, reason });
            }
            RelayEvent::GaveUp { url } => {
                self.drop_relay(&url);
                let _ = self.out.send(RelayEvent::GaveUp { url });
            }
            other => {
                let _ = self.out.send(other);
            }
        }
    }

    fn on_control(&mut self, message: Ctl) {
        match message {
            Ctl::TrackEose {
                subscription_id,
                urls,
                epoch,
            } => {
                self.eose.insert(
                    subscription_id,
                    EoseTracker {
                        epoch,
                        awaiting: urls.into_iter().collect(),
                    },
                );
            }
            Ctl::EoseTimeout {
                subscription_id,
                epoch,
            } => {
                // A relay that never answers must not leave a subscription hanging
                // forever in "still loading".
                if self
                    .eose
                    .get(&subscription_id)
                    .is_some_and(|tracker| tracker.epoch == epoch)
                {
                    self.eose.remove(&subscription_id);
                    self.complete_subscription(subscription_id);
                }
            }
            Ctl::ForgetSubscription { subscription_id } => {
                self.eose.remove(&subscription_id);
                self.dedup.forget_subscription(&subscription_id);
            }
            Ctl::TrackConfirm {
                event_id,
                urls,
                token,
                reply,
            } => {
                if let Some(replaced) = self.confirms.remove(&event_id) {
                    let _ = replaced.reply.send(false);
                }
                self.confirms.insert(
                    event_id,
                    ConfirmTracker {
                        token,
                        awaiting: urls.into_iter().collect(),
                        reply,
                    },
                );
            }
            Ctl::ConfirmTimeout { event_id, token } => {
                if self
                    .confirms
                    .get(&event_id)
                    .is_some_and(|tracker| tracker.token == token)
                {
                    if let Some(tracker) = self.confirms.remove(&event_id) {
                        let _ = tracker.reply.send(false);
                    }
                }
            }
            Ctl::Stop => {}
        }
    }

    fn settle_eose(&mut self, subscription_id: &str, url: &str) {
        let complete = match self.eose.get_mut(subscription_id) {
            None => return,
            Some(tracker) => {
                tracker.awaiting.remove(url);
                tracker.awaiting.is_empty()
            }
        };
        if complete {
            self.eose.remove(subscription_id);
            self.complete_subscription(subscription_id.to_string());
        }
    }

    fn settle_confirm(&mut self, event_id: &str, url: &str, accepted: bool) {
        // One acceptance is enough — the event is stored somewhere durable. A
        // rejection only settles the send once every target has rejected it.
        let resolved = match self.confirms.get_mut(event_id) {
            None => return,
            Some(tracker) => {
                if !tracker.awaiting.remove(url) {
                    return;
                }
                accepted || tracker.awaiting.is_empty()
            }
        };
        if resolved {
            if let Some(tracker) = self.confirms.remove(event_id) {
                let _ = tracker.reply.send(accepted);
            }
        }
    }

    /// A relay that dropped can answer nothing further: settle everything waiting
    /// on it rather than letting it stall a subscription or a send.
    fn drop_relay(&mut self, url: &str) {
        let stalled: Vec<String> = self
            .eose
            .iter()
            .filter(|(_, tracker)| tracker.awaiting.contains(url))
            .map(|(id, _)| id.clone())
            .collect();
        for id in stalled {
            self.settle_eose(&id, url);
        }

        let waiting: Vec<String> = self
            .confirms
            .iter()
            .filter(|(_, tracker)| tracker.awaiting.contains(url))
            .map(|(id, _)| id.clone())
            .collect();
        for id in waiting {
            self.settle_confirm(&id, url, false);
        }
    }

    fn complete_subscription(&self, subscription_id: String) {
        let _ = self
            .out
            .send(RelayEvent::SubscriptionComplete { subscription_id });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relay::mock::{MockRelay, Script, fast_config};
    use crate::relay::{BoxFuture, RelayConnection};

    // --- Dedup, pure ---

    fn dedup(cap: usize, trim: usize) -> InboundDedup {
        InboundDedup::new(DedupConfig {
            cap,
            trim_target: trim,
        })
    }

    #[test]
    fn dedup_admits_once_per_subscription() {
        let mut dedup = dedup(INBOUND_DEDUP_CAP, INBOUND_DEDUP_TRIM_TARGET);
        assert!(dedup.admit("sub-1", "ff"));
        assert!(!dedup.admit("sub-1", "ff"));
        // A different subscription genuinely wants its own copy.
        assert!(dedup.admit("sub-2", "ff"));
        assert!(dedup.admit("sub-1", "ee"));
        assert_eq!(dedup.duplicates(), 1);
        assert_eq!(dedup.len(), 3);
    }

    #[test]
    fn dedup_always_admits_an_empty_event_id() {
        let mut dedup = dedup(INBOUND_DEDUP_CAP, INBOUND_DEDUP_TRIM_TARGET);
        assert!(dedup.admit("sub-1", ""));
        assert!(dedup.admit("sub-1", ""));
        assert_eq!(dedup.len(), 0, "nothing to remember");
        assert_eq!(dedup.duplicates(), 0);
    }

    #[test]
    fn dedup_trims_to_the_target_when_it_overflows() {
        let mut dedup = dedup(INBOUND_DEDUP_CAP, INBOUND_DEDUP_TRIM_TARGET);
        for index in 0..INBOUND_DEDUP_CAP {
            assert!(dedup.admit("sub", &format!("{index}")));
        }
        assert_eq!(dedup.len(), INBOUND_DEDUP_CAP);

        // One past the cap trims back to the target — plus the entry that caused it.
        assert!(dedup.admit("sub", "overflow"));
        assert_eq!(dedup.len(), INBOUND_DEDUP_TRIM_TARGET);

        // The oldest were evicted, so they are admitted again; the newest are not.
        assert!(dedup.admit("sub", "0"));
        assert!(!dedup.admit("sub", &format!("{}", INBOUND_DEDUP_CAP - 1)));
        assert!(!dedup.admit("sub", "overflow"));
    }

    #[test]
    fn dedup_growth_is_bounded_under_a_flood() {
        // The DoS case: a relay streaming distinct event IDs forever.
        let mut dedup = dedup(64, 48);
        for index in 0..100_000 {
            dedup.admit("sub", &format!("{index}"));
            assert!(dedup.len() <= 64);
        }
        assert!(dedup.len() <= 64);
    }

    #[test]
    fn forgetting_a_subscription_only_drops_its_own_entries() {
        let mut dedup = dedup(INBOUND_DEDUP_CAP, INBOUND_DEDUP_TRIM_TARGET);
        dedup.admit("sub-1", "aa");
        dedup.admit("sub-1", "bb");
        dedup.admit("sub-2", "aa");

        dedup.forget_subscription("sub-1");
        assert_eq!(dedup.len(), 1);
        assert!(
            dedup.admit("sub-1", "aa"),
            "re-subscribing replays the backlog"
        );
        assert!(
            !dedup.admit("sub-2", "aa"),
            "the other subscription is untouched"
        );
    }

    // --- Pool, against mock relays ---

    struct Fleet {
        relays: HashMap<String, Arc<MockRelay>>,
    }

    impl Fleet {
        fn new(entries: Vec<(&str, Arc<MockRelay>)>) -> Arc<Self> {
            Arc::new(Self {
                relays: entries
                    .into_iter()
                    .map(|(url, relay)| (url.to_string(), relay))
                    .collect(),
            })
        }
    }

    impl RelayConnector for Fleet {
        fn connect<'a>(
            &'a self,
            url: &'a str,
        ) -> BoxFuture<'a, Result<RelayConnection, NostrError>> {
            match self.relays.get(url) {
                Some(relay) => {
                    let relay = Arc::clone(relay);
                    Box::pin(async move { relay.connect("wss://mock").await })
                }
                None => Box::pin(async move {
                    Err(NostrError::Relay("no such relay in the fleet".to_string()))
                }),
            }
        }
    }

    fn event(id: &str) -> NostrEvent {
        NostrEvent {
            id: id.to_string(),
            pubkey: "ab".repeat(32),
            created_at: 1_700_000_000,
            kind: 20000,
            tags: Vec::new(),
            content: "hi".to_string(),
            sig: Some("cd".repeat(64)),
        }
    }

    fn event_frame(sub: &str, id: &str) -> String {
        let value = serde_json::to_value(event(id)).expect("serialises");
        serde_json::Value::Array(vec![
            serde_json::Value::from("EVENT"),
            serde_json::Value::from(sub),
            value,
        ])
        .to_string()
    }

    fn pool_config() -> RelayPoolConfig {
        RelayPoolConfig {
            client: fast_config(),
            eose_fallback: Some(Duration::from_millis(200)),
            confirm_timeout: Some(Duration::from_millis(200)),
            ..RelayPoolConfig::default()
        }
    }

    async fn next(rx: &mut mpsc::UnboundedReceiver<RelayEvent>) -> RelayEvent {
        tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("an event arrives")
            .expect("channel open")
    }

    /// Waits until both relays report connected, discarding transitions.
    async fn await_connections(rx: &mut mpsc::UnboundedReceiver<RelayEvent>, count: usize) {
        let mut seen = 0;
        while seen < count {
            if let RelayEvent::Connected { .. } = next(rx).await {
                seen += 1;
            }
        }
    }

    fn two_relays() -> (Arc<MockRelay>, Arc<MockRelay>, Arc<Fleet>, Vec<String>) {
        let a = MockRelay::new(vec![Script::Serve(Vec::new())]);
        let b = MockRelay::new(vec![Script::Serve(Vec::new())]);
        let fleet = Fleet::new(vec![
            ("wss://a.test", Arc::clone(&a)),
            ("wss://b.test", Arc::clone(&b)),
        ]);
        let urls = vec!["wss://a.test".to_string(), "wss://b.test".to_string()];
        (a, b, fleet, urls)
    }

    #[tokio::test]
    async fn ensure_relays_normalises_and_deduplicates() {
        let (_, _, fleet, _) = two_relays();
        let (pool, _rx) = RelayPool::with_config(fleet, pool_config());

        let used = pool
            .ensure_relays(&[
                "wss://A.test:443/".to_string(),
                "wss://a.test".to_string(),
                "https://b.test".to_string(),
                "not a url".to_string(),
            ])
            .await;
        assert_eq!(used, vec!["wss://a.test", "wss://b.test"]);
        assert_eq!(
            pool.relay_urls().await,
            vec!["wss://a.test".to_string(), "wss://b.test".to_string()]
        );
        pool.shutdown().await;
    }

    #[tokio::test]
    async fn publish_fans_out_to_every_relay() {
        let (a, b, fleet, urls) = two_relays();
        let (pool, mut rx) = RelayPool::with_config(fleet, pool_config());
        pool.ensure_relays(&urls).await;
        await_connections(&mut rx, 2).await;

        assert_eq!(
            pool.publish(&event("ff"), &urls).await.expect("publishes"),
            2
        );
        tokio::time::sleep(Duration::from_millis(30)).await;

        for relay in [&a, &b] {
            let sent = relay.sent_frames();
            assert_eq!(sent.len(), 1, "{sent:?}");
            assert!(
                sent.first().is_some_and(|f| f.contains("\"ff\"")),
                "{sent:?}"
            );
        }
        pool.shutdown().await;
    }

    #[tokio::test]
    async fn the_same_event_from_two_relays_is_delivered_once() {
        let (a, b, fleet, urls) = two_relays();
        let (pool, mut rx) = RelayPool::with_config(fleet, pool_config());
        pool.subscribe("sub-1", &[RelayFilter::default()], &urls)
            .await
            .expect("subscribes");
        await_connections(&mut rx, 2).await;

        a.push(&event_frame("sub-1", "ff"));
        b.push(&event_frame("sub-1", "ff"));
        b.push(&event_frame("sub-1", "ee"));

        let mut ids = Vec::new();
        while ids.len() < 2 {
            if let RelayEvent::Event { event, .. } = next(&mut rx).await {
                ids.push(event.id);
            }
        }
        assert_eq!(ids, vec!["ff".to_string(), "ee".to_string()]);

        // Nothing further: the duplicate was swallowed rather than forwarded.
        let stray = tokio::time::timeout(Duration::from_millis(150), async {
            loop {
                match rx.recv().await {
                    Some(RelayEvent::Event { event, .. }) => return Some(event.id),
                    Some(_) => continue,
                    None => return None,
                }
            }
        })
        .await;
        assert!(
            stray.is_err(),
            "a duplicate reached the consumer: {stray:?}"
        );
        pool.shutdown().await;
    }

    #[tokio::test]
    async fn a_subscription_completes_once_every_relay_has_eosed() {
        let (a, b, fleet, urls) = two_relays();
        let (pool, mut rx) = RelayPool::with_config(fleet, pool_config());
        pool.subscribe("sub-1", &[RelayFilter::default()], &urls)
            .await
            .expect("subscribes");
        await_connections(&mut rx, 2).await;

        a.push(r#"["EOSE","sub-1"]"#);
        tokio::time::sleep(Duration::from_millis(20)).await;
        b.push(r#"["EOSE","sub-1"]"#);

        let mut completed = false;
        for _ in 0..6 {
            if let RelayEvent::SubscriptionComplete { subscription_id } = next(&mut rx).await {
                assert_eq!(subscription_id, "sub-1");
                completed = true;
                break;
            }
        }
        assert!(completed, "the subscription never completed");
        pool.shutdown().await;
    }

    #[tokio::test]
    async fn a_subscription_completes_on_the_fallback_when_a_relay_stays_silent() {
        // Ephemeral kinds produce no stored events, and a relay is free to answer
        // slowly or not at all. Neither may leave the UI loading forever.
        let (a, _b, fleet, urls) = two_relays();
        let (pool, mut rx) = RelayPool::with_config(fleet, pool_config());
        pool.subscribe("sub-1", &[RelayFilter::default()], &urls)
            .await
            .expect("subscribes");
        await_connections(&mut rx, 2).await;
        a.push(r#"["EOSE","sub-1"]"#);

        let mut completed = false;
        for _ in 0..8 {
            if let RelayEvent::SubscriptionComplete { .. } = next(&mut rx).await {
                completed = true;
                break;
            }
        }
        assert!(completed, "the fallback never fired");
        pool.shutdown().await;
    }

    #[tokio::test]
    async fn a_confirmed_send_succeeds_only_on_an_accepting_ok() {
        let (a, _b, fleet, urls) = two_relays();
        let (pool, mut rx) = RelayPool::with_config(fleet, pool_config());
        pool.ensure_relays(&urls).await;
        await_connections(&mut rx, 2).await;

        let pool2 = Arc::clone(&pool);
        let urls2 = urls.clone();
        let publish =
            tokio::spawn(async move { pool2.publish_confirmed(&event("ff"), &urls2).await });

        tokio::time::sleep(Duration::from_millis(20)).await;
        a.push(r#"["OK","ff",true,"stored"]"#);

        let confirmed = tokio::time::timeout(Duration::from_secs(2), publish)
            .await
            .expect("resolves")
            .expect("no panic")
            .expect("no error");
        assert!(confirmed);
        pool.shutdown().await;
    }

    #[tokio::test]
    async fn a_rejecting_ok_from_every_relay_fails_the_send() {
        // The bug this exists to prevent: a successful socket write followed by a
        // rejection, reported to the user as delivered.
        let (a, b, fleet, urls) = two_relays();
        let (pool, mut rx) = RelayPool::with_config(fleet, pool_config());
        pool.ensure_relays(&urls).await;
        await_connections(&mut rx, 2).await;

        let pool2 = Arc::clone(&pool);
        let urls2 = urls.clone();
        let publish =
            tokio::spawn(async move { pool2.publish_confirmed(&event("ff"), &urls2).await });

        tokio::time::sleep(Duration::from_millis(20)).await;
        a.push(r#"["OK","ff",false,"blocked: pow required"]"#);
        b.push(r#"["OK","ff",false,"rate-limited"]"#);

        let confirmed = tokio::time::timeout(Duration::from_secs(2), publish)
            .await
            .expect("resolves")
            .expect("no panic")
            .expect("no error");
        assert!(!confirmed, "every relay rejected it");
        pool.shutdown().await;
    }

    #[tokio::test]
    async fn one_acceptance_outweighs_a_rejection() {
        let (a, b, fleet, urls) = two_relays();
        let (pool, mut rx) = RelayPool::with_config(fleet, pool_config());
        pool.ensure_relays(&urls).await;
        await_connections(&mut rx, 2).await;

        let pool2 = Arc::clone(&pool);
        let urls2 = urls.clone();
        let publish =
            tokio::spawn(async move { pool2.publish_confirmed(&event("ff"), &urls2).await });

        tokio::time::sleep(Duration::from_millis(20)).await;
        a.push(r#"["OK","ff",false,"rate-limited"]"#);
        b.push(r#"["OK","ff",true,""]"#);

        assert!(
            tokio::time::timeout(Duration::from_secs(2), publish)
                .await
                .expect("resolves")
                .expect("no panic")
                .expect("no error")
        );
        pool.shutdown().await;
    }

    #[tokio::test]
    async fn a_confirmed_send_times_out_when_nothing_answers() {
        let (_a, _b, fleet, urls) = two_relays();
        let (pool, mut rx) = RelayPool::with_config(fleet, pool_config());
        pool.ensure_relays(&urls).await;
        await_connections(&mut rx, 2).await;

        let confirmed = pool
            .publish_confirmed(&event("ff"), &urls)
            .await
            .expect("no error");
        assert!(!confirmed, "silence is not delivery");
        pool.shutdown().await;
    }

    #[tokio::test]
    async fn a_confirmed_send_with_no_connected_relay_fails_immediately() {
        let fleet = Fleet::new(Vec::new());
        let (pool, _rx) = RelayPool::with_config(fleet, pool_config());
        let urls = vec!["wss://nowhere.test".to_string()];
        assert!(
            !pool
                .publish_confirmed(&event("ff"), &urls)
                .await
                .expect("no error")
        );
        pool.shutdown().await;
    }

    #[tokio::test]
    async fn a_confirmed_send_refuses_an_event_with_no_id() {
        let (_a, _b, fleet, urls) = two_relays();
        let (pool, mut rx) = RelayPool::with_config(fleet, pool_config());
        pool.ensure_relays(&urls).await;
        await_connections(&mut rx, 2).await;

        let unsigned = NostrEvent {
            id: String::new(),
            ..event("ignored")
        };
        assert!(pool.publish_confirmed(&unsigned, &urls).await.is_err());
        pool.shutdown().await;
    }

    #[tokio::test]
    async fn the_verifier_drops_events_before_they_are_deduped() {
        let (a, _b, fleet, urls) = two_relays();
        let config = RelayPoolConfig {
            verifier: Some(Arc::new(|event: &NostrEvent| event.id != "bad")),
            ..pool_config()
        };
        let (pool, mut rx) = RelayPool::with_config(fleet, config);
        pool.subscribe("sub-1", &[RelayFilter::default()], &urls)
            .await
            .expect("subscribes");
        await_connections(&mut rx, 2).await;

        a.push(&event_frame("sub-1", "bad"));
        a.push(&event_frame("sub-1", "good"));

        loop {
            if let RelayEvent::Event { event, .. } = next(&mut rx).await {
                assert_eq!(event.id, "good", "the unverified event was forwarded");
                break;
            }
        }
        pool.shutdown().await;
    }

    #[tokio::test]
    async fn unsubscribing_forgets_the_dedup_history() {
        let (a, _b, fleet, urls) = two_relays();
        let (pool, mut rx) = RelayPool::with_config(fleet, pool_config());
        pool.subscribe("sub-1", &[RelayFilter::default()], &urls)
            .await
            .expect("subscribes");
        await_connections(&mut rx, 2).await;

        a.push(&event_frame("sub-1", "ff"));
        loop {
            if let RelayEvent::Event { event, .. } = next(&mut rx).await {
                assert_eq!(event.id, "ff");
                break;
            }
        }

        pool.unsubscribe("sub-1").await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        pool.subscribe("sub-1", &[RelayFilter::default()], &urls)
            .await
            .expect("re-subscribes");
        a.push(&event_frame("sub-1", "ff"));

        loop {
            if let RelayEvent::Event { event, .. } = next(&mut rx).await {
                assert_eq!(
                    event.id, "ff",
                    "the backlog is replayed after re-subscribing"
                );
                break;
            }
        }
        pool.shutdown().await;
    }

    #[tokio::test]
    async fn subscribing_needs_a_filter_and_a_usable_relay() {
        let fleet = Fleet::new(Vec::new());
        let (pool, _rx) = RelayPool::with_config(fleet, pool_config());
        assert!(pool.subscribe("sub-1", &[], &[]).await.is_err());
        assert!(
            pool.subscribe(
                "sub-1",
                &[RelayFilter::default()],
                &["not a url".to_string()]
            )
            .await
            .is_err()
        );
        pool.shutdown().await;
    }
}

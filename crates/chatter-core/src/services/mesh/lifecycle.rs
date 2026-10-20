use super::*;

impl MeshService {
    /// Load the identity and wire the stack. Does not touch the radio or the
    /// network — [`start`](Self::start) does that.
    pub fn new(deps: MeshDeps, config: MeshConfig) -> Result<Self, MeshError> {
        let identity_store = Arc::clone(&deps.identity_store);
        let identity = Arc::new(deps.identity_store.get_or_create()?);
        let config = Arc::new(config);
        let state = Arc::new(MeshState::new(identity.peer_id()));
        let nostr_state = Arc::new(NostrState::new());
        let timeline = Arc::new(MeshTimeline::new(
            deps.messages,
            deps.conversations,
            deps.deliveries,
            deps.attachments,
        ));
        for channel in deps.private_channels.list()? {
            if channel.member_key(&identity.peer_id()).is_some() {
                timeline.open_private_channel(&channel.id, &channel.name);
            }
        }
        let events = MeshEvents::new(EVENT_CAPACITY, Arc::clone(&timeline));

        // The receivers for the first channel are dropped: nothing runs until
        // `start`, and `start` rebinds. A command sent before then goes nowhere,
        // which is what "not running" means.
        let (ble_bus, _) = CommandBus::new();
        let (nostr_bus, _) = CommandBus::new();

        let router = Arc::new(RouterHandle::spawn(RouterDeps {
            state: Arc::clone(&state),
            nostr_state: Arc::clone(&nostr_state),
            identity: Arc::clone(&identity),
            storage: Arc::clone(&deps.storage),
            sealer: Arc::clone(&deps.sealer),
            ble: Arc::clone(&ble_bus),
            nostr: Arc::clone(&nostr_bus),
            events: events.clone(),
            bridge_enabled: config.bridge_enabled,
        }));

        state.with(|inner| inner.duty_enabled = config.scan_duty_cycle);

        // Probed once, here, by sealing a byte. `Sealer` is a trait object so it
        // cannot be asked directly, and a probe covers every way a sealer can be
        // unable to hold a key rather than only the one this crate knows about.
        // Doing it at construction keeps `start()` cheap and means the reason is
        // available before the radio is ever touched.
        let sealer_unavailable = match deps.sealer.seal(b"probe") {
            Ok(_) => None,
            Err(error) => {
                tracing::warn!(
                    target: "mesh",
                    %error,
                    "the mesh cannot seal what it retains; it will not start"
                );
                Some(error.to_string())
            }
        };

        Ok(Self {
            key_store: deps.key_store,
            binding_store: deps.binding_store,
            private_channels: deps.private_channels,
            signal_lock: Arc::new(Mutex::new(())),
            identity,
            config,
            state,
            nostr_state,
            identity_store,
            storage: deps.storage,
            sealer: deps.sealer,
            router,
            ble: Arc::clone(&ble_bus),
            nostr: Arc::clone(&nostr_bus),
            events,
            timeline,
            has_radio: deps.radio_factory.is_some() || radio::platform_radio().is_some(),
            radio_factory: deps.radio_factory,
            relay_connector: deps.relay_connector,
            tasks: Mutex::new(Vec::new()),
            running: AtomicBool::new(false),
            identity_wiped: AtomicBool::new(false),
            sealer_unavailable,
        })
    }

    /// Why the mesh cannot seal what it retains, if it cannot.
    pub(super) fn sealer_unavailable_reason(&self) -> Option<String> {
        self.sealer_unavailable.clone()
    }

    /// Self-asserted name. No identity or account verification is implied.
    pub fn nickname(&self) -> String {
        self.config.nickname()
    }

    pub fn set_nickname(&self, nickname: String) {
        self.config.set_nickname(nickname);
        if self.is_running() {
            let _ = self.ble.send(BleCommand::Announce);
        }
    }

    /// Start the radio and the relay pool.
    ///
    /// Both halves are optional and independent: a build with no radio still
    /// carries geohash channels, and a client with no relay connector still
    /// carries the BLE mesh. Neither absence is an error.
    pub async fn start(&self) -> Result<(), MeshError> {
        // Checked before the radio is touched. A mesh whose sealer has no key
        // would advertise, connect, and then fail every retained write — a peer
        // list that looks healthy over a store that cannot keep anything. Better
        // to stay off and say which key is missing.
        if let Some(reason) = self.sealer_unavailable_reason() {
            self.running.store(false, Ordering::SeqCst);
            return Err(MeshError::SealerUnavailable(reason));
        }
        if self.identity_wiped.load(Ordering::SeqCst) {
            return Err(MeshError::IdentityWiped);
        }
        if self.running.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        // Fresh channels for fresh runtimes. Every holder — this service and the
        // router — sends through the bus, so none of them has to be told.
        let ble_rx = self.ble.rebind();
        let nostr_rx = self.nostr.rebind();

        let mut tasks = Vec::new();

        // Fresh per start, never reused: see `radio_override`.
        let radio = match self.radio_factory.as_ref() {
            Some(build) => Some(build()),
            None => radio::platform_radio(),
        };
        if let Some(radio) = radio {
            let runtime = BleRuntime::new(
                radio,
                Arc::clone(&self.state),
                Arc::clone(&self.nostr_state),
                Arc::clone(&self.identity),
                Arc::clone(&self.config),
                self.events.clone(),
                Arc::clone(&self.router),
                self.storage.clone(),
                Arc::clone(&self.sealer),
                Arc::clone(&self.binding_store),
                Arc::clone(&self.private_channels),
                Arc::clone(&self.key_store),
                Arc::clone(&self.signal_lock),
            );
            tasks.push(tokio::spawn(runtime.run(ble_rx)));
        } else {
            tracing::info!(
                target: "mesh",
                "no radio on this platform; the mesh runs over nostr only"
            );
        }

        if let Some(connector) = self.relay_connector.clone() {
            // `RelayPool::new` spawns its dispatcher, so it has to be built
            // inside the runtime rather than at construction time.
            let (pool, relay_events) = RelayPool::new(connector);
            let runtime = NostrRuntime::new(
                pool,
                Arc::clone(&self.nostr_state),
                Arc::clone(&self.state),
                NostrIdentities::from_mesh(&self.identity),
                Arc::clone(&self.config),
                self.events.clone(),
                Arc::clone(&self.router),
            );
            tasks.push(tokio::spawn(runtime.run(nostr_rx, relay_events)));
        } else {
            tracing::info!(
                target: "mesh",
                "no relay connector configured; geohash channels and gift-wrapped DMs are off"
            );
        }

        *self.tasks.lock() = tasks;
        Ok(())
    }

    /// Stop and join both transport runtimes; the router lives until service drop.
    pub async fn shutdown(&self) {
        if !self.running.swap(false, Ordering::SeqCst) {
            return;
        }
        let _ = self.ble.send(BleCommand::Stop);
        let _ = self.nostr.send(NostrCommand::Stop);

        let tasks: Vec<JoinHandle<()>> = std::mem::take(&mut *self.tasks.lock());
        for task in tasks {
            let _ = task.await;
        }
        // The router's channel is permanent. Keep its retained outbox actor
        // alive across a radio restart; RouterHandle::drop joins it at teardown.
    }

    // -------------------------------------------------------------- read paths

    /// Subscribe to everything the mesh reports.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<MeshEvent> {
        self.events.subscribe()
    }

    /// This device's mesh peer ID — `SHA256(static key)[..8]` as hex.
    #[must_use]
    pub fn local_peer(&self) -> PeerId {
        self.identity.peer_id()
    }

    /// This device's mesh fingerprint, for out-of-band verification.
    #[must_use]
    pub fn fingerprint(&self) -> String {
        self.identity.fingerprint()
    }

    /// Every peer the mesh currently knows, shaped for display.
    #[must_use]
    pub fn peers(&self) -> Vec<MeshPeer> {
        self.state.peers()
    }

    /// Peers still in radio range, shaped and ordered by the core peer registry.
    #[must_use]
    pub fn nearby_peers(&self) -> Vec<MeshPeer> {
        if !self.status().radio_running {
            return Vec::new();
        }
        self.state
            .peers()
            .into_iter()
            .filter(|peer| peer.reachable || peer.connected)
            .collect()
    }

    /// Resolve a message's local mesh sender id without consulting the server.
    #[must_use]
    pub fn peer_for_sender(&self, sender: &UserId) -> Option<MeshPeer> {
        self.state
            .peers()
            .into_iter()
            .find(|peer| mapping::peer_user_id(&peer.peer) == *sender)
    }
}

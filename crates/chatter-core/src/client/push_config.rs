//! Public push endpoint configuration and authenticated wake routing.
use super::*;

pub(super) fn configured_push_url() -> Option<String> {
    let runtime = std::env::var("CHATTER_NTFY_URL").ok();
    let baked = option_env!("CHATTER_NTFY_URL");
    if runtime.is_some() || baked.is_some() {
        return select_push_url(runtime, baked);
    }
    // Android must have wake delivery without depending on a process environment.
    // Reuse the shipped server pin; an explicit empty URL still disables push.
    #[cfg(target_os = "android")]
    {
        crate::config::PINNED_ONION_ADDR.map(|_| secret_push_url(""))
    }
    #[cfg(not(target_os = "android"))]
    {
        None
    }
}

fn select_push_url(runtime: Option<String>, baked: Option<&str>) -> Option<String> {
    runtime
        .or_else(|| baked.map(str::to_owned))
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

impl ChatterClient {
    pub(super) fn start_configured_push(self: &Arc<Self>) {
        // Release the URL lock before configure_push updates that same slot.
        let url = self.push_url.lock().clone();
        if let Some(url) = url {
            if let Err(error) = self.configure_push(&url) {
                tracing::warn!(%error, "configured push unavailable; inbox polling remains active");
            }
        }
    }

    pub(super) fn push_wakeup_allowed(&self, revision: u64) -> bool {
        self.auth_state().session_is_current(revision)
            && !self.anon_mode_enabled()
            && self.services.grpc_pool.transport_mode()
                == crate::network::grpc::client_pool::TransportMode::Direct
    }

    pub(super) async fn route_push_wakeup(
        &self,
        revision: u64,
        topic: &str,
        kind: crate::inbox::handlers::PushKind,
    ) -> bool {
        // Account teardown/login cannot cross a router's awaited inbox start.
        let phase = super::lifecycle_diagnostics::Phase::PushWakeup;
        phase.requested();
        let _lifecycle = self.auth_lifecycle_lock.lock().await;
        let _scope = phase.acquired();
        if !self.push_wakeup_allowed(revision) {
            return false;
        }
        let _ =
            self.services
                .event_bus
                .domain_sender()
                .send(crate::events::DomainEvent::PushWakeup {
                    topic: topic.to_owned(),
                    kind,
                });
        let _ = self.services.start_inbox_provider().await;
        if self.push_wakeup_allowed(revision) {
            if let Some(provider) = self.services.device_inbox_provider.get() {
                provider.wake();
            }
            // A content-free wake may be for a channel message. Backgrounded, the
            // channel stream is parked (Signal-mobile model), so nudge it to run
            // a one-shot catch-up now instead of waiting for foreground.
            self.services.channel_stream_service.request_resubscribe();
        }
        true
    }

    /// Enable UnifiedPush: persist the distributor endpoint, register it with the
    /// server (which then POSTs content-free wakes there), and let the push
    /// supervisor drop this device's own ntfy SSE socket on its next 2 s scan.
    pub async fn set_unified_push_endpoint(&self, endpoint: String) -> Result<(), crate::error::NetworkError> {
        use crate::storage::repositories::settings_repo::{SettingsRepository, keys};
        let settings = SettingsRepository::new(self.services.database.clone());
        let _ = settings.set_string(keys::UNIFIED_PUSH_ENDPOINT, &endpoint);
        let _ = settings.set_string(keys::UNIFIED_PUSH_ENABLED, "true");
        let mut auth = self.services.grpc_pool.auth_client();
        auth.register_push_endpoint(endpoint).await
    }

    /// Disable UnifiedPush: clear the server registration and the stored flag, so
    /// the supervisor restores this device's own ntfy SSE socket.
    pub async fn disable_unified_push(&self) -> Result<(), crate::error::NetworkError> {
        use crate::storage::repositories::settings_repo::{SettingsRepository, keys};
        let settings = SettingsRepository::new(self.services.database.clone());
        let _ = settings.set_string(keys::UNIFIED_PUSH_ENABLED, "false");
        let _ = settings.set_string(keys::UNIFIED_PUSH_ENDPOINT, "");
        let mut auth = self.services.grpc_pool.auth_client();
        auth.register_push_endpoint(String::new()).await
    }

    /// Whether UnifiedPush delivery is currently enabled on this device.
    pub fn unified_push_active(&self) -> bool {
        use crate::storage::repositories::settings_repo::{SettingsRepository, keys};
        SettingsRepository::new(self.services.database.clone())
            .get_string(keys::UNIFIED_PUSH_ENABLED)
            .ok()
            .flatten()
            .as_deref()
            == Some("true")
    }

    /// Handle a wake delivered by a UnifiedPush distributor: run the same
    /// content-free catch-up as an in-process SSE wake.
    pub async fn on_unified_push_message(self: &std::sync::Arc<Self>) {
        let revision = self.auth_state().get().session_revision;
        let _ = self
            .route_push_wakeup(revision, "unifiedpush", crate::inbox::handlers::PushKind::ContentFree)
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_push_endpoint_can_be_baked_for_envless_mobile_startup() {
        assert_eq!(
            select_push_url(None, Some("https://push.example.invalid:8096")),
            Some("https://push.example.invalid:8096".into())
        );
        assert_eq!(
            select_push_url(
                Some("https://override.invalid".into()),
                Some("https://baked.invalid")
            ),
            Some("https://override.invalid".into())
        );
        assert_eq!(
            select_push_url(Some(" ".into()), Some("https://baked.invalid")),
            None
        );
        assert_eq!(select_push_url(None, None), None);
    }

    fn client() -> (Arc<ChatterClient>, tempfile::TempDir) {
        // Push lifecycle fixtures exercise no OS credential-store behavior.
        static TEST_KEYRING: std::sync::Once = std::sync::Once::new();
        TEST_KEYRING.call_once(|| {
            keyring::set_default_store(
                crate::system::keyring::test_credential_store(),
            );
        });
        let dir = tempfile::tempdir().unwrap();
        let client = ChatterClient::builder()
            .server_url("https://localhost:1")
            .data_dir(dir.path().to_str().unwrap())
            .build()
            .unwrap();
        (client, dir)
    }

    #[tokio::test]
    async fn configured_push_defers_until_topic_exists_and_while_shield_is_active() {
        let (client, _dir) = client();
        *client.push_url.lock() = Some("https://push.example.invalid:8096".into());
        client
            .auth_state()
            .set_logged_in(crate::domain::ids::UserId::new(), 1, String::new());
        client.start_configured_push();
        assert!(
            client.push_shutdown.lock().is_none(),
            "no topic must leave polling only"
        );
        crate::storage::repositories::settings_repo::SettingsRepository::new(
            client.services.database.clone(),
        )
        .set_string(
            crate::storage::repositories::settings_repo::keys::PUSH_TOPIC,
            "fixture-topic",
        )
        .unwrap();
        client.anon_mode.store(true, Ordering::Release);
        client.start_configured_push();
        assert!(
            client.push_shutdown.lock().is_none(),
            "Shield must not open direct SSE"
        );
        assert!(
            client.push_url.lock().is_some(),
            "Shield exit can restore the desired URL"
        );
    }

    #[tokio::test]
    async fn stale_account_or_shield_push_cannot_emit_a_wakeup() {
        let (client, _dir) = client();
        let owner = crate::domain::ids::UserId::new();
        client.auth_state().set_logged_in(owner, 1, String::new());
        let revision = client.auth_state().get().session_revision;
        assert!(client.push_wakeup_allowed(revision));
        client.auth_state().set_logged_out();
        client.auth_state().set_logged_in(owner, 1, String::new());
        let mut events = client.services.event_bus.subscribe_domain();
        assert!(
            !client
                .route_push_wakeup(
                    revision,
                    "old-topic",
                    crate::inbox::handlers::PushKind::ContentFree
                )
                .await
        );
        assert!(events.try_recv().is_err());
        client.anon_mode.store(true, Ordering::Release);
        let current = client.auth_state().get().session_revision;
        assert!(
            !client
                .route_push_wakeup(
                    current,
                    "new-topic",
                    crate::inbox::handlers::PushKind::ContentFree
                )
                .await
        );
        assert!(events.try_recv().is_err());
    }

    #[tokio::test]
    async fn topic_ready_push_can_start_and_explicit_disable_is_remembered() {
        let (client, _dir) = client();
        // This current-thread test disables the listener before yielding, so
        // its direct transport never opens a socket.
        *client.push_url.lock() = Some("https://localhost:1".into());
        client
            .auth_state()
            .set_logged_in(crate::domain::ids::UserId::new(), 1, String::new());
        crate::storage::repositories::settings_repo::SettingsRepository::new(
            client.services.database.clone(),
        )
        .set_string(
            crate::storage::repositories::settings_repo::keys::PUSH_TOPIC,
            "fixture-topic",
        )
        .unwrap();
        client.start_configured_push();
        assert!(client.push_shutdown.lock().is_some());
        client.configure_push("").unwrap();
        assert!(client.push_shutdown.lock().is_none());
        assert!(client.push_url.lock().is_none());
        client.start_configured_push();
        assert!(client.push_shutdown.lock().is_none());
    }

    #[test]
    fn remembered_url_does_not_deadlock_on_shield_exit() {
        // A dedicated thread makes even a synchronous mutex regression fail
        // within a bounded deadline rather than hanging the async test suite.
        // The deadline times the reconfiguration only: building the client
        // (database open, migrations) took longer than it under a loaded
        // parallel suite.
        let (built, ready) = std::sync::mpsc::channel();
        let (done, result) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            tokio::runtime::Runtime::new().unwrap().block_on(async {
                let (client, _dir) = client();
                *client.push_url.lock() = Some("https://localhost:1".into());
                built.send(()).unwrap();
                // Signed out: restoration validates auth without dialing.
                client.rebuild_transport_loops().await;
                done.send(()).unwrap();
            });
        });
        ready.recv().expect("test client built");
        result
            .recv_timeout(std::time::Duration::from_secs(3))
            .expect("Shield exit held the remembered URL mutex during reconfiguration");
    }
}

impl ChatterClient {
    /// Listener tasks are children of the authenticated session. Mailbox subscriptions
    /// use Tor independently of Shield; no mailbox capability enters UI events.
    pub(super) fn start_mailbox_wakes(self: &Arc<Self>, url: String) {
        let client = Arc::downgrade(self);
        let revision = self.auth_state().get().session_revision;
        #[cfg(feature = "onion")]
        let tor = self.services.grpc_pool.onion_tor();
        let handle = tokio::spawn(async move {
            let mut listeners =
                std::collections::HashMap::<String, tokio::task::AbortHandle>::new();
            let mut tasks = tokio::task::JoinSet::new();
            let mut refresh_at = std::time::Instant::now();
            // Cached wake topics. They are written once per session (login/register,
            // or the identity refresh below for a linked device) and never rotate
            // mid-session — this supervisor restarts on a new session revision — so
            // once known we stop re-reading them from the encrypted settings store
            // every 2 s on every logged-in device.
            let mut device_topic = String::new();
            let mut push_topic = String::new();
            loop {
                let Some(owner) = client.upgrade() else { break };
                if !owner.auth_state().session_is_current(revision) {
                    break;
                }
                drop(owner);
                use crate::network::streaming::push_listener::PushSubscription;
                let mut active: Vec<(String, bool, PushSubscription)> =
                    crate::network::streaming::mailbox_wakes::active()
                        .into_iter()
                        .map(|topic| (topic, true, PushSubscription::Mailbox))
                        .collect();
                let Some(owner) = client.upgrade() else { break };
                if owner.push_wakeup_allowed(revision) {
                    use crate::storage::repositories::settings_repo::{SettingsRepository, keys};
                    // Only touch the encrypted settings store while a topic is still
                    // unknown. A linked device may learn both capabilities after the
                    // initial account listener configuration already returned, so keep
                    // reading until each is non-empty, then rely on the cache.
                    if device_topic.is_empty() || push_topic.is_empty() {
                        let settings = SettingsRepository::new(owner.services.database.clone());
                        if device_topic.is_empty() {
                            device_topic = settings
                                .get_string(keys::DEVICE_WAKE_TOPIC)
                                .ok()
                                .flatten()
                                .unwrap_or_default();
                        }
                        if push_topic.is_empty() {
                            push_topic = settings
                                .get_string(keys::PUSH_TOPIC)
                                .ok()
                                .flatten()
                                .unwrap_or_default();
                        }
                        if device_topic.is_empty() && std::time::Instant::now() >= refresh_at {
                            // Linked devices receive no AuthResponse. Fetch their own
                            // capabilities through authenticated identity, with session fencing.
                            refresh_at = std::time::Instant::now() + std::time::Duration::from_secs(60);
                            let phase = super::lifecycle_diagnostics::Phase::PushIdentityRefresh;
                            phase.requested();
                            let _guard = owner.auth_lifecycle_lock.lock().await;
                            let _scope = phase.acquired();
                            if !owner.auth_state().session_is_current(revision) {
                                break;
                            }
                            let mut auth = owner.services.grpc_pool.auth_client();
                            if let Ok(Ok(identity)) = tokio::time::timeout(
                                std::time::Duration::from_secs(15),
                                auth.get_my_identity(),
                            )
                            .await
                            {
                                let _ = settings
                                    .set_string(keys::DEVICE_WAKE_TOPIC, &identity.device_wake_topic);
                                let _ = settings.set_string(keys::PUSH_TOPIC, &identity.push_topic);
                                device_topic = identity.device_wake_topic;
                                push_topic = identity.push_topic;
                            }
                        }
                    }
                    // Carry both non-secret wake topics on ONE ntfy SSE socket
                    // (comma-joined multi-topic) rather than a socket each: the
                    // persistent connection is the cellular-radio cost, and
                    // route_push_wakeup runs the same content-free catch-up for
                    // either topic, so one socket loses nothing but a log label.
                    // Secret/mailbox topics keep their own Tor sockets below.
                    // Battery saver: when UnifiedPush is on, a distributor holds
                    // the socket and the server POSTs wakes to its registered
                    // endpoint, so this device keeps NO own ntfy SSE socket for
                    // its account/device topic. Read each 2 s scan, so toggling
                    // UnifiedPush drops or restores these listeners within one
                    // cycle; mailbox/secret Tor subscriptions are unaffected.
                    let unified_push = crate::storage::repositories::settings_repo::SettingsRepository::new(
                        owner.services.database.clone(),
                    )
                    .get_string(crate::storage::repositories::settings_repo::keys::UNIFIED_PUSH_ENABLED)
                    .ok()
                    .flatten()
                    .as_deref()
                        == Some("true");
                    let mut shared: Vec<&str> = Vec::new();
                    if !unified_push {
                        if !device_topic.is_empty() {
                            shared.push(&device_topic);
                        }
                        if owner.push_shutdown.lock().is_none() && !push_topic.is_empty() {
                            shared.push(&push_topic);
                        }
                    }
                    if !shared.is_empty() {
                        active.push((shared.join(","), false, PushSubscription::Account));
                    }
                }
                drop(owner);
                listeners.retain(|topic, task| {
                    if active.iter().any(|(active, _, _)| active == topic) && !task.is_finished() {
                        true
                    } else {
                        task.abort();
                        false
                    }
                });
                for (topic, secret, subscription) in active {
                    if listeners.contains_key(&topic) {
                        continue;
                    }
                    let endpoint = if secret {
                        secret_push_url(&url)
                    } else {
                        url.clone()
                    };
                    let mut config =
                        crate::network::streaming::PushListenerConfig::new(endpoint, topic.clone());
                    config.force_tor = secret;
                    config.subscription = subscription;
                    // A sleeping phone can lose its SSE socket. Retry promptly
                    // after network returns, with per-device jitter and the
                    // listener's bounded exponential backoff (including 429).
                    config.reconnect.jitter_ratio = 0.2;
                    let listener = crate::network::streaming::PushListener::new(config);
                    #[cfg(feature = "onion")]
                    let listener = listener.with_tor(tor.clone());
                    let listener = Arc::new(listener);
                    let mut wakes = listener.subscribe();
                    let owner = client.clone();
                    let route_topic = topic.clone();
                    let task=tasks.spawn(async move {
                        let (stop,shutdown)=tokio::sync::broadcast::channel(1);
                        // Cancellation drops this future and its Tor stream together.
                        let listen=listener.listen(shutdown);
                        tokio::pin!(listen);
                        loop { tokio::select! {
                            _=&mut listen => break,
                            event=wakes.recv() => {
                                let Ok(event)=event else { continue };
                                if !secret {
                                    let Some(owner)=owner.upgrade() else { break };
                                    if !owner.route_push_wakeup(revision,&route_topic,event.kind()).await { break }
                                    continue;
                                }
                                let Some(owner)=owner.upgrade() else { break };
                                let _guard=owner.auth_lifecycle_lock.lock().await;
                                if !owner.auth_state().session_is_current(revision) { break }
                                if let Some(provider)=owner.services.device_inbox_provider.get() { provider.wake(); }
                            }
                        } }
                        // Keep a sender alive across every await. Otherwise
                        // recv() reports Closed and the supervisor recreates
                        // this subscription on its next two-second scan.
                        drop(stop);
                    });
                    listeners.insert(topic, task);
                }
                tokio::select! {
                    _=tokio::time::sleep(std::time::Duration::from_secs(2)) => {},
                    _=tasks.join_next(), if !tasks.is_empty() => {},
                }
            }
            // JoinSet aborts every child on session teardown.
        });
        *self.mailbox_wake_task.lock() = Some(handle.abort_handle());
        self.tasks.lock().push(handle);
    }
}

fn secret_push_url(fallback: &str) -> String {
    secret_push_url_with_pin(fallback, crate::config::PINNED_ONION_ADDR)
}

fn secret_push_url_with_pin(fallback: &str, pin: Option<&str>) -> String {
    let Some(onion) = pin else {
        return fallback.to_owned();
    };
    let base = if onion.contains("://") {
        onion.to_owned()
    } else {
        format!("http://{onion}")
    };
    let Ok(mut url) = url::Url::parse(&base) else {
        return fallback.to_owned();
    };
    let _ = url.set_scheme("http");
    let _ = url.set_port(Some(8095));
    url.set_path("");
    url.set_query(None);
    url.set_fragment(None);
    url.to_string()
}

#[cfg(test)]
mod mailbox_subscription_tests {
    #[test]
    fn mailbox_endpoint_uses_pinned_onion_virtual_port() {
        let host = format!("{}.onion", "a".repeat(56));
        let expected = format!("http://{host}:8095/");
        assert_eq!(
            super::secret_push_url_with_pin("http://127.0.0.1:8095", Some(&host)),
            expected
        );
        assert_eq!(
            super::secret_push_url_with_pin(
                "https://push.invalid",
                Some(&format!("http://{host}:443"))
            ),
            expected
        );
        assert_eq!(
            super::secret_push_url_with_pin("https://push.invalid", None),
            "https://push.invalid"
        );
    }
}

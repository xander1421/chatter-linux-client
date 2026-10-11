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
        let _lifecycle = self.auth_lifecycle_lock.lock().await;
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
        }
        true
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
            loop {
                let Some(owner) = client.upgrade() else { break };
                if !owner.auth_state().session_is_current(revision) {
                    break;
                }
                drop(owner);
                let mut active: Vec<(String, bool)> =
                    crate::network::streaming::mailbox_wakes::active()
                        .into_iter()
                        .map(|topic| (topic, true))
                        .collect();
                let Some(owner) = client.upgrade() else { break };
                if owner.push_wakeup_allowed(revision) {
                    use crate::storage::repositories::settings_repo::{SettingsRepository, keys};
                    let settings = SettingsRepository::new(owner.services.database.clone());
                    if settings
                        .get_string(keys::DEVICE_WAKE_TOPIC)
                        .ok()
                        .flatten()
                        .unwrap_or_default()
                        .is_empty()
                        && std::time::Instant::now() >= refresh_at
                    {
                        // Linked devices receive no AuthResponse. Fetch their own
                        // capabilities through authenticated identity, with session fencing.
                        refresh_at = std::time::Instant::now() + std::time::Duration::from_secs(60);
                        let _guard = owner.auth_lifecycle_lock.lock().await;
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
                        }
                    }
                    if let Ok(Some(topic)) = settings.get_string(keys::DEVICE_WAKE_TOPIC) {
                        if !topic.is_empty() {
                            active.push((topic, false));
                        }
                    }
                    // A linked device may learn both capabilities after the
                    // initial account listener configuration already returned.
                    if owner.push_shutdown.lock().is_none() {
                        if let Ok(Some(topic)) = settings.get_string(keys::PUSH_TOPIC) {
                            if !topic.is_empty() {
                                active.push((topic, false));
                            }
                        }
                    }
                }
                drop(owner);
                listeners.retain(|topic, task| {
                    if active.iter().any(|(active, _)| active == topic) && !task.is_finished() {
                        true
                    } else {
                        task.abort();
                        false
                    }
                });
                for (topic, secret) in active {
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
                    let listener = crate::network::streaming::PushListener::new(config);
                    #[cfg(feature = "onion")]
                    let listener = listener.with_tor(tor.clone());
                    let listener = Arc::new(listener);
                    let mut wakes = listener.subscribe();
                    let owner = client.clone();
                    let route_topic = topic.clone();
                    let task=tasks.spawn(async move {
                        let (_stop,shutdown)=tokio::sync::broadcast::channel(1);
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

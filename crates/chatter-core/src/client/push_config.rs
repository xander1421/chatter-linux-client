//! Public push endpoint configuration and authenticated wake routing.
use super::*;

pub(super) fn configured_push_url() -> Option<String> {
    select_push_url(
        std::env::var("CHATTER_NTFY_URL").ok(),
        option_env!("CHATTER_NTFY_URL"),
    )
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
        let (done, result) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            tokio::runtime::Runtime::new().unwrap().block_on(async {
                let (client, _dir) = client();
                *client.push_url.lock() = Some("https://localhost:1".into());
                // Signed out: restoration validates auth without dialing.
                client.rebuild_transport_loops().await;
                done.send(()).unwrap();
            });
        });
        result
            .recv_timeout(std::time::Duration::from_secs(3))
            .expect("Shield exit held the remembered URL mutex during reconfiguration");
    }
}

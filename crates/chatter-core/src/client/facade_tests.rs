use super::*;

#[cfg(test)]
mod global_profile_refresh_tests {
    use crate::domain::anon_dm::AnonDmId;
    use crate::domain::anon_group::AnonGroupId;
    use crate::domain::ids::UserId;
    use crate::state::ActiveConversation;

    #[test]
    fn secret_chats_and_shield_never_refresh_the_global_profile() {
        let peer = UserId::new();
        let secret = [
            ActiveConversation::AnonDm(AnonDmId::new()),
            ActiveConversation::AnonGroup(AnonGroupId::new()),
        ];
        for conversation in &secret {
            assert!(!super::may_refresh_global_profile(false, conversation));
            assert!(!super::may_refresh_global_profile(true, conversation));
        }
        let dm = ActiveConversation::ServerDm { peer_id: peer };
        assert!(super::may_refresh_global_profile(false, &dm));
        assert!(!super::may_refresh_global_profile(true, &dm));
    }
}

#[cfg(test)]
mod queued_bubble_id_tests {
    /// The bubble of a DM queued behind the Shield bootstrap carries the
    /// stored message's id, the one `MessageDeliveryChanged` names.
    #[test]
    fn a_stored_queued_dm_keeps_its_real_id() {
        let id = crate::domain::ids::MessageId::new().to_string();
        assert_eq!(super::queued_bubble_id(id.clone(), 7), id);
        assert_eq!(super::queued_bubble_id(String::new(), 7), "pending-7");
    }
}

#[cfg(test)]
mod upload_attachments_tests {
    //! Regression test for the "must have text or attachments" mask: a
    //! failed pending-attachment upload must propagate its `AttachmentError`
    //! instead of being dropped, which previously left `pointers` empty and
    //! sent attachments-only messages into the empty-content validation path.
    use super::*;
    use crate::crypto::attachment_crypto::override_max_attachment_size;
    use crate::domain::AttachmentId;
    use crate::storage::database::Database;
    use crate::storage::repositories::AttachmentRepository;
    fn make_service() -> (AttachmentService, tempfile::TempDir) {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("upload_attachments_test.db");
        let db = Arc::new(Database::open(&db_path, "testpass").unwrap());
        db.run_migrations().unwrap();
        let repo = Arc::new(AttachmentRepository::new(db));
        (AttachmentService::new_local(repo), temp_dir)
    }

    fn pending_for(path: std::path::PathBuf, size: u64) -> PendingAttachment {
        PendingAttachment {
            id: AttachmentId::from_cdn_key("pending-test"),
            filename: "big.bin".to_string(),
            path,
            size,
            mime_type: "application/octet-stream".to_string(),
            pointer: None,
        }
    }

    #[tokio::test]
    async fn failed_upload_propagates_error_instead_of_dropping_pointer() {
        let _cap = override_max_attachment_size(100);

        let (svc, _tmp) = make_service();
        let payload = vec![7u8; 200];
        let input = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(input.path(), &payload).unwrap();

        let pending = vec![pending_for(
            input.path().to_path_buf(),
            payload.len() as u64,
        )];
        let result = upload_attachments(&pending, &svc, None).await;

        match result {
            Err(AttachmentError::EncryptionError(msg)) => {
                assert!(
                    msg.contains("attachment too large"),
                    "unexpected msg: {}",
                    msg
                );
            }
            other => panic!(
                "expected propagated AttachmentError::EncryptionError(AttachmentTooLarge), got {:?}",
                other.map(|u| u.pointers.len())
            ),
        }
    }
}

#[cfg(test)]
mod transport_switch_tests {
    //! GAP 2 bookkeeping: toggling anon mode must stop/re-spawn the long-lived
    //! loops so they re-dial over the new transport, and must be idempotent /
    //! race-safe (the flip runs on the UI thread). A full transport switch
    //! needs the `onion` feature + a live Tor server, so these assert the
    //! stop/restart bookkeeping directly instead.
    use super::*;

    fn build_test_client() -> (Arc<ChatterClient>, tempfile::TempDir) {
        // These tests build a real ChatterClient, which stores the DB passphrase
        // in the OS keyring. A headless/CI session has no Secret Service, so point
        // keyring at its in-memory mock backend (process-global, set once) — the
        // tests exercise transport-switch bookkeeping, not credential storage.
        static MOCK_KEYRING: std::sync::Once = std::sync::Once::new();
        MOCK_KEYRING.call_once(|| {
            keyring::set_default_store(crate::system::keyring::test_credential_store());
        });

        let dir = tempfile::tempdir().unwrap();
        let client = ChatterClient::builder()
            .server_url("https://localhost:1")
            .data_dir(dir.path().to_str().unwrap())
            .build()
            .expect("build client");
        // The directory must outlive the client: dropping it deletes the
        // database under a live connection.
        (client, dir)
    }

    /// Entering anon mode must fire the ntfy push shutdown and clear the slot
    /// (product decision: push OFF while anon, poll-only). Idempotent: a second
    /// enter with no listener is a clean no-op.
    #[tokio::test]
    async fn anon_enter_suspends_push_and_is_idempotent() {
        let (client, _dir) = build_test_client();

        // Simulate a configured push listener with anon mode enabled — the
        // rebuild reads the LIVE atomic (Fix 3), not a param, so set it first.
        client.anon_mode.store(true, Ordering::Release);
        let (tx, mut rx) = tokio::sync::broadcast::channel::<()>(1);
        *client.push_shutdown.lock() = Some(tx);
        *client.push_url.lock() = Some("http://push.example".into());

        client.rebuild_transport_loops().await;

        // Shutdown fired and the slot was taken.
        assert!(
            rx.try_recv().is_ok(),
            "push shutdown signal must fire on anon enter"
        );
        assert!(
            client.push_shutdown.lock().is_none(),
            "push slot must be cleared"
        );
        // The URL is remembered so exit can restore it.
        assert_eq!(
            client.push_url.lock().clone(),
            Some("http://push.example".into())
        );

        // Idempotent: second enter with no live listener does not panic.
        client.rebuild_transport_loops().await;
        assert!(client.push_shutdown.lock().is_none());
    }

    /// Fix 3 convergence: with the live atomic at `false` (final transport =
    /// direct), a rebuild must NOT suspend push even if invoked as part of a
    /// toggle burst — the push decision reads the authoritative atomic, not a
    /// stale captured param.
    #[tokio::test]
    async fn push_decision_follows_live_atomic_not_stale_param() {
        let (client, _dir) = build_test_client();
        client.anon_mode.store(false, Ordering::Release);
        let (tx, mut rx) = tokio::sync::broadcast::channel::<()>(1);
        *client.push_shutdown.lock() = Some(tx);

        client.rebuild_transport_loops().await;

        // anon=false → push must be left intact (no shutdown fired).
        assert!(
            rx.try_recv().is_err(),
            "push must not be suspended when live anon=false"
        );
        assert!(
            client.push_shutdown.lock().is_some(),
            "push slot must be preserved"
        );
    }

    /// Fix 1: `refresh_client` on the group fanout runs without deadlock and is
    /// idempotent under repeated invocation (rapid toggle). Uses the live pool.
    #[tokio::test]
    async fn group_fanout_refresh_is_idempotent() {
        let (client, _dir) = build_test_client();
        client.ctx.init_group_fanout();
        let fanout = client.ctx.get_group_fanout().expect("fanout initialized");

        // Repeated refresh (simulated rapid toggle) must not deadlock the
        // tokio Mutex or panic.
        for _ in 0..5 {
            fanout.refresh_client(&client.services.grpc_pool).await;
        }
    }

    /// Re-spawning the notification stream before login is a safe no-op — the
    /// stream hasn't started, so the "started" flag stays clear and no task is
    /// installed.
    #[tokio::test]
    async fn notification_respawn_is_noop_before_login() {
        let (client, _dir) = build_test_client();
        assert!(!client.authed_notif_started.load(Ordering::SeqCst));

        client.respawn_notification_stream();
        client.respawn_notification_stream();

        assert!(!client.authed_notif_started.load(Ordering::SeqCst));
        assert!(
            client.notif_stream_handle.lock().is_none(),
            "no notification stream task should be installed before login"
        );
    }

    /// Entering Shield hides channel rows straight from local state: the UI
    /// rebuilds from `sidebar_rows` before any network call, so the filter
    /// must not wait on the Tor link the flip just started (RC112: the
    /// channel row stayed listed until Tor connected).
    #[tokio::test]
    async fn shield_hides_channel_rows_without_network() {
        use crate::domain::channel::{Channel, ChannelType, MemberRole, Membership};
        use crate::storage::repositories::settings_repo::{SettingsRepository, keys};
        let (client, _dir) = build_test_client();
        let me = crate::domain::ids::UserId::new();
        SettingsRepository::new(client.services.database.clone())
            .set_string(keys::CURRENT_USER_ID, &me.to_string())
            .unwrap();
        let channel = Channel::new(
            crate::domain::ids::ChannelId::new(),
            "z2-channel".into(),
            ChannelType::Private,
            me,
        );
        let repo = crate::storage::repositories::channel_repo::ChannelRepository::new(
            client.services.database.clone(),
        );
        repo.insert(&channel).unwrap();
        repo.insert_membership(&Membership {
            channel_id: channel.id,
            user_id: me,
            role: MemberRole::Owner,
            joined_at: chrono::Utc::now(),
            last_read_message_id: None,
            is_muted: false,
            muted_until: None,
            notification_preference: Default::default(),
        })
        .unwrap();
        let id = channel.id.to_string();
        let listed = |c: &Arc<ChatterClient>| c.sidebar_rows().iter().any(|r| r.id == id);

        assert!(listed(&client), "a stored channel is listed outside Shield");
        client.set_anon_mode_enabled(true);
        assert!(
            !listed(&client),
            "Shield hides it at once, from local state"
        );
        client.set_anon_mode_enabled(false);
        assert!(listed(&client), "leaving Shield lists it again");
    }

    /// The public toggle must remain race-safe when hammered: the sync flag
    /// tracks the last value and the spawned rebuild tasks (no provider yet)
    /// converge without panicking or hanging the caller.
    #[tokio::test]
    async fn rapid_toggle_converges_without_hang() {
        let (client, _dir) = build_test_client();
        for _ in 0..10 {
            client.set_anon_mode_enabled(true);
            client.set_anon_mode_enabled(false);
        }
        assert!(!client.anon_mode_enabled());
        client.set_anon_mode_enabled(true);
        assert!(client.anon_mode_enabled());
        // Give the serialized rebuild tasks a chance to drain.
        tokio::task::yield_now().await;
    }

    /// Pre-login Shield fails closed when the build has nowhere to route over
    /// Tor: an error, and nothing changed — no Shield flag, still direct.
    #[tokio::test]
    async fn shield_before_login_refuses_without_onion() {
        use crate::network::grpc::client_pool::TransportMode;
        let (client, _dir) = build_test_client();
        client.services.grpc_pool.set_onion_address(None);

        assert!(!client.shield_available());
        assert!(client.set_shield_before_login(true).is_err());
        assert!(!client.anon_mode_enabled());
        assert_eq!(
            client.services.grpc_pool.transport_mode(),
            TransportMode::Direct
        );
    }

    #[tokio::test]
    async fn captured_state_respects_deferred_contact_then_explicit_failure() {
        use crate::network::connection::ConnectionState;
        let (client, _dir) = build_test_client();
        client.set_server_url("http://127.0.0.1:1").unwrap();
        assert!(
            !client
                .startup_connect(std::time::Duration::from_secs(1))
                .await
        );
        let captured = client.offline_state().connection_state();
        assert_eq!(captured, ConnectionState::Disconnected);
        assert!(!client.server_unreachable_for_state(captured));
        assert!(client.connect().await.is_err());
        let failed = client.offline_state().connection_state();
        assert!(client.server_unreachable_for_state(failed));
        client
            .offline_state()
            .set_connection_state(ConnectionState::Connected);
        assert!(
            client.server_unreachable_for_state(failed),
            "use captured failure, not a second state read"
        );
        assert!(!client.server_unreachable());
    }

    // Serialize these regressions because reachability is process-global.
    static AUTHORITY_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    async fn stop_authority_fixture(client: &ChatterClient) {
        // shutdown aborts the monitor before its best-effort presence RPC.
        // These fixtures deliberately have no live presence server; cancel
        // that unrelated RPC instead of hanging teardown on a lazy channel.
        let _ =
            tokio::time::timeout(std::time::Duration::from_millis(250), client.shutdown()).await;
    }

    fn pending_dial(
        pool: &crate::network::grpc::GrpcClientPool,
    ) -> (
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Sender<Result<bool, NetworkError>>,
    ) {
        let (started_tx, started) = tokio::sync::oneshot::channel();
        let (complete, completion) = tokio::sync::oneshot::channel();
        let channel = tonic::transport::Endpoint::from_static("https://localhost:1").connect_lazy();
        pool.control_next_dial(Box::pin(async move {
            let _ = started_tx.send(());
            let fresh = completion.await.expect("controlled dial released")?;
            Ok((channel.into(), fresh))
        }));
        (started, complete)
    }

    #[tokio::test]
    async fn connect_uses_new_rpc_evidence_when_pending_dial_finishes() {
        use crate::network::connection::{
            ConnectionState, ServerStatus, server_status, set_server_status,
        };
        use crate::network::grpc::transport::outcome_tests::authoritative_inbox;
        let _serial = AUTHORITY_TEST_LOCK.lock().await;
        // Both dial outcomes against both RPC outcomes. Application rejection
        // is a real response and must still count as reachable.
        for rpc_reachable in [true, false] {
            for dial_success in [true, false] {
                let (client, _dir) = build_test_client();
                set_server_status(ServerStatus::Unknown);
                let (started, complete) = pending_dial(&client.services.grpc_pool);
                let connecting = {
                    let client = client.clone();
                    tokio::spawn(async move { client.connect().await })
                };
                started.await.unwrap();
                let rpc = authoritative_inbox(Some(tonic::Code::Unavailable), !rpc_reachable).await;
                assert!(rpc.is_err());
                let expected = if rpc_reachable {
                    ServerStatus::Reachable
                } else {
                    ServerStatus::Unreachable
                };
                assert_eq!(server_status(), expected);
                complete
                    .send(if dial_success {
                        Ok(true)
                    } else {
                        Err(NetworkError::ConnectionFailed(
                            "controlled dial failure".into(),
                        ))
                    })
                    .unwrap();
                let result = connecting.await.unwrap();
                assert_eq!(result.is_ok(), dial_success && rpc_reachable);
                assert_eq!(
                    server_status(),
                    expected,
                    "stale dial replaced newer RPC evidence"
                );
                assert_eq!(
                    client.offline_state().connection_state(),
                    if rpc_reachable {
                        ConnectionState::Connected
                    } else {
                        ConnectionState::Disconnected
                    }
                );
                stop_authority_fixture(&client).await;
            }
        }
    }

    #[tokio::test]
    async fn connection_generation_swap_never_installs_or_publishes_old_dial() {
        use crate::network::connection::{
            ConnectionState, ServerStatus, server_status, set_server_status,
        };
        use crate::network::grpc::transport::outcome_tests::authoritative_inbox;
        let _serial = AUTHORITY_TEST_LOCK.lock().await;
        for dial_success in [true, false] {
            let (client, _dir) = build_test_client();
            set_server_status(ServerStatus::Unknown);
            let (started, complete) = pending_dial(&client.services.grpc_pool);
            let connecting = {
                let client = client.clone();
                tokio::spawn(async move { client.connect().await })
            };
            started.await.unwrap();
            client.set_server_url("https://new-server.invalid").unwrap();
            authoritative_inbox(None, false).await.unwrap();
            complete
                .send(if dial_success {
                    Ok(true)
                } else {
                    Err(NetworkError::ConnectionFailed(
                        "old transport failed".into(),
                    ))
                })
                .unwrap();
            assert!(connecting.await.unwrap().is_err());
            assert!(!client.services.grpc_pool.is_connected());
            assert_eq!(server_status(), ServerStatus::Reachable);
            assert_eq!(
                client.offline_state().connection_state(),
                ConnectionState::Connected
            );
            stop_authority_fixture(&client).await;
        }
    }

    #[tokio::test]
    async fn cached_channel_connect_returns_only_confirmed_authority() {
        use crate::network::connection::{
            ServerStatus, server_evidence_revision, server_status, set_server_status,
        };
        let _serial = AUTHORITY_TEST_LOCK.lock().await;
        let (client, _dir) = build_test_client();
        for status in [
            ServerStatus::Unknown,
            ServerStatus::Unreachable,
            ServerStatus::Reachable,
        ] {
            set_server_status(status);
            let revision = server_evidence_revision();
            let (started, complete) = pending_dial(&client.services.grpc_pool);
            complete.send(Ok(false)).unwrap();
            assert_eq!(
                client.connect().await.is_ok(),
                status == ServerStatus::Reachable
            );
            started.await.unwrap();
            assert_eq!(server_status(), status);
            assert_eq!(
                server_evidence_revision(),
                revision,
                "local construction fabricated evidence"
            );
        }
        stop_authority_fixture(&client).await;
    }

    #[tokio::test]
    async fn startup_timeout_is_bounded_and_fenced_by_rpc_and_generation() {
        use crate::network::connection::{
            ConnectionState, ServerStatus, server_status, set_server_status,
        };
        use crate::network::grpc::transport::outcome_tests::authoritative_inbox;
        let _serial = AUTHORITY_TEST_LOCK.lock().await;
        for scenario in 0..3 {
            let (client, _dir) = build_test_client();
            let user = crate::domain::ids::UserId::new();
            client
                .sessions()
                .set_string_setting(
                    crate::storage::repositories::settings_repo::keys::CURRENT_USER_ID,
                    &user.to_string(),
                )
                .unwrap();
            // A persisted session is a user id AND a device token (ec677fb7).
            crate::storage::repositories::settings_repo::SettingsRepository::new(
                client.services.database.clone(),
            )
            .set_bytes(
                crate::storage::repositories::settings_repo::keys::DEVICE_AUTH_TOKEN,
                b"token",
            )
            .unwrap();
            client.auth_state().set_logged_in(user, 1, "Me".into());
            assert!(
                client.is_logged_in(),
                "fixture must enter authenticated startup"
            );
            set_server_status(ServerStatus::Unknown);
            let (started, complete) = pending_dial(&client.services.grpc_pool);
            let boot = {
                let client = client.clone();
                tokio::spawn(async move {
                    client
                        .startup_connect(std::time::Duration::from_millis(50))
                        .await
                })
            };
            tokio::time::timeout(std::time::Duration::from_secs(1), started)
                .await
                .expect("authenticated startup must reach the controlled dial")
                .unwrap();
            if scenario == 1 {
                authoritative_inbox(None, false).await.unwrap();
            }
            if scenario == 2 {
                client.set_server_url("https://new-server.invalid").unwrap();
            }
            assert_eq!(
                tokio::time::timeout(std::time::Duration::from_secs(2), boot)
                    .await
                    .unwrap()
                    .unwrap(),
                scenario == 1
            );
            assert!(
                complete.send(Ok(true)).is_err(),
                "timed-out dial was not cancelled"
            );
            assert!(!client.services.grpc_pool.is_connected());
            let expected = match scenario {
                0 => ServerStatus::Unreachable,
                1 => ServerStatus::Reachable,
                _ => ServerStatus::Unknown,
            };
            assert_eq!(server_status(), expected);
            if scenario == 1 {
                assert_eq!(
                    client.offline_state().connection_state(),
                    ConnectionState::Connected
                );
            }
            stop_authority_fixture(&client).await;
        }
    }

    #[tokio::test]
    async fn monitor_mirrors_rpc_edges_while_production_probe_is_pending() {
        use crate::network::connection::{
            ConnectionState, ServerStatus, server_status, set_server_status,
        };
        use crate::network::grpc::transport::outcome_tests::authoritative_inbox;
        let _serial = AUTHORITY_TEST_LOCK.lock().await;
        let (client, _dir) = build_test_client();
        set_server_status(ServerStatus::Unknown);
        let (started, complete) = pending_dial(&client.services.grpc_pool);
        let monitor = tokio::spawn(crate::network::server_monitor::run_with_timing(
            client.services.grpc_pool.clone(),
            client.offline_state().clone(),
            std::time::Duration::ZERO,
            std::time::Duration::from_millis(1),
        ));
        started.await.unwrap();
        for reachable in [true, false, true] {
            let _ = authoritative_inbox(None, !reachable).await;
            let expected = if reachable {
                ConnectionState::Connected
            } else {
                ConnectionState::Disconnected
            };
            tokio::time::timeout(std::time::Duration::from_secs(1), async {
                while client.offline_state().connection_state() != expected {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("monitor blocked behind pending probe");
            assert!(
                !complete.is_closed(),
                "probe settled before RPC edge was mirrored"
            );
        }
        let (second_started, second_complete) = pending_dial(&client.services.grpc_pool);
        complete
            .send(Err(NetworkError::ConnectionFailed(
                "stale probe failed".into(),
            )))
            .unwrap();
        // A second controlled probe can start only after the first has returned
        // through generation/revision validation and the monitor mirrored it.
        tokio::time::timeout(std::time::Duration::from_secs(1), second_started)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(server_status(), ServerStatus::Reachable);
        monitor.abort();
        let _ = monitor.await;
        assert!(second_complete.is_closed());
        stop_authority_fixture(&client).await;
    }

    /// Signed-out startup contacts nothing: no boot connect, no reachability
    /// monitor, no "unreachable" claim, until a sign-in action connects — so
    /// a user who then picks Shield has leaked nothing. The first `connect`
    /// starts the monitor.
    #[cfg(feature = "onion")]
    #[tokio::test]
    async fn signed_out_startup_contacts_nothing_until_a_sign_in_action() {
        use std::sync::atomic::AtomicUsize;
        use std::time::Duration;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let direct_url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let accepts = Arc::new(AtomicUsize::new(0));
        let counter = accepts.clone();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((sock, _)) = listener.accept().await {
                counter.fetch_add(1, Ordering::SeqCst);
                held.push(sock);
            }
        });

        let (client, _dir) = build_test_client();
        client.set_server_url(&direct_url).unwrap();
        assert!(!client.is_logged_in());

        let started = std::time::Instant::now();
        assert!(!client.startup_connect(Duration::from_secs(5)).await);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "signed-out startup must not dial"
        );
        assert!(
            !client.server_unreachable(),
            "no contact yet: unreachable would be a guess"
        );
        assert!(
            !client.server_monitor_started.load(Ordering::SeqCst),
            "monitor must wait"
        );
        tokio::time::sleep(Duration::from_secs(6)).await; // past one monitor tick
        assert_eq!(
            accepts.load(Ordering::SeqCst),
            0,
            "signed-out startup dialled the server"
        );

        // The user picks Shield, then signs up: still nothing direct.
        client
            .services
            .grpc_pool
            .set_onion_address(Some(format!("http://{}.onion:443", "a".repeat(56))));
        client.set_shield_before_login(true).unwrap();
        let _ = tokio::time::timeout(
            Duration::from_secs(5),
            client.register(
                "deferred",
                "pw-123456789012",
                "D",
                "dev",
                None,
                Some("correct-horse-9"),
            ),
        )
        .await;
        assert!(
            client.server_monitor_started.load(Ordering::SeqCst),
            "first contact starts the monitor"
        );
        tokio::time::sleep(Duration::from_secs(6)).await; // monitor never probes on onion
        assert_eq!(
            accepts.load(Ordering::SeqCst),
            0,
            "Shield sign-up after a deferred start dialled direct"
        );

        // Control: Shield off, a direct connect does reach the listener.
        client.set_shield_before_login(false).unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(5), client.connect()).await;
        for _ in 0..20 {
            if accepts.load(Ordering::SeqCst) > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(
            accepts.load(Ordering::SeqCst) > 0,
            "listener should see the direct dial"
        );
    }

    /// The owner requirement: with Shield chosen before sign-in, onboarding
    /// never dials the server directly. The "direct server" here is a local
    /// listener counting TCP accepts; the pinned .onion is fake, so the onion
    /// attempt can only fail or time out. Registration, sign-in and a direct
    /// URL change must all produce zero direct connections and never succeed.
    /// Turning Shield off afterwards does dial the listener — proof the
    /// counter would have caught a leak.
    #[cfg(feature = "onion")]
    #[tokio::test]
    async fn shield_before_login_never_dials_direct() {
        use crate::network::grpc::client_pool::TransportMode;
        use std::sync::atomic::AtomicUsize;
        use std::time::Duration;

        static MOCK_KEYRING: std::sync::Once = std::sync::Once::new();
        MOCK_KEYRING.call_once(|| {
            keyring::set_default_store(crate::system::keyring::test_credential_store());
        });

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let direct_url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let accepts = Arc::new(AtomicUsize::new(0));
        let counter = accepts.clone();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((sock, _)) = listener.accept().await {
                counter.fetch_add(1, Ordering::SeqCst);
                held.push(sock);
            }
        });

        // Built on the placeholder URL every test uses: the builder publishes
        // its URL in a process-wide env var (`app/context.rs`), so building on
        // the listener's URL would point other tests' clients at it. The URL
        // goes to this client's pool only.
        let dir = tempfile::tempdir().unwrap();
        let client = ChatterClient::builder()
            .server_url("https://localhost:1")
            .data_dir(dir.path().to_str().unwrap())
            .build()
            .expect("build client");
        client.set_server_url(&direct_url).unwrap();
        let fake_onion = format!("http://{}.onion:443", "a".repeat(56));
        client
            .services
            .grpc_pool
            .set_onion_address(Some(fake_onion));

        assert!(client.shield_available());
        client
            .set_shield_before_login(true)
            .expect("Shield before login");
        assert!(
            client.anon_mode_enabled(),
            "the session must come up in Shield"
        );
        assert_eq!(
            client.services.grpc_pool.transport_mode(),
            TransportMode::Onion
        );

        // The register form always re-applies its Server URL field.
        client.set_server_url(&direct_url).unwrap();
        assert!(
            client.endpoint().contains(".onion"),
            "a URL change must not leave onion"
        );

        let budget = Duration::from_secs(5);
        let registered = tokio::time::timeout(
            budget,
            client.register(
                "shieldtest",
                "pw-123456789012",
                "S",
                "dev",
                None,
                Some("correct-horse-9"),
            ),
        )
        .await;
        assert!(
            !matches!(registered, Ok(Ok(_))),
            "fake .onion cannot register"
        );
        let logged_in = tokio::time::timeout(
            budget,
            client.login("shieldtest", "pw-123456789012", "dev", None),
        )
        .await;
        assert!(
            !matches!(logged_in, Ok(Ok(_))),
            "fake .onion cannot sign in"
        );
        let connected = tokio::time::timeout(budget, client.connect()).await;
        assert!(!matches!(connected, Ok(Ok(()))));
        // Storage transfers (link-device archive) ride Tor too, never direct.
        let uploaded = tokio::time::timeout(
            budget,
            client
                .message_backup()
                .upload_link_archive(b"archive", &format!("{direct_url}/blob")),
        )
        .await;
        assert!(!matches!(uploaded, Ok(Ok(()))));

        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            accepts.load(Ordering::SeqCst),
            0,
            "Shield onboarding dialled the server directly"
        );
        assert!(
            client.anon_mode_enabled(),
            "a failed onion dial must not drop Shield"
        );
        assert_eq!(
            client.services.grpc_pool.transport_mode(),
            TransportMode::Onion
        );

        // Sanity: the counter sees a direct dial once Shield is off.
        client.set_shield_before_login(false).unwrap();
        let _ = tokio::time::timeout(budget, client.connect()).await;
        for _ in 0..20 {
            if accepts.load(Ordering::SeqCst) > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(
            accepts.load(Ordering::SeqCst) > 0,
            "listener should see the direct dial"
        );
    }
}

#[cfg(test)]
mod unread_badge_tests {
    //! Badges are absolute counts from storage, filtered by the current mode.
    use std::sync::atomic::Ordering;

    use super::*;
    use crate::domain::ids::{ChannelId, UserId};
    use crate::domain::message::{ConversationType, Message};
    use crate::state::ActiveConversation;

    /// The `TempDir` must outlive the client: the database's reader pool
    /// opens connections lazily BY PATH (`ReaderPool::open_reader`), so once
    /// the directory is deleted any read that needs a fresh reader fails, and
    /// `server_dm_timer` swallows a failed read as "timer 0".
    fn build_test_client() -> (Arc<ChatterClient>, tempfile::TempDir) {
        static MOCK_KEYRING: std::sync::Once = std::sync::Once::new();
        MOCK_KEYRING.call_once(|| {
            keyring::set_default_store(crate::system::keyring::test_credential_store());
        });
        let dir = tempfile::tempdir().unwrap();
        let client = ChatterClient::builder()
            .server_url("https://localhost:1")
            .data_dir(dir.path().to_str().unwrap())
            .build()
            .expect("build client");
        (client, dir)
    }

    fn receive(client: &ChatterClient, id: ChannelId, kind: ConversationType, sent_at: u64) {
        let msg = Message::new_text(id, kind, Some(UserId::new()), "hi".into(), sent_at);
        client.messages().store_message_local(&msg).expect("store");
    }

    /// Make `peer` an accepted friend of a logged-in `me`: the one kind of
    /// server DM the sidebar gives a row (`listed_friends`).
    fn befriend(client: &ChatterClient, peer: UserId) {
        let me = UserId::new();
        client.auth_state().set_logged_in(me, 1, "Me".into());
        client
            .messages()
            .settings_repo
            .set_string(
                crate::storage::repositories::settings_repo::keys::CURRENT_USER_ID,
                &me.to_string(),
            )
            .unwrap();
        let users = &client.messages().user_repo;
        users.ensure_user_exists(&me).unwrap();
        users.ensure_user_exists(&peer).unwrap();
        let mut friendship = crate::domain::user::Friendship::new_request(me, peer);
        friendship.accept();
        users.insert_friendship(&friendship).unwrap();
    }

    #[tokio::test]
    async fn badges_are_absolute_and_follow_the_mode() {
        let (client, _dir) = build_test_client();
        let peer = UserId::new();
        befriend(&client, peer);
        let mut anon_dm = crate::domain::anon_dm::AnonDm::new_initiated(UserId::new());
        anon_dm.status = crate::domain::anon_dm::AnonDmStatus::Active;
        client
            .anon_dms()
            .anon_dm_repo()
            .insert_conversation(&anon_dm)
            .unwrap();
        let anon = anon_dm.id;
        receive(
            &client,
            ChannelId::from(peer),
            ConversationType::ServerDm,
            1,
        );
        receive(
            &client,
            ChannelId::from(peer),
            ConversationType::ServerDm,
            2,
        );
        receive(&client, anon.into(), ConversationType::AnonDm, 3);

        let dm = ActiveConversation::ServerDm { peer_id: peer };
        // Two unread on the DM row; the anon DM is a secret chat, a row of
        // its own in both modes, so the total is 2.
        assert_eq!(client.unread_badges(&dm), UnreadBadges { row: 2, total: 2 });

        // Shield on: the same two rows (friend DMs and secret chats both show
        // in both modes — `visible_in_mode`), so the total is 2.
        client.anon_mode.store(true, Ordering::Release);
        assert_eq!(
            client.unread_badges(&ActiveConversation::AnonDm(anon)),
            UnreadBadges { row: 1, total: 2 },
        );
        assert_eq!(client.unread_conversation_count(), 2);
    }

    /// F73: the Chats total counts only conversations the list shows as a
    /// row, and call records never count.
    #[tokio::test]
    async fn rowless_conversations_and_call_records_never_badge() {
        use crate::domain::message::{CallMessage, CallMessageType, MessageContent};
        let (client, _dir) = build_test_client();
        let friend = UserId::new();
        befriend(&client, friend);

        // A DM with someone who is not a friend (unfriended, or never one)
        // and a channel we are not in: neither has a row.
        receive(
            &client,
            ChannelId::from(UserId::new()),
            ConversationType::ServerDm,
            1,
        );
        receive(&client, ChannelId::new(), ConversationType::Channel, 2);
        assert_eq!(client.unread_conversation_count(), 0);

        // A missed call filed in the friend's DM is history, not unread.
        let mut call = Message::new_text(
            ChannelId::from(friend),
            ConversationType::ServerDm,
            Some(friend),
            String::new(),
            3,
        );
        call.content = MessageContent::Call(CallMessage {
            call_id: "c".into(),
            call_type: CallMessageType::Missed,
            initiator_id: friend,
            initiator_name: String::new(),
            started_at: None,
            ended_at: None,
            duration: None,
            participant_count: 2,
        });
        client.messages().store_message_local(&call).expect("store");
        assert_eq!(client.unread_conversation_count(), 0);

        receive(
            &client,
            ChannelId::from(friend),
            ConversationType::ServerDm,
            4,
        );
        assert_eq!(client.unread_conversation_count(), 1);
    }

    /// A lost timer update healed by the next visible message: the gateway
    /// adopts the new timer with no event bus, so the subscriber notices the
    /// changed value on that message and the header refreshes. A message that
    /// restates the same timer does not fire.
    #[tokio::test]
    async fn timer_changes_fire_when_a_message_heals_the_timer() {
        use std::time::Duration;
        let (client, _dir) = build_test_client();
        let mut changes = client.subscribe_disappearing_timer_changes();
        let peer = UserId::new();
        let dm = |sent_at| DomainEvent::MessageReceived {
            message: Message::new_text(
                ChannelId::from(peer),
                ConversationType::ServerDm,
                Some(peer),
                "hi".into(),
                sent_at,
            ),
        };
        let bus = &client.services.event_bus;

        // The subscriber reads the stored timer when it handles a message, and
        // on this current-thread runtime it only runs once the test awaits: so
        // receive each change before moving the stored timer on.
        async fn next_change(
            changes: &mut tokio::sync::broadcast::Receiver<ChannelId>,
        ) -> ChannelId {
            tokio::time::timeout(Duration::from_secs(2), changes.recv())
                .await
                .expect("a change")
                .expect("open")
        }

        bus.emit_domain(dm(1)); // first sight: fires
        bus.emit_domain(dm(2)); // same timer: silent
        assert_eq!(next_change(&mut changes).await, ChannelId::from(peer));

        client
            .channels()
            .settings_repo
            .set(
                &format!("conv.disappearing.server_dm.{peer}"),
                &crate::services::disappearing::DmTimer {
                    seconds: 3_600,
                    version: 2,
                    enabled_until: 0,
                    activation_seconds: 0,
                },
            )
            .expect("set timer");
        bus.emit_domain(dm(3)); // healed to 1 hour: fires
        assert_eq!(next_change(&mut changes).await, ChannelId::from(peer));

        assert!(
            tokio::time::timeout(Duration::from_millis(200), changes.recv())
                .await
                .is_err(),
            "a message restating the same timer is not a change"
        );
    }
}

#[cfg(all(test, feature = "media"))]
mod call_policy_tests {
    use super::roster_changes;
    use std::collections::HashSet;

    /// A lag that swallowed a ParticipantLeft still yields the departure
    /// (and so a rotation) once the roster is re-read.
    #[test]
    fn lagged_roster_resync_finds_departures_and_joins() {
        let set = |ids: &[&str]| ids.iter().map(|s| s.to_string()).collect::<HashSet<_>>();
        let (gone, new) = roster_changes(&set(&["a", "b", "c"]), &set(&["a", "c", "d"]));
        assert_eq!(gone, vec!["b".to_string()]);
        assert_eq!(new, vec!["d".to_string()]);
        let (gone, new) = roster_changes(&set(&["a"]), &set(&["a"]));
        assert!(gone.is_empty() && new.is_empty());
    }
}

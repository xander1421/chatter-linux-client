use super::*;

impl ChatterClient {
    // ── Anon Mode (in-memory UI filter) ────────────────────────────────
    //
    // Shield flag: when true the UI hides channels and Saved Messages
    // (`visible_in_mode`), friend DMs go through the mailbox
    // (`send_friend_dm_content`), and the transport switches to onion. Not
    // persisted by design —
    // every fresh client starts with anon_mode = false so privacy
    // properties auto-revert on restart.

    /// Platform lifecycle report: is the app on screen (window focused /
    /// activity resumed)? While it is not, nothing is marked read or
    /// receipted — an open chat behind a lock screen has not been read.
    ///
    /// Coming back into view catches up: the open conversation, now actually
    /// on screen, is marked read (and receipted, subject to the user's
    /// receipt setting). Call from within the tokio runtime.
    pub fn set_app_visible(self: &Arc<Self>, visible: bool) {
        let was_visible = self.chat_state().set_app_visible(visible);
        #[cfg(target_os = "android")]
        if let Some(provider) = self.services.device_inbox_provider.get() {
            provider.set_app_visible(visible);
        }
        if !visible || was_visible {
            return;
        }
        // Coming back to the app is activity: an auto-away (5 min without
        // typing or sending) returns to Online, as reading alone never did.
        let presence = Arc::clone(self);
        tokio::spawn(async move { presence.record_presence_activity().await });
        self.refresh_onion_routes();
        let active = self.chat_state().active_conversation();
        if !active.is_active() {
            return;
        }
        let client = Arc::clone(self);
        tokio::spawn(async move {
            if let Err(e) = client.conversation(active).mark_as_read().await {
                tracing::debug!(error = %e, "mark-read on return to foreground failed");
            }
        });
    }

    /// Refresh the Tor directory and replace potentially dead h2 connections
    /// after foreground resume or an Android default-network change.
    pub fn refresh_onion_routes(&self) {
        #[cfg(feature = "onion")]
        {
            let pool = self.services.grpc_pool.clone();
            let shield = self.anon_mode_enabled();
            // A network transition invalidates the h2 connections, not just
            // Arti's directory. Do this before scheduling warmup so sends and
            // polls cannot keep borrowing the old channel while it redials.
            pool.invalidate_onion_connections();
            if shield
                && pool.transport_mode()
                    == crate::network::grpc::client_pool::TransportMode::Onion
            {
                crate::network::connection::set_tor_status(
                    crate::network::connection::TorStatus::Connecting,
                );
            }
            let inbox = self.services.device_inbox_provider.get().cloned();
            self.services.runtime_handle.spawn(async move {
                crate::network::grpc::onion_transport::refresh_directory(pool.onion_tor());
                if shield
                    && pool.transport_mode()
                        == crate::network::grpc::client_pool::TransportMode::Onion
                {
                    // The provider owns clones of its old authenticated and
                    // anonymous channels; replacing pool entries alone cannot
                    // move its active poll loop to the new network.
                    if let Some(inbox) = inbox {
                        inbox.refresh_clients(&pool).await;
                    }
                    let _ = tokio::join!(
                        async {
                            if let Err(error) = pool.connect().await {
                                tracing::debug!(%error, "Shield onion preconnect failed");
                            }
                        },
                        async {
                            if let Err(error) = pool.preconnect_unidentified().await {
                                tracing::debug!(%error, "Shield anonymous preconnect failed");
                            }
                        }
                    );
                }
                if let Err(error) = pool.preconnect_secret().await {
                    tracing::debug!(%error, "secret onion preconnect failed");
                }
            });
        }
    }

    /// Read the in-memory anon-mode flag.
    pub fn anon_mode_enabled(&self) -> bool {
        self.anon_mode.load(Ordering::Acquire)
    }

    /// Flip the in-memory anon-mode flag. Callers (the UI handler) are
    /// responsible for re-running sidebar refresh helpers after flip;
    /// this method does not emit a domain event because the flag is a
    /// presentation concern, not a chatter-core state change.
    pub fn set_anon_mode_enabled(self: &Arc<Self>, enabled: bool) {
        self.update_transport_epoch
            .send_modify(|epoch| *epoch = epoch.wrapping_add(1));
        self.anon_mode.store(enabled, Ordering::Release);
        // Suspend direct SSE synchronously at Shield entry, before queuing
        // transport rebuild. configure_push holds this same admission lock.
        if enabled {
            if let Some(tx) = self.push_shutdown.lock().take() {
                let _ = tx.send(());
            }
        }

        // Anon mode also routes the network transport over Tor (onion): enabling
        // it switches the whole pool to the pinned .onion, disabling it switches
        // back to direct. If onion isn't available (feature not built, no .onion
        // pinned, or bootstrap fails), we log LOUDLY and keep the current
        // transport — the UI filter still applies, but we must NOT pretend the
        // network location is hidden when it isn't.
        use crate::network::grpc::client_pool::TransportMode;
        let mode = if enabled {
            TransportMode::Onion
        } else {
            TransportMode::Direct
        };
        let switch_result = self.services.grpc_pool.set_transport_mode(mode);

        // Reflect the pool's ACTUAL transport into the process-global HTTP flag
        // that leaf media modules (gif) gate on and that the
        // CDN reqwest callsites branch on. Keyed on the real transport, not the
        // request, so a failed onion switch leaves it false — no false anonymity.
        let on_onion = self.services.grpc_pool.transport_mode() == TransportMode::Onion;
        crate::network::anon_http::ANON_HTTP_ACTIVE.store(on_onion, Ordering::Release);

        // Drive the Tor UI indicator: entering onion = Connecting (until the
        // first onion RPC round-trips, when the poller flips it to Connected);
        // leaving onion = Off; Shield on but still direct = Unavailable, which
        // the UI must show as "network location NOT hidden" for as long as it
        // lasts — Shield is not allowed to look fully active.
        use crate::network::connection::TorStatus;
        crate::network::connection::set_tor_status(if on_onion {
            TorStatus::Connecting
        } else if enabled {
            TorStatus::Unavailable
        } else {
            TorStatus::Off
        });

        if let Err(e) = switch_result {
            tracing::warn!(
                requested = ?mode,
                error = %e,
                "anon mode: transport switch failed — UI filter applies but network location is NOT hidden"
            );
            // Transport unchanged: leave the long-lived loops alone. They are
            // already on the current (correct) transport, and tearing them down
            // would only churn connections for no anonymity gain.
            return;
        }

        // ANONYMITY-CRITICAL: set_transport_mode drops the pool's cached
        // channels so every *new* RPC and per-attempt retry rebuilds over the
        // new transport — but the long-lived loops opened BEFORE the flip hold
        // their own channel clones (pinned to the pre-switch transport) and
        // would keep egressing the client IP until they naturally reconnect.
        // Rebuild them now so they re-dial over the new transport. Runs on the
        // client runtime (this method is invoked from the UI thread, which has
        // no ambient runtime and must never block). Serialized via
        // `transport_switch_lock` so rapid toggles converge, not interleave.
        let this = Arc::clone(self);
        self.services.runtime_handle.spawn(async move {
            let _guard = this.transport_switch_lock.lock().await;
            this.rebuild_transport_loops().await;
            drop(_guard);
            if this.anon_mode_enabled()
                && this.services.grpc_pool.transport_mode() == TransportMode::Onion
            {
                let pool = this.services.grpc_pool.clone();
                // Warm authenticated, unidentified-deposit, and secret routes.
                let _ = tokio::join!(
                    pool.connect(),
                    async {
                        #[cfg(feature = "onion")]
                        if let Err(error) = pool.preconnect_unidentified().await {
                            tracing::debug!(%error, "Shield anonymous preconnect failed");
                        }
                    },
                    async {
                        #[cfg(feature = "onion")]
                        if let Err(error) = pool.preconnect_secret().await {
                            tracing::debug!(%error, "secret onion preconnect will retry on use");
                        }
                    }
                );
            }
        });
    }

    /// Tear down and rebuild every long-lived network loop so it re-dials over
    /// the transport that is now active. Called after a successful anon-mode
    /// transport switch. Runs inside the client runtime (ambient runtime
    /// required for the re-spawns) and under `transport_switch_lock`.
    pub(super) async fn rebuild_transport_loops(self: &Arc<Self>) {
        // Read the LIVE anon state, not a captured param. Under rapid toggles
        // the `transport_switch_lock` grants in arrival order (not spawn order),
        // so a stale captured value could resurrect push against the final Onion
        // transport (SSE-over-direct leak). notif/inbox/fanout already read live
        // pool state; the push decision must converge with them to the final
        // transport, so it reads the same authoritative atomic.
        let anon = self.anon_mode.load(std::sync::atomic::Ordering::Acquire);

        // 1. gRPC notification stream — abort the in-flight stream (pinned to
        //    the old channel) and re-spawn so it connects over the new one.
        self.respawn_notification_stream();

        // 2. Account/device push uses direct private-CA HTTPS and remains off
        //    in Shield. Anonymous mailbox subscriptions stay on isolated Tor.
        //    Restore the previously-configured account push on exit.
        let remembered_push_url = self.push_url.lock().clone();
        if anon {
            if let Some(tx) = self.push_shutdown.lock().take() {
                let _ = tx.send(());
                tracing::info!("anon mode: ntfy push suspended (poll-only delivery)");
            }
        } else if let Some(url) = remembered_push_url {
            if let Err(e) = self.configure_push(&url) {
                tracing::warn!(error = %e, "anon mode: failed to restore ntfy push on exit");
            } else {
                tracing::info!("anon mode: ntfy push restored");
            }
        }

        // 3. Inbox poll loop — the provider captured its mailbox/message gRPC
        //    clients at construction (pinned to the pre-switch channel), so a
        //    bare stop/start would keep the old transport. Stop, rebuild those
        //    clients from the pool (now on the new transport), then restart.
        if let Some(provider) = self.ctx.get_device_inbox_provider() {
            if let Err(e) = provider.stop().await {
                tracing::warn!(error = %e, "anon mode: inbox provider stop failed during transport switch");
            }
            provider.refresh_clients(&self.services.grpc_pool).await;
            if let Err(e) = provider.start().await {
                tracing::warn!(error = %e, "anon mode: inbox provider restart failed during transport switch");
            } else {
                tracing::info!("anon mode: inbox poll loop re-dialed over new transport");
            }
        }

        // 4. Group fanout — the mailbox client is captured once at login
        //    (`init_group_fanout`) and pinned into a OnceLock, so every group
        //    send deposits through it. Without this swap the core group-message
        //    path keeps egressing the real IP after anon-enable. Rebuild it
        //    from the pool (now on the new transport).
        if let Some(fanout) = self.ctx.get_group_fanout() {
            fanout.refresh_client(&self.services.grpc_pool).await;
            tracing::info!("anon mode: group fanout client re-dialed over new transport");
        }

        // 5. Call worker — the signaling `CallClient` is captured for the
        //    worker's lifetime, so call create/join/leave/end would leak the
        //    real IP after anon-enable. Hand the worker a fresh client built on
        //    the new transport; it applies the swap between complete call
        //    operations, so no in-flight call is corrupted.
        #[cfg(feature = "media")]
        if let Some(handle) = self.ctx.get_call_service() {
            handle
                .swap_call_client(self.services.grpc_pool.call_client())
                .await;
            tracing::info!("anon mode: call worker client swap enqueued for new transport");
        }
    }

    /// Verify locally, or use the existing metered SVR proof on a device
    /// without a fingerprint. The PIN never enters diagnostic output.
    pub async fn verify_pin_for_anon_mode(
        &self,
        pin: &str,
    ) -> crate::services::link_device_service::PinVerification {
        self.verify_pin_with_backup(pin).await
    }

    /// Whether Shield can hide this device's network location in this build:
    /// the `onion` feature is compiled in and a server `.onion` is pinned
    /// (`config::PINNED_ONION_ADDR`). The sign-in and create-account screens
    /// offer Shield only when this holds.
    pub fn shield_available(&self) -> bool {
        self.services.grpc_pool.onion_available()
    }

    /// Choose Shield on the sign-in / create-account / recovery screens,
    /// before any account is signed in, so the whole of onboarding —
    /// registration challenge and proof of work, Register, OPAQUE login,
    /// recovery, key upload, SVR backup, avatar upload — reaches the server
    /// only over its onion service.
    ///
    /// Fails closed. Enabling returns an error and changes nothing when the
    /// build cannot route over Tor, and never leaves the client on a direct
    /// connection while claiming Shield. Once on, the pool has no direct path:
    /// if Tor cannot bootstrap, every call fails rather than going direct.
    ///
    /// The session that signs in stays in Shield (`anon_mode_enabled()`), the
    /// same state the chat-list toggle produces. Like that toggle it is
    /// in-memory only: the next app start is direct again (owner decision).
    ///
    /// Refused once signed in: from there the chat-list toggle and its PIN
    /// gate own the switch.
    pub fn set_shield_before_login(self: &Arc<Self>, enabled: bool) -> Result<(), ClientError> {
        if self.is_logged_in() {
            return Err(ClientError::Validation(
                "already signed in; use the Shield toggle on the chat list".into(),
            ));
        }
        if enabled && !self.shield_available() {
            return Err(ClientError::Validation(
                "Shield can't hide your network location in this build (no Tor onion service configured)".into(),
            ));
        }
        self.set_anon_mode_enabled(enabled);

        use crate::network::grpc::client_pool::TransportMode;
        if enabled && self.services.grpc_pool.transport_mode() != TransportMode::Onion {
            // Availability was checked above, so this is an endpoint that would
            // not build. Undo rather than sign in with Shield showing and the
            // traffic going direct.
            self.set_anon_mode_enabled(false);
            return Err(ClientError::Validation(
                "couldn't switch to the Tor onion service; Shield is off".into(),
            ));
        }
        Ok(())
    }

    pub(crate) fn offline_state(&self) -> &OfflineState {
        &self.services.offline_state
    }

    #[cfg(feature = "testing")]
    pub fn set_offline_connection_state_for_testing(
        &self,
        state: crate::network::connection::ConnectionState,
    ) {
        self.services.offline_state.set_connection_state(state);
    }
    #[cfg(feature = "jarvis")]
    pub fn jarvis_state(&self) -> &JarvisState {
        &self.services.jarvis_state
    }

}

use super::*;

impl ChatterClient {
    // ── bitchat mesh façade ─────────────────────────────────────────
    //
    // `MeshService` is `pub(crate)`, so the UI reaches it through here. Most of
    // these are one-line forwards; `mesh_send` is not, and that is the point —
    // choosing between the room, a cell and a DM is a routing decision, and the
    // presentation layer must not be the thing that makes it.

    /// Live mesh events: messages, peers, transport state.
    #[cfg(feature = "mesh")]
    pub fn mesh_subscribe(
        &self,
    ) -> tokio::sync::broadcast::Receiver<crate::services::mesh::MeshEvent> {
        self.mesh().subscribe()
    }

    /// Radio and relay state. `connected_peers` is the mesh's connectivity
    /// signal; zero is an ordinary state, not a failure.
    #[cfg(feature = "mesh")]
    pub fn mesh_status(&self) -> crate::services::mesh::MeshStatus {
        self.mesh().status()
    }

    /// Everyone currently visible on the radio.
    #[cfg(feature = "mesh")]
    pub fn mesh_peers(&self) -> Vec<crate::services::mesh::MeshPeer> {
        let mut peers = self.mesh().peers();
        // The mesh state has no owner account; the private binding is per owner.
        if let Some(owner) = self.session_current_user_id() {
            for peer in &mut peers {
                peer.account_verified = matches!(
                    self.mesh().binding_state(owner, &peer.peer),
                    Ok(crate::storage::mesh_binding_store::PeerBinding::Verified { .. })
                );
            }
        }
        peers
    }

    #[cfg(feature = "mesh")]
    pub fn mesh_nearby_peers(&self) -> Vec<crate::services::mesh::MeshPeer> {
        self.mesh().nearby_peers()
    }

    #[cfg(feature = "mesh")]
    pub fn mesh_peer_for_sender(
        &self,
        sender: &crate::domain::ids::UserId,
    ) -> Option<crate::services::mesh::MeshPeer> {
        self.mesh().peer_for_sender(sender)
    }

    #[cfg(feature = "mesh")]
    pub fn mesh_open_dm(&self, peer: &str) -> Result<String, crate::services::mesh::MeshError> {
        self.mesh()
            .open_dm(&chatter_mesh::ids::PeerId::from_str(peer))
    }

    /// Open mesh conversations, for the sidebar.
    #[cfg(feature = "mesh")]
    pub fn mesh_conversations(
        &self,
    ) -> Result<
        Vec<crate::domain::unified_conversation::UnifiedConversation>,
        crate::services::mesh::MeshError,
    > {
        self.mesh().conversations()
    }

    /// Mesh sidebar rows, shaped and ordered by the mesh service.
    #[cfg(feature = "mesh")]
    pub fn mesh_sidebar_rows(
        &self,
    ) -> Result<Vec<crate::services::mesh::MeshSidebarRow>, crate::services::mesh::MeshError> {
        self.mesh().sidebar_rows(self.session_current_user_id())
    }

    #[cfg(feature = "mesh")]
    pub fn mesh_sidebar_row_for(
        &self,
        id: &str,
    ) -> Result<Option<crate::services::mesh::MeshSidebarRow>, crate::services::mesh::MeshError>
    {
        self.mesh()
            .sidebar_row_for(self.session_current_user_id(), id)
    }

    #[cfg(feature = "mesh")]
    pub fn mesh_peer_by_id(&self, peer_id: &str) -> Option<crate::services::mesh::MeshPeer> {
        self.mesh().peer_by_id(peer_id)
    }

    /// Which mesh scope a sidebar row refers to.
    #[cfg(feature = "mesh")]
    pub fn mesh_conversation_for(
        &self,
        id: &str,
    ) -> Option<crate::services::mesh::MeshConversation> {
        self.mesh().conversation_for(id)
    }

    /// The mesh sidebar's stored title for a conversation.
    #[cfg(feature = "mesh")]
    pub fn mesh_conversation_name(&self, id: &crate::domain::ids::ChannelId) -> Option<String> {
        self.mesh().conversation_name(id)
    }

    /// Stored history for one mesh conversation, oldest first.
    #[cfg(feature = "mesh")]
    pub fn mesh_history(
        &self,
        conversation: &crate::services::mesh::MeshConversation,
        before_sent_at: Option<u64>,
        limit: u32,
    ) -> Result<Vec<crate::domain::message::Message>, crate::services::mesh::MeshError> {
        self.mesh().history(conversation, before_sent_at, limit)
    }

    /// Clear a mesh conversation's unread count locally.
    ///
    /// Local only. Telling the peer costs airtime and is a separate act.
    #[cfg(feature = "mesh")]
    pub fn mesh_mark_conversation_read(
        &self,
        conversation: &crate::services::mesh::MeshConversation,
    ) -> Result<(), crate::services::mesh::MeshError> {
        self.mesh().mark_conversation_read(conversation)
    }

    /// Join a geohash cell. Public: everything said there is readable by anyone
    /// subscribed to that cell.
    #[cfg(feature = "mesh")]
    pub fn mesh_join_geohash(&self, cell: &str) -> Result<(), crate::services::mesh::MeshError> {
        self.mesh().join_geohash(cell)
    }

    /// Leave a cell. History stays; only the sidebar entry goes.
    #[cfg(feature = "mesh")]
    pub fn mesh_leave_geohash(&self, cell: &str) -> Result<(), crate::services::mesh::MeshError> {
        self.mesh().leave_geohash(cell)
    }

    /// Our own mesh sender id, for telling our messages from everyone else's.
    ///
    /// Derived from the mesh peer ID and unrelated to the chatter account: the
    /// two identities are deliberately separate.
    #[cfg(feature = "mesh")]
    pub fn mesh_local_user_id(&self) -> crate::domain::ids::UserId {
        crate::services::mesh::mapping::peer_user_id(&self.mesh().local_peer())
    }

    #[cfg(feature = "mesh")]
    pub fn mesh_share_identity(&self, peer: &str) -> Result<(), crate::services::mesh::MeshError> {
        let owner = self.session_current_user_id().ok_or_else(|| {
            crate::services::mesh::MeshError::Binding("sign in before sharing your identity".into())
        })?;
        let device = self.sessions().current_device_id().ok_or_else(|| {
            crate::services::mesh::MeshError::Binding("no account device is active".into())
        })?;
        self.mesh()
            .share_identity(&chatter_mesh::ids::PeerId::from_str(peer), owner, device)
    }

    #[cfg(feature = "mesh")]
    pub fn mesh_identity_qr_png(
        &self,
        peer: &str,
    ) -> Result<Vec<u8>, crate::services::mesh::MeshError> {
        let owner = self.session_current_user_id().ok_or_else(|| {
            crate::services::mesh::MeshError::Binding("sign in before sharing your identity".into())
        })?;
        let device = self.sessions().current_device_id().ok_or_else(|| {
            crate::services::mesh::MeshError::Binding("no account device is active".into())
        })?;
        self.mesh()
            .identity_qr_png(&chatter_mesh::ids::PeerId::from_str(peer), owner, device)
    }

    #[cfg(feature = "mesh")]
    pub fn mesh_verify_binding(
        &self,
        peer: &str,
        scanned_qr: Option<&str>,
    ) -> Result<MeshBindingStatus, crate::services::mesh::MeshError> {
        let owner = self.session_current_user_id().ok_or_else(|| {
            crate::services::mesh::MeshError::Binding("sign in before verifying a contact".into())
        })?;
        let device = self.sessions().current_device_id().ok_or_else(|| {
            crate::services::mesh::MeshError::Binding("no account device is active".into())
        })?;
        self.mesh()
            .verify_binding(
                owner,
                device,
                &chatter_mesh::ids::PeerId::from_str(peer),
                scanned_qr,
            )
            .map(Into::into)
    }

    #[cfg(feature = "mesh")]
    pub fn mesh_open_session(&self, peer: &str) {
        self.mesh()
            .open_session(&chatter_mesh::ids::PeerId::from_str(peer));
    }

    #[cfg(feature = "mesh")]
    pub fn mesh_binding_state(
        &self,
        peer: &str,
    ) -> Result<MeshBindingStatus, crate::services::mesh::MeshError> {
        let owner = self.session_current_user_id().ok_or_else(|| {
            crate::services::mesh::MeshError::Binding("sign in to view account bindings".into())
        })?;
        self.mesh()
            .binding_state(owner, &chatter_mesh::ids::PeerId::from_str(peer))
            .map(Into::into)
    }

    #[cfg(feature = "mesh")]
    pub fn mesh_signal_opted_in(
        &self,
        peer: &str,
    ) -> Result<bool, crate::services::mesh::MeshError> {
        let Some(owner) = self.session_current_user_id() else {
            return Ok(false);
        };
        self.mesh()
            .signal_opted_in(owner, &chatter_mesh::ids::PeerId::from_str(peer))
    }

    #[cfg(feature = "mesh")]
    pub fn mesh_signal_protection_label(
        &self,
        peer: &str,
    ) -> Result<&'static str, crate::services::mesh::MeshError> {
        let Some(owner) = self.session_current_user_id() else {
            return Ok("Noise (bitchat)");
        };
        self.mesh()
            .signal_protection_label(owner, &chatter_mesh::ids::PeerId::from_str(peer))
    }

    #[cfg(feature = "mesh")]
    pub fn mesh_set_signal_opt_in(
        &self,
        peer: &str,
        enabled: bool,
    ) -> Result<(), crate::services::mesh::MeshError> {
        let owner = self.session_current_user_id().ok_or_else(|| {
            crate::services::mesh::MeshError::Binding("sign in before selecting Signal mesh".into())
        })?;
        self.mesh()
            .set_signal_opt_in(owner, &chatter_mesh::ids::PeerId::from_str(peer), enabled)
    }

    #[cfg(feature = "mesh")]
    pub fn mesh_comparison_code(&self, peer: &str) -> Option<String> {
        self.mesh()
            .comparison_code(&chatter_mesh::ids::PeerId::from_str(peer))
    }

    /// The nickname behind a mesh sender id, or `None` if no known peer matches.
    ///
    /// Only meaningful for messages that came off the mesh. The ordinary
    /// display-name path cannot resolve these: a mesh peer has no row in any
    /// user repository, so `UserService::get_display_name` falls through to
    /// stringifying the id and the UI showed a raw UUID.
    #[cfg(feature = "mesh")]
    pub fn mesh_sender_display_name(&self, sender: &crate::domain::ids::UserId) -> Option<String> {
        self.mesh().sender_display_name(sender)
    }

    /// Start the mesh. Deliberately **not** part of [`start`](Self::start).
    ///
    /// `start` is the authenticated client coming up: inbox polling, channel
    /// streams, prekeys — all of which need an account. The mesh needs none of
    /// them. It makes no RPC, shares no identity with the account, and is the
    /// one part of this app that is supposed to work when there is no server,
    /// no internet, and no login.
    ///
    /// Gating it behind authentication would have broken exactly the situations
    /// it exists for: the disaster and protest scenarios bitchat targets are
    /// the ones where nobody is signing in to anything.
    ///
    /// Safe to call more than once; the service tracks whether it is running.
    #[cfg(feature = "mesh")]
    pub async fn start_mesh(&self) -> Result<(), crate::services::mesh::MeshError> {
        if !self.mesh_enabled() {
            return Ok(());
        }
        self.mesh().start().await
    }

    /// Stop the mesh: radio down, relays disconnected, runtimes joined.
    ///
    /// The service can be started again afterwards — see `CommandBus`, which
    /// exists so that a stop is not permanent. What is retained stays retained:
    /// stopping the transport does not discard the timeline, the identity, or the
    /// outbox, so anything already written is still there when it comes back.
    #[cfg(feature = "mesh")]
    pub async fn stop_mesh(&self) {
        self.mesh().shutdown().await;
    }

    /// Whether the user explicitly enabled mesh. Runtime radios default off.
    #[cfg(feature = "mesh")]
    #[must_use]
    pub fn mesh_enabled(&self) -> bool {
        use crate::storage::repositories::settings_repo::{SettingsRepository, keys};
        SettingsRepository::new(self.services.database.clone())
            .get_bool(keys::MESH_ENABLED)
            .ok()
            .flatten()
            .unwrap_or(false)
    }

    #[cfg(feature = "mesh")]
    pub fn mesh_nickname(&self) -> String {
        self.mesh().nickname()
    }

    #[cfg(feature = "mesh")]
    pub fn mesh_nearby_channels(&self) -> Vec<String> {
        self.mesh().nearby_channels()
    }

    #[cfg(feature = "mesh")]
    pub fn mesh_create_private_channel(
        &self,
        name: &str,
    ) -> Result<String, crate::services::mesh::MeshError> {
        self.mesh().create_private_channel(name)
    }

    #[cfg(feature = "mesh")]
    pub fn mesh_private_channel_invite_png(
        &self,
        id: &str,
        peer: &str,
    ) -> Result<Vec<u8>, crate::services::mesh::MeshError> {
        self.mesh()
            .private_channel_invite_png(id, &chatter_mesh::ids::PeerId::from_str(peer))
    }

    #[cfg(feature = "mesh")]
    pub fn mesh_join_private_channel(
        &self,
        qr: &str,
    ) -> Result<(), crate::services::mesh::MeshError> {
        self.mesh().join_private_channel(qr)
    }

    #[cfg(feature = "mesh")]
    pub fn mesh_invite_private_channel_direct(
        &self,
        id: &str,
        peer: &str,
    ) -> Result<(), crate::services::mesh::MeshError> {
        self.mesh()
            .invite_private_channel_direct(id, &chatter_mesh::ids::PeerId::from_str(peer))
    }

    #[cfg(feature = "mesh")]
    pub fn mesh_decide_private_invitation(
        &self,
        id: &str,
        join: bool,
    ) -> Result<(), crate::services::mesh::MeshError> {
        self.mesh().decide_private_invitation(id, join)
    }

    #[cfg(feature = "mesh")]
    pub fn mesh_pending_private_invitations(&self) -> Vec<(String, String, String)> {
        self.mesh().pending_private_invitations()
    }

    #[cfg(feature = "mesh")]
    pub fn mesh_remove_private_channel_member(
        &self,
        id: &str,
        peer: &str,
    ) -> Result<(), crate::services::mesh::MeshError> {
        self.mesh()
            .remove_private_channel_member(id, &chatter_mesh::ids::PeerId::from_str(peer))
    }

    #[cfg(feature = "mesh")]
    pub fn mesh_leave_private_channel(
        &self,
        id: &str,
    ) -> Result<(), crate::services::mesh::MeshError> {
        self.mesh().leave_private_channel(id)
    }

    #[cfg(feature = "mesh")]
    pub fn mesh_private_channel_removable_members(
        &self,
        id: &str,
    ) -> Result<Vec<crate::services::mesh::PrivateChannelMember>, crate::services::mesh::MeshError>
    {
        self.mesh().private_channel_removable_members(id)
    }

    #[cfg(feature = "mesh")]
    pub fn mesh_private_channel_member_count(
        &self,
        id: &str,
    ) -> Result<usize, crate::services::mesh::MeshError> {
        self.mesh().private_channel_member_count(id)
    }

    #[cfg(feature = "mesh")]
    pub fn mesh_private_channel_is_creator(
        &self,
        id: &str,
    ) -> Result<bool, crate::services::mesh::MeshError> {
        self.mesh().private_channel_is_creator(id)
    }

    #[cfg(feature = "mesh")]
    pub fn mesh_private_channel_rekey_pending(
        &self,
        id: &str,
    ) -> Result<bool, crate::services::mesh::MeshError> {
        self.mesh().private_channel_rekey_pending(id)
    }

    #[cfg(feature = "mesh")]
    pub fn mesh_join_channel(
        &self,
        name: &str,
    ) -> Result<String, crate::services::mesh::MeshError> {
        self.mesh().join_channel(name)
    }

    #[cfg(feature = "mesh")]
    pub fn mesh_leave_channel(&self, name: &str) -> Result<(), crate::services::mesh::MeshError> {
        self.mesh().leave_channel(name)
    }

    #[cfg(feature = "mesh")]
    pub fn mesh_channel_muted(&self, name: &str) -> bool {
        self.mesh().channel_muted(name)
    }

    #[cfg(feature = "mesh")]
    pub fn mesh_set_channel_muted(
        &self,
        name: &str,
        muted: bool,
    ) -> Result<(), crate::services::mesh::MeshError> {
        use crate::storage::repositories::settings_repo::{SettingsRepository, keys};
        let name = chatter_mesh::channel::canonical_name(name).ok_or(
            crate::services::mesh::MeshError::OutOfRange("invalid channel name"),
        )?;
        let settings = SettingsRepository::new(self.services.database.clone());
        let mut names = settings
            .get::<Vec<String>>(keys::MESH_MUTED_CHANNELS)?
            .unwrap_or_default();
        names.retain(|existing| existing != &name);
        if muted {
            names.push(name.clone());
        }
        settings.set(keys::MESH_MUTED_CHANNELS, &names)?;
        self.mesh().set_channel_muted(&name, muted)
    }

    #[cfg(feature = "mesh")]
    pub fn mesh_peer_blocked(&self, peer: &str) -> bool {
        self.mesh()
            .peer_blocked(&chatter_mesh::ids::PeerId::from_str(peer))
    }

    #[cfg(feature = "mesh")]
    pub fn mesh_peer_muted(&self, peer: &str) -> bool {
        self.mesh()
            .peer_muted(&chatter_mesh::ids::PeerId::from_str(peer))
    }

    #[cfg(feature = "mesh")]
    pub fn mesh_set_peer_muted(
        &self,
        peer: &str,
        muted: bool,
    ) -> Result<(), crate::services::mesh::MeshError> {
        use crate::storage::repositories::settings_repo::{SettingsRepository, keys};
        let peer = chatter_mesh::ids::PeerId::from_str(peer);
        let settings = SettingsRepository::new(self.services.database.clone());
        let mut peers = settings
            .get::<Vec<String>>(keys::MESH_MUTED_PEERS)?
            .unwrap_or_default();
        peers.retain(|existing| existing != peer.as_str());
        if muted {
            peers.push(peer.as_str().to_owned());
        }
        settings.set(keys::MESH_MUTED_PEERS, &peers)?;
        self.mesh().set_peer_muted(&peer, muted);
        Ok(())
    }

    #[cfg(feature = "mesh")]
    pub fn mesh_set_peer_blocked(
        &self,
        peer: &str,
        blocked: bool,
    ) -> Result<(), crate::services::mesh::MeshError> {
        use crate::storage::repositories::settings_repo::{SettingsRepository, keys};
        let peer = chatter_mesh::ids::PeerId::from_str(peer);
        let settings = SettingsRepository::new(self.services.database.clone());
        let mut peers = settings
            .get::<Vec<String>>(keys::MESH_BLOCKED_PEERS)?
            .unwrap_or_default();
        peers.retain(|existing| existing != peer.as_str());
        if blocked {
            peers.push(peer.as_str().to_owned());
        }
        settings.set(keys::MESH_BLOCKED_PEERS, &peers)?;
        self.mesh().set_peer_blocked(&peer, blocked);
        Ok(())
    }

    #[cfg(feature = "mesh")]
    pub fn set_mesh_nickname(&self, name: &str) -> Result<(), crate::services::mesh::MeshError> {
        use crate::storage::repositories::settings_repo::{SettingsRepository, keys};
        let name = name.trim();
        if name.is_empty() || name.len() > 32 || name.chars().any(char::is_control) {
            return Err(crate::services::mesh::MeshError::OutOfRange(
                "mesh nickname must be 1-32 bytes without control characters",
            ));
        }
        SettingsRepository::new(self.services.database.clone())
            .set_string(keys::MESH_NICKNAME, name)?;
        self.mesh().set_nickname(name.to_owned());
        Ok(())
    }

    /// Record the preference and act on it now.
    ///
    /// Both halves, deliberately: a toggle that persists without taking effect
    /// until a restart is a toggle that appears broken, and one that takes effect
    /// without persisting comes back on by itself. A storage failure is reported
    /// rather than swallowed, because the setting silently not sticking is worse
    /// than being told it did not.
    #[cfg(feature = "mesh")]
    pub async fn set_mesh_enabled(
        &self,
        enabled: bool,
    ) -> Result<(), crate::services::mesh::MeshError> {
        use crate::storage::repositories::settings_repo::{SettingsRepository, keys};
        SettingsRepository::new(self.services.database.clone())
            .set_bool(keys::MESH_ENABLED, enabled)?;
        if enabled {
            self.start_mesh().await
        } else {
            self.stop_mesh().await;
            Ok(())
        }
    }

    /// Whether the mesh service came up and is still up.
    ///
    /// Not "the radio is on" and not "a peer is in range" — just that the
    /// service exists, which is what decides whether there are mesh
    /// conversations to open at all.
    #[cfg(feature = "mesh")]
    #[must_use]
    pub fn mesh_running(&self) -> bool {
        self.mesh().is_running()
    }

    /// Whether this app has anything usable by someone with no account.
    ///
    /// True exactly when the mesh service is running. The mesh is serverless and
    /// account-free by construction (see [`start_mesh`](Self::start_mesh)), so a
    /// running mesh means a user who has never registered, and cannot register
    /// because there is no server to register with, still has conversations to
    /// open — and the sign-in screen has something to offer a way past itself
    /// *to*.
    ///
    /// It is deliberately the running service and not the `mesh` Cargo feature.
    /// A build with the feature whose `start` failed (no sealer key, for
    /// instance) has nothing behind the login screen, and offering a way past it
    /// would strand the user somewhere emptier than where they began.
    ///
    /// This says nothing about whether messages will be delivered. Delivery on a
    /// mesh depends on who is in radio range, is best-effort, and may be
    /// deferred by hours; the mesh surfaces say so themselves and this must not
    /// be read as a promise on their behalf.
    #[must_use]
    pub fn has_account_free_surfaces(&self) -> bool {
        #[cfg(feature = "mesh")]
        {
            self.mesh_running()
        }
        #[cfg(not(feature = "mesh"))]
        {
            false
        }
    }

    /// Start the server-reachability monitor.
    ///
    /// Deliberately **not** part of [`start`](Self::start), for the same reason
    /// [`start_mesh`](Self::start_mesh) is not: `start` is the authenticated
    /// client coming up, and the client that most needs to know whether the
    /// server is there is the one that could not sign in to it. Called from app
    /// startup regardless of login.
    ///
    /// Safe to call more than once — extra monitors would only duplicate probes,
    /// so the second call is a no-op.
    /// App startup's server contact. Signed in: connect (bounded by `budget`)
    /// and start the reachability monitor, as before. Signed out: contact
    /// nothing. The user has not chosen Shield yet, and a direct dial now —
    /// the boot connect or the monitor's probes — would show the server this
    /// IP opening the app seconds before an onion sign-up from it. Both wait
    /// for the first `connect`, which every sign-in / create-account /
    /// recovery / link-device action makes, by then on the chosen transport.
    ///
    /// Returns whether the server answered (always false when signed out).
    pub async fn startup_connect(&self, budget: std::time::Duration) -> bool {
        if !self.is_logged_in() {
            self.server_contact_deferred.store(true, Ordering::SeqCst);
            tracing::info!("signed out: no server contact until a sign-in action");
            return false;
        }
        let fence = self.services.grpc_pool.connect_fence();
        match tokio::time::timeout(budget, self.connect()).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::warn!("Could not connect to server: {} (offline mode)", e);
            }
            Err(_) => {
                tracing::warn!(
                    budget_s = budget.as_secs(),
                    "Server did not answer within the startup budget (offline mode)"
                );
                self.services.grpc_pool.note_connect_timeout(fence);
                crate::network::server_monitor::mirror(self.offline_state());
            }
        };
        self.start_server_monitor();
        crate::network::connection::server_status()
            == crate::network::connection::ServerStatus::Reachable
    }

    /// Whether the sign-in screens should say the server is unreachable. Not
    /// before anything has tried to reach it: a signed-out start defers all
    /// contact (`startup_connect`), and "unreachable" would be a guess.
    pub fn server_unreachable(&self) -> bool {
        self.server_unreachable_for_state(self.offline_state().connection_state())
    }

    /// Apply the deferred-contact guard to a captured connection state, without
    /// rereading OfflineState and mixing two different UI snapshots.
    pub fn server_unreachable_for_state(
        &self,
        state: crate::network::connection::ConnectionState,
    ) -> bool {
        !self.server_contact_deferred.load(Ordering::SeqCst) && state.is_unreachable()
    }

    pub fn start_server_monitor(&self) {
        if self.server_monitor_started.swap(true, Ordering::SeqCst) {
            return;
        }
        let handle = tokio::spawn(crate::network::server_monitor::run(
            self.services.grpc_pool.clone(),
            self.services.offline_state.clone(),
        ));
        self.tasks.lock().push(handle);
    }

    /// Panic wipe: destroy everything this device holds about the mesh.
    ///
    /// Identity keypair, retained mail, carried courier envelopes, gossip
    /// history, bridge dedup sets, every mesh message, the conversation
    /// directory, and favourites.
    ///
    /// **It does not touch the chatter account**, its Signal identity, or its
    /// messages. Those are a separate identity with a separate threat model,
    /// and destroying an account is a larger irreversible act that deserves its
    /// own confirmation rather than riding along with this one. Anyone relying
    /// on this to leave a device clean of everything needs to know that.
    #[cfg(feature = "mesh")]
    pub fn mesh_panic_wipe(&self) -> Result<(), crate::services::mesh::MeshError> {
        use crate::storage::repositories::settings_repo::{SettingsRepository, keys};
        let wiped = self.mesh().panic_wipe();
        let settings = SettingsRepository::new(self.services.database.clone());
        for key in [
            keys::MESH_NICKNAME,
            keys::MESH_MUTED_CHANNELS,
            keys::MESH_MUTED_PEERS,
            keys::MESH_BLOCKED_PEERS,
        ] {
            settings.delete(key)?;
        }
        wiped
    }

    /// Send into a mesh conversation, choosing the transport from the scope.
    ///
    /// The room and a cell are broadcasts and return as soon as the packet is
    /// queued; a DM resolves to a [`MeshDelivery`] describing how far it
    /// actually got. Note that "sent" here can mean "handed to a courier who
    /// may hand it over hours from now" — further from "delivered" than
    /// anywhere else in the app.
    #[cfg(feature = "mesh")]
    pub async fn mesh_send(
        &self,
        conversation: &crate::services::mesh::MeshConversation,
        text: &str,
    ) -> Result<crate::services::mesh::MeshDelivery, crate::services::mesh::MeshError> {
        use crate::services::mesh::{MeshConversation, MeshDelivery};
        match conversation {
            MeshConversation::Room => {
                self.mesh().send_room(text)?;
                Ok(MeshDelivery::Broadcast)
            }
            MeshConversation::Geohash(cell) => {
                self.mesh().send_geohash(cell, text)?;
                Ok(MeshDelivery::Broadcast)
            }
            MeshConversation::Channel(name) => {
                self.mesh().send_channel(name, text)?;
                Ok(MeshDelivery::Broadcast)
            }
            MeshConversation::PrivateChannel(id) => {
                self.mesh().send_private_channel(id, text)?;
                Ok(MeshDelivery::Broadcast)
            }
            MeshConversation::Dm(peer) => {
                if let Some(owner) = self.session_current_user_id()
                    && self.mesh().signal_opted_in(owner, peer)?
                {
                    let device = self.sessions().current_device_id().ok_or_else(|| {
                        crate::services::mesh::MeshError::Binding(
                            "no account device is active".into(),
                        )
                    })?;
                    self.mesh().send_signal_dm(owner, device, peer, text).await
                } else {
                    self.mesh().send_dm(peer, text).await
                }
            }
        }
    }

    /// Put a file on the air for the mesh room.
    ///
    /// Room only, and there is no conversation argument for that reason: a mesh
    /// file transfer is a cleartext broadcast, while this client's mesh DMs are
    /// Noise-encrypted, so there is no directed form to send and the receiver
    /// refuses one. Returns the wire message ID; the row arrives on the mesh
    /// event stream like any other message, carrying the file as an attachment.
    ///
    /// Anything over the mesh's one-packet budget is chunked and streamed
    /// rather than truncated; past `MAX_STREAMED_FILE_BYTES` it is refused.
    #[cfg(feature = "mesh")]
    pub fn mesh_send_file(
        &self,
        path: &std::path::Path,
    ) -> Result<String, crate::services::mesh::MeshError> {
        self.mesh().send_file(path)
    }
    crate::service_accessor!(local_ai, LocalAIService, local_ai_service);
    crate::service_accessor!(
        http_local_ai,
        crate::services::local_ai_service::HttpLocalAIService,
        http_local_ai_service
    );
    crate::service_accessor!(dm_bootstrap, DmBootstrapService, dm_bootstrap_service);
    crate::service_accessor!(
        notifications,
        crate::services::notification_service::NotificationService,
        notification_service
    );
    crate::service_accessor!(sections, SectionService, section_service);
    crate::service_accessor!(
        conversation_pins,
        crate::services::conversation_pins::ConversationPins,
        conversation_pins
    );
    crate::service_accessor!(
        devices,
        crate::services::device_service::DeviceService,
        device_service
    );
}

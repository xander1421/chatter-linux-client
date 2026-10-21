use super::*;

/// How long a handshake with a known peer may sit unanswered before the next
/// direct announce from that peer restarts it.
const KNOWN_PEER_HANDSHAKE_STALL_MS: u64 = 10_000;

impl BleRuntime {
    /// Returns whether the announce was accepted, which is also the answer to
    /// "may the gossip store keep this packet".
    pub(super) async fn on_announce(
        &mut self,
        link: &LinkId,
        packet: &BitchatPacket,
        claimed: &PeerId,
    ) -> bool {
        let local = self.identity.peer_id();
        let now_ms = now_millis();
        let now_s = now_secs();

        // What the trust decision needs from our own tables, read before it runs.
        //
        // `presence` and `peer_for` are facts about *our* connection state and so
        // are not forgeable; the recorded Noise key is what a key substitution
        // would be trying to overwrite, which is why the policy wants it before
        // it looks at the signature.
        let (presence, link_bound_elsewhere, existing_noise_key, existing_signing_key) =
            self.state.with(|state| {
                let record = state.peers.info(claimed);
                let pin = state.peers.pinned(claimed);
                (
                    state.links.presence(claimed),
                    state
                        .links
                        .peer_for(link)
                        .is_some_and(|bound| bound != claimed),
                    pin.map(|p| p.noise_public_key.clone())
                        .or_else(|| record.and_then(|r| r.noise_public_key.clone())),
                    pin.map(|p| p.signing_public_key.clone())
                        .or_else(|| record.and_then(|r| r.signing_public_key.clone())),
                )
            });

        // The whole inbound announce policy, in one call: decode, derive the peer
        // ID from the announced Noise key and refuse a sender that does not match
        // it, refuse our own announce coming back, refuse a stale one, refuse a
        // swapped key, and only then spend a signature verification.
        //
        // The derivation check is the load-bearing one and it was missing. A
        // packet signature verified against the signing key the announcement
        // itself carries proves only that whoever wrote the announce holds that
        // key — which every attacker does. The mesh ID *is*
        // `SHA256(noise static key)[..8]` (`MeshIdentity::peer_id`), so binding
        // the sender ID to the key is free and is what stops a device in radio
        // range announcing any identity it likes.
        let context = AnnounceLinkContext {
            direct_announce_ttl: DIRECT_ANNOUNCE_TTL,
            has_peripheral_link: presence.as_peripheral,
            has_central_link: presence.as_central,
            link_bound_to_other_peer: link_bound_elsewhere,
            existing_noise_public_key: existing_noise_key.as_deref(),
            // §98: the signing key is pinned the same way. The registry has
            // recorded it since the first announce; nothing compared it, so
            // "Verified" meant only "signed by the key this packet carries",
            // which every attacker in radio range also holds.
            existing_signing_public_key: existing_signing_key.as_deref(),
        };
        let accepted = match evaluate_announce(
            packet,
            claimed,
            &local,
            now_ms,
            &context,
            &AnnouncePreflightConfig::default(),
            |packet, signing_key| {
                <[u8; 32]>::try_from(signing_key).is_ok_and(|key| verify_packet(packet, &key))
            },
        ) {
            Ok(accepted) => accepted,
            Err(rejection) => {
                // Our own announce relayed back is routine; anything else is a
                // peer we will never show, and the reason is the only clue.
                if matches!(rejection, AnnounceRejection::SelfAnnounce) {
                    tracing::debug!(target: "mesh::ble", "own announce relayed back, ignored");
                } else {
                    tracing::info!(
                        target: "mesh::ble",
                        peer = %claimed,
                        ?rejection,
                        "announce rejected before the trust check"
                    );
                }
                return false;
            }
        };

        // bitchat drops an unverified announce outright rather than recording the
        // peer unverified (`BLEAnnounceHandler.swift:167`, `MessageHandler.kt:259`).
        // Recording it would be worse than useless: an unverified announce carries
        // an attacker-chosen noise key and nickname, so accepting it is exactly the
        // identity-poisoning their handler exists to prevent.
        if !accepted.should_upsert_peer() {
            if matches!(
                accepted.trust.distrust(),
                Some(AnnounceDistrust::KeyMismatch | AnnounceDistrust::SigningKeyMismatch)
            ) {
                self.events.send(MeshEvent::IdentityKeyChanged {
                    peer: claimed.clone(),
                });
            }
            tracing::info!(
                target: "mesh::ble",
                peer = %claimed,
                distrust = ?accepted.trust.distrust(),
                "unverified announce ignored"
            );
            return false;
        }

        if !self.state.with(|state| state.peers.can_pin(claimed)) {
            self.degraded("Peer identity storage is full; new peers are refused.".to_string());
            return false;
        }
        let announcement = accepted.announcement.clone();
        // Commit the pin before exposing the peer as trusted. A failed write
        // leaves no accepted identity that would silently lose its pin on restart.
        let persisted = self.state.with(|state| -> Result<(), String> {
            if state.peers.pinned(claimed).is_some() {
                return Ok(());
            }
            let mut candidate = state.peers.clone();
            candidate.upsert_verified_announce(claimed, &announcement, false, now_ms);
            let sealed = self
                .sealer
                .seal(&candidate.pins_to_bytes())
                .map_err(|e| e.to_string())?;
            self.storage
                .save(StorageKey::PeerPins, &sealed)
                .map_err(|e| e.to_string())
        });
        if let Err(error) = persisted {
            self.degraded(format!("Peer identity pin could not be saved: {error}"));
            return false;
        }
        let direct = VerifiedDirectAnnounce::from_packet(
            claimed.clone(),
            packet,
            accepted.is_verified(),
            DIRECT_ANNOUNCE_TTL,
        );

        let (is_new, newly_bound, link_authenticated, first_sighting, reconnected, bound_peer) =
            self.state.with(|state| {
                // Whether this announce means the peer is *reachable on a link of
                // its own*, which is not the same as "we heard from it".
                //
                // Passing `true` unconditionally, as this did, marks a peer that only
                // reached us by relay as connected — and `reconcile_connectivity` then
                // demotes it after PEER_INACTIVITY_TIMEOUT_MS because no live link is
                // bound to it, so every subsequent announce arrives as a reconnect.
                // With three devices in the room that is visible immediately: the
                // second phone's announces come in over the first phone's link and
                // flap `reconnected=true` forever.
                //
                // bitchat computes it as `hasPeripheralConnection ||
                // hasCentralSubscription || (isDirectAnnounce && !linkBoundToOtherPeer)`
                // (`BLEAnnounceHandler.swift:178`), and notes why the last clause is
                // guarded: the TTL is unsigned, so a replayed announce can look direct
                // on the replayer's link, and that must not shortcut an absent peer
                // into "connected".
                let connected = accepted.connected_flag().unwrap_or(false);

                let update =
                    state
                        .peers
                        .upsert_verified_announce(claimed, &announcement, connected, now_ms);
                // Bind first, always. `rebind_after_verified_direct` refuses an
                // *unbound* link by design — it exists for the contested case, where a
                // link already belongs to someone else and a verified direct announce
                // is the only evidence strong enough to move it. Calling only rebind
                // for a direct announce therefore never bound a fresh inbound link at
                // all, which is the normal first-contact case: the peer registered,
                // no link was bound to it, so `reconcile_connectivity` saw no live
                // presence and demoted it 8s later — and the next announce arrived as
                // a reconnect. bitchat binds a peripheral link to the claimed sender
                // on a direct announce before any signature check
                // (`BLEService.swift:2516`), so binding is not the guarded part.
                //
                // Rebinding still runs afterwards, and is still fully guarded: on a
                // link we just bound it refuses as `SamePeer`, and on a contested one
                // `bind_if_unbound` was the no-op instead.
                let newly_bound = matches!(
                    state.links.bind_if_unbound(link, claimed),
                    chatter_mesh_link::registry::BindOutcome::Bound
                );
                if let Some(direct) = direct.as_ref() {
                    state
                        .links
                        .rebind_after_verified_direct(link, direct, now_ms);
                }
                // Per-link proof only: a Noise handshake completed, or traffic
                // decrypted, on *this* link from this peer. A peer-level session is
                // not enough — it outlives its link, and a replayed announce can
                // rebind an attacker's link to the victim's ID.
                let authenticated = state.links.is_noise_authenticated(link, claimed);
                (
                    update.is_new_peer || update.was_disconnected,
                    newly_bound,
                    authenticated,
                    update.is_new_peer,
                    update.was_disconnected,
                    state.links.peer_for(link).cloned(),
                )
            });

        // Hang up any duplicate ACL to this peer. The announce is the only thing
        // that reveals identity — the address rotated out from under us — so this
        // is the first moment the duplicate can be recognised as one.
        let redundant = self.state.with(|state| {
            let doomed = redundant_central_links_locked(state, claimed);
            // Stop the address being dialled straight back. Closing the link on its
            // own is a loop: the peer is still advertising that address, discovery
            // reports it as a device we do not hold, and we dial it again — measured
            // as a prune at 03:43:34 followed by a redial of the same address 8s
            // later. The address, not the peer, is what discovery is keyed on.
            for link in &doomed {
                if let Some(address) = state.address_of.get(link).cloned() {
                    state.scheduler.note_duplicate_peer(&address, now_ms);
                }
            }
            doomed
        });
        for link in redundant {
            tracing::info!(
                target: "mesh::ble",
                peer = %claimed,
                %link,
                "closing a duplicate link to a peer we already hold"
            );
            let _ = self.radio.disconnect(&link).await;
        }

        self.emit_peers();
        // The two halves are reported separately on purpose. `is_new` is
        // `first_sighting || reconnected`, and conflating them hides the
        // interesting case: a peer that keeps *reconnecting* was demoted for
        // silence between announces, which only happens when no live link is
        // bound to it. That is a binding problem wearing a discovery problem's
        // clothes, and one field cannot tell you which you have.
        tracing::debug!(
            target: "mesh::ble",
            peer = %claimed,
            nickname = %announcement.nickname,
            first_sighting,
            reconnected,
            link = %link,
            bound_to = ?bound_peer,
            "announce accepted"
        );

        // Hand over any mail we are carrying for them.
        //
        // The ingress that releases it is deliberately strict: a *signature
        // verified* direct announce arriving on a link that is Noise
        // authenticated for this peer right now. A session outlives its link, so
        // "we have a session with X" is not enough — a replayed announce could
        // otherwise rebind X's ID to the replayer's link and drain their mail.
        let ingress = AnnounceIngress::new(direct.is_some(), link_authenticated);
        if let Ok(key) = <[u8; 32]>::try_from(announcement.noise_public_key.as_slice()) {
            if ingress.releases_mail() {
                self.hand_over_courier_mail(claimed, &key, now_s, ingress)
                    .await;
            } else if direct.is_some()
                && self.couriers.carries_for(&key, now_s)
                && !self
                    .state
                    .with(|state| state.sessions.is_handshaking(claimed))
            {
                // We carry mail for a peer that is here, direct, but has not
                // proved its key on this link. A handshake is that proof; once
                // it completes, the link is marked and its next announce
                // releases the mail.
                self.start_handshake(claimed).await;
            }
        }

        self.retry_pending_dm_handshake(claimed).await;
        // A peer we have authenticated before (pinned) and that is directly in
        // range, with no session: re-open one now. After either side restarts
        // its radio, the restarted side has no session and nothing else starts
        // one until a DM, carried mail or a tap needs it, so private channel
        // admissions, key updates and leave notices stalled indefinitely.
        // Strangers are left alone; Noise to them stays on demand.
        if direct.is_some()
            && self.state.with(|state| {
                // A handshake whose reply was lost on a link that was still
                // coming up never completes and was never retried; restart it
                // once it is older than the stall window.
                let stalled = state.sessions.is_handshaking(claimed)
                    && now_millis().saturating_sub(
                        *state.dm_handshake_at_ms.get(claimed).unwrap_or(&0),
                    ) >= KNOWN_PEER_HANDSHAKE_STALL_MS;
                state.peers.pinned(claimed).is_some()
                    && !state.sessions.has_session(claimed)
                    && (!state.sessions.is_handshaking(claimed) || stalled)
            })
            && self.state.can_write_to(claimed)
        {
            tracing::info!(target: "mesh::ble", peer = %claimed, "re-opening Noise with a known peer");
            self.start_handshake(claimed).await;
        }
        if is_new || newly_bound {
            // Push anything retained for them, and offer them mail for others.
            let _ = self.router.send(RouterCommand::PeerAvailable {
                peer: claimed.clone(),
            });
            // A peer we have not synced with yet gets our public history.
            if let Ok(send) = self.gossip.initial_sync_to(claimed, now_s) {
                self.dispatch_sync(send).await;
            }
            // A newly reachable verified peer can carry previously queued
            // Signal ciphertext for some other verified recipient.
            if let Ok(Some((owner, _))) = self.key_store.mesh_local_account_device()
                && let Ok(pending) = self
                    .binding_store
                    .pending_mesh_courier_carriers(&owner, now_s)
            {
                for (recipient, carrier) in pending {
                    if recipient != *claimed {
                        self.send_signal_carrier(&recipient, carrier).await;
                    }
                }
            }
        }

        true
    }
}

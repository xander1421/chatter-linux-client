use super::*;

/// How long a handshake with a known peer may sit unanswered before the next
/// direct announce from that peer restarts it.
const KNOWN_PEER_HANDSHAKE_STALL_MS: u64 = 10_000;

pub(super) fn known_peer_retry_delay_ms(attempts: u8, jitter_ms: u64) -> u64 {
    let exponent = u32::from(attempts.saturating_sub(1).min(5));
    KNOWN_PEER_HANDSHAKE_STALL_MS * (1u64 << exponent) + jitter_ms
}

impl BleRuntime {
    /// Returns whether the announce was accepted, which is also the answer to
    /// "may the gossip store keep this packet".
    pub(super) async fn on_announce(
        &mut self,
        link: &LinkId,
        packet: &BitchatPacket,
        claimed: &PeerId,
    ) -> bool {
        if self.state.route(claimed) != *claimed {
            return false;
        }
        let local = self.identity.peer_id();
        let now_ms = now_millis();
        let now_s = now_secs();
        let (presence, link_bound_elsewhere, pin) = self.state.with(|state| {
            (
                state.links.presence(claimed),
                state
                    .links
                    .peer_for(link)
                    .is_some_and(|bound| bound != claimed),
                state.peers.pinned(claimed).cloned(),
            )
        });

        // The signature proves control of the announced signing key. The
        // public Noise key alone provides no proof of identity. Only a
        // Noise-bound pin can veto a later announce for this peer.
        let context = AnnounceLinkContext {
            direct_announce_ttl: DIRECT_ANNOUNCE_TTL,
            has_peripheral_link: presence.as_peripheral,
            has_central_link: presence.as_central,
            link_bound_to_other_peer: link_bound_elsewhere,
            existing_noise_public_key: pin.as_ref().map(|pin| pin.noise_public_key.as_slice()),
            existing_signing_public_key: pin.as_ref().map(|pin| pin.signing_public_key.as_slice()),
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

        // Renewals are free. A different unproven key pair for the same peer
        // is a new candidate and consumes the ingress budget.
        let announcement = &accepted.announcement;
        let renewal = pin.is_some()
            || self.state.with(|state| {
                state.peers.info(claimed).is_some_and(|record| {
                    record.noise_public_key.as_deref()
                        == Some(announcement.noise_public_key.as_slice())
                        && record.signing_public_key.as_deref()
                            == Some(announcement.signing_public_key.as_slice())
                })
            });
        if !renewal {
            let direct = packet.ttl == DIRECT_ANNOUNCE_TTL && !link_bound_elsewhere;
            let key = (link.to_string(), direct);
            self.announce_ingress
                .retain(|_, (start, _)| now_ms.saturating_sub(*start) < 60_000);
            if self.announce_ingress.len() >= 128 && !self.announce_ingress.contains_key(&key) {
                let oldest_relay = self
                    .announce_ingress
                    .iter()
                    .filter(|((_, direct), _)| !direct)
                    .min_by_key(|(_, (start, _))| *start)
                    .map(|(key, _)| key.clone());
                let Some(oldest_relay) = oldest_relay.filter(|_| direct) else {
                    return false;
                };
                self.announce_ingress.remove(&oldest_relay);
            }
            let budget = self.announce_ingress.entry(key).or_insert((now_ms, 0));
            if budget.1 >= 60 {
                return false;
            }
            budget.1 += 1;
        }

        let direct = accepted.direct_hint;
        let paired_contact = direct
            .then(|| self.state.paired_contact_for_announce(announcement))
            .flatten();
        if let Some(contact) = &paired_contact {
            self.note_pairing_candidate(claimed, contact, now_ms);
        }

        let (is_new, new_discovery, link_authenticated, first_sighting, reconnected, bound_peer) =
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

                let update = state.peers.upsert_verified_announce_preserving(
                    claimed,
                    announcement,
                    connected,
                    now_ms,
                    |peer| state.links.presence(peer).any(),
                );
                // The TTL is unsigned. A direct announce can supply an
                // address for discovery traffic; only Noise promotes it to a
                // binding used for routing and registry eviction protection.
                let new_discovery = direct && state.links.note_discovery_peer(link, claimed);
                // Per-link proof only: a Noise handshake completed, or traffic
                // decrypted, on *this* link from this peer. A peer-level session is
                // not enough — it outlives its link, and another link can
                // advertise the same peer ID.
                let authenticated = state.links.is_noise_authenticated(link, claimed);
                (
                    update.is_new_peer || update.was_disconnected,
                    new_discovery,
                    authenticated,
                    update.is_new_peer,
                    update.was_disconnected,
                    state.links.peer_for(link).cloned(),
                )
            });

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
        // A verified direct announce on an authenticated link can retry the
        // handover. The courier retains each envelope until its recipient confirms it.
        let ingress = AnnounceIngress::new(direct, link_authenticated);
        if ingress.permits_offer() {
            let claimed_keys = self
                .courier_claims
                .get(claimed)
                .cloned()
                .unwrap_or_default();
            for key in claimed_keys {
                self.hand_over_courier_mail(claimed, &key, now_s, ingress)
                    .await;
            }
        }
        if let Ok(key) = <[u8; 32]>::try_from(announcement.noise_public_key.as_slice()) {
            if ingress.permits_offer() {
                self.hand_over_courier_mail(claimed, &key, now_s, ingress)
                    .await;
            } else if direct
                && self.couriers.carries_for(&key, now_s)
                && !self
                    .state
                    .with(|state| state.sessions.is_handshaking(claimed))
            {
                // We carry mail for a peer that is here, direct, but has not
                // proved its key on this link. A handshake is that proof; once
                // it completes, the direct link is marked and offered the mail.
                self.start_handshake(claimed).await;
            }
        }

        self.retry_pending_dm_handshake(claimed).await;
        if paired_contact.is_some() && self.state.has_session(claimed) {
            self.send_pairing_proof(claimed).await;
        }
        // A known peer may return on a new link, where an old session alone
        // cannot authorize routing. Reopen Noise to restore private delivery.
        // Strangers still open Noise on demand.
        if direct
            && self.state.with(|state| {
                // A handshake whose reply was lost on a link that was still
                // coming up never completes and was never retried; restart it
                // once it is older than the stall window.
                let last_open = state.dm_handshake_at_ms.get(claimed).copied();
                let attempts = state
                    .known_peer_reopen_attempts
                    .get(claimed)
                    .copied()
                    .unwrap_or(0);
                let jitter_ms = claimed
                    .to_routing_bytes()
                    .map(|bytes| u64::from_be_bytes(bytes) % 1_000)
                    .unwrap_or(0);
                let due = last_open.is_none_or(|last| {
                    now_ms.saturating_sub(last) >= known_peer_retry_delay_ms(attempts, jitter_ms)
                });
                let has_session = state.sessions.has_session(claimed);
                let known = has_session
                    || state.peers.pinned(claimed).is_some()
                    || paired_contact.is_some();
                known && (!has_session || !state.links.presence(claimed).any()) && due
            })
            && self.state.can_open_session_to(claimed)
        {
            let first_attempt = self.state.with(|state| {
                let attempts = state
                    .known_peer_reopen_attempts
                    .entry(claimed.clone())
                    .or_default();
                *attempts = attempts.saturating_add(1);
                *attempts == 1
            });
            if first_attempt {
                tracing::info!(target: "mesh::ble", peer = %claimed, "re-opening Noise with a known peer");
            }
            self.start_handshake(claimed).await;
        }
        let direct_discovery = direct
            && self
                .state
                .with(|state| state.links.discovery_peer_for(link) == Some(claimed));
        if new_discovery || (is_new && (link_authenticated || direct_discovery)) {
            // Push anything retained for them, and offer them mail for others.
            let _ = self.router.send(RouterCommand::PeerAvailable {
                peer: self.state.contact(claimed),
            });
            self.gossip.set_public_chat_enabled(self.config.public_chat_enabled());
            if let Ok(send) = self.gossip.initial_sync_to(claimed, now_s) {
                let packet = self.maybe_sign(send.packet, send.sign_before_send);
                self.transmit_on_link(&packet, claimed, link).await;
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

    pub(super) async fn prune_duplicate_links(&self, claimed: &PeerId, now_ms: u64) {
        // Only confirmed link owners can trigger pruning of another link.
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
    }
}

#[cfg(test)]
mod retry_tests {
    use super::*;

    #[test]
    fn known_peer_retries_back_off_and_cap_after_first_fast_retry() {
        assert_eq!(known_peer_retry_delay_ms(1, 0), 10_000);
        assert_eq!(known_peer_retry_delay_ms(2, 0), 20_000);
        assert_eq!(known_peer_retry_delay_ms(3, 250), 40_250);
        assert_eq!(known_peer_retry_delay_ms(8, 999), 320_999);
    }
}

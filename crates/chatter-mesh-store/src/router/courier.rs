use super::{MessageRouter, RouterEvent};
use crate::outbox::QueuedMessage;
use crate::transport::PeerSnapshot;
use chatter_mesh::ids::PeerId;
use std::collections::BTreeSet;

/// A peer who can be asked to carry a sealed envelope.
struct CourierCandidate {
    peer: PeerId,
    noise_key: [u8; 32],
}

impl MessageRouter {
    /// Seal the message to the recipient's known static key and hand it to peers who
    /// may physically encounter them.
    ///
    /// Only signature-verified peers carry mail; the envelopes are opaque to them.
    /// The retained copy stays retained, so a direct delivery still wins if the
    /// peer reappears first.
    pub(super) fn attempt_courier_deposit(
        &mut self,
        message_id: &str,
        peer: &PeerId,
    ) -> Vec<RouterEvent> {
        let Some(recipient_key) = self.directory.noise_key(peer) else {
            return Vec::new();
        };
        let Some(entry) = self.outbox.queued(peer, message_id).cloned() else {
            return Vec::new();
        };

        // The bridge drop needs no connected courier — only the recipient key — so
        // it runs before any courier-slot bookkeeping, and independently of it.
        let mut events = Vec::new();
        events.extend(self.request_bridge_deposit(&entry, peer, &recipient_key));

        let remaining = entry.remaining_courier_slots();
        if remaining == 0 {
            return events;
        }

        for index in 0..self.transports.len() {
            let couriers = self.eligible_couriers(
                index,
                &recipient_key,
                &entry.deposited_courier_keys,
                remaining,
            );
            if couriers.is_empty() {
                continue;
            }
            let via: Vec<PeerId> = couriers.iter().map(|c| c.peer.clone()).collect();
            let accepted = self.transports[index].send_courier(
                &entry.content,
                message_id,
                &recipient_key,
                &via,
            );
            // Credit only the couriers that actually took it. Crediting every
            // candidate would burn the message's remaining slots against carriers
            // that never received it, and no further deposit would be attempted —
            // the mail then rides with fewer couriers than the outbox believes.
            let keys: Vec<[u8; 32]> = couriers
                .iter()
                .filter(|c| accepted.contains(&c.peer))
                .map(|c| c.noise_key)
                .collect();
            if keys.is_empty() {
                continue;
            }
            self.outbox.record_courier_deposit(peer, message_id, &keys);
            self.persist_into(&mut events);
            events.push(RouterEvent::Carried {
                message_id: message_id.to_string(),
                peer: peer.clone(),
            });
            return events;
        }
        events
    }

    /// A courier candidate just connected: hand it any queued mail it is not already
    /// carrying.
    ///
    /// This is what turns couriering from "a courier happened to be nearby at send
    /// time" into eventual spread — deposits retry as eligible peers appear, until
    /// each message rides with its full complement of distinct couriers or expires.
    pub fn courier_became_available(
        &mut self,
        peer: &PeerId,
        now_unix_secs: u64,
    ) -> Vec<RouterEvent> {
        let mut events = Vec::new();

        for index in 0..self.transports.len() {
            if !self.transports[index].is_peer_connected(peer) {
                continue;
            }
            // Only a signature-verified peer may carry mail, and only one whose
            // static key we know — the envelope has to be addressed.
            let Some(courier_key) = self.transports[index]
                .peer_snapshots()
                .into_iter()
                .find(|s| &s.peer == peer && s.is_connected)
                .and_then(|s| {
                    let key = s.noise_public_key?;
                    s.is_verified.then_some(key)
                })
            else {
                continue;
            };

            // Snapshot the queues first: recording a deposit mutates the outbox.
            let queues: Vec<(PeerId, Vec<QueuedMessage>)> = self
                .outbox
                .iter()
                .map(|(recipient, queue)| (recipient.clone(), queue.to_vec()))
                .collect();

            for (recipient, queue) in queues {
                // Mail *to* this peer flushes directly on connect. Handing someone
                // their own mail back as an opaque envelope achieves nothing.
                if &recipient == peer {
                    continue;
                }
                let Some(recipient_key) = self.directory.noise_key(&recipient) else {
                    continue;
                };
                if recipient_key == courier_key {
                    continue;
                }

                for message in queue {
                    if message.remaining_courier_slots() == 0
                        || message.is_carried_by(&courier_key)
                        || message.is_expired(now_unix_secs)
                    {
                        continue;
                    }
                    let accepted = self.transports[index].send_courier(
                        &message.content,
                        &message.message_id,
                        &recipient_key,
                        std::slice::from_ref(peer),
                    );
                    // Single-courier deposit, so acceptance is membership rather
                    // than a subset filter.
                    if !accepted.contains(peer) {
                        continue;
                    }
                    self.outbox.record_courier_deposit(
                        &recipient,
                        &message.message_id,
                        &[courier_key],
                    );
                    events.push(RouterEvent::Carried {
                        message_id: message.message_id.clone(),
                        peer: recipient.clone(),
                    });
                }
            }

            self.persist_into(&mut events);
            // bitchat stops at the first transport this peer is an eligible courier
            // on: a second would only hand the same peer a duplicate.
            return events;
        }
        events
    }

    /// Verified candidates on one transport, in snapshot order, capped at `limit`.
    fn eligible_couriers(
        &self,
        transport_index: usize,
        recipient_key: &[u8; 32],
        excluded: &BTreeSet<[u8; 32]>,
        limit: usize,
    ) -> Vec<CourierCandidate> {
        if limit == 0 {
            return Vec::new();
        }
        self.transports[transport_index]
            .peer_snapshots()
            .into_iter()
            .filter_map(|snapshot: PeerSnapshot| {
                if !snapshot.is_connected {
                    return None;
                }
                // No static key means nothing can be sealed to them, so they cannot
                // be handed an envelope either.
                let key = snapshot.noise_public_key?;
                // Never hand the recipient their own mail, and never re-burn a
                // courier already carrying this message.
                if &key == recipient_key || excluded.contains(&key) {
                    return None;
                }
                // Unverified strangers may not carry mail at all.
                if !snapshot.is_verified {
                    return None;
                }
                Some(CourierCandidate {
                    peer: snapshot.peer,
                    noise_key: key,
                })
            })
            .take(limit)
            .collect()
    }

    fn request_bridge_deposit(
        &self,
        message: &QueuedMessage,
        peer: &PeerId,
        recipient_key: &[u8; 32],
    ) -> Option<RouterEvent> {
        let deposit = self.bridge_deposit.as_ref()?;
        // The drop layer dedups by message ID, so re-depositing is cheap and this
        // does not have to track in-flight IDs itself.
        deposit(
            message.content.as_str(),
            message.message_id.as_str(),
            recipient_key,
        )
        .then(|| RouterEvent::Carried {
            message_id: message.message_id.clone(),
            peer: peer.clone(),
        })
    }

    /// Re-attempts bridge drops for retained messages that no transport can promptly
    /// reach.
    ///
    /// This covers sends that raced the reachability retention window: a peer stays
    /// "reachable" for a minute after its radio disappears, so the original send
    /// trusted the mesh and skipped the deposit — and nothing else ever retried.
    /// bitchat found this in the field. Safe to call often; the drop layer dedups.
    pub fn retry_bridge_deposits(&mut self, now_unix_secs: u64) -> Vec<RouterEvent> {
        if self.bridge_deposit.is_none() {
            return Vec::new();
        }
        let mut events = Vec::new();
        for peer in self.outbox.peers() {
            let Some(recipient_key) = self.directory.noise_key(&peer) else {
                continue;
            };
            // Note `is_peer_reachable && can_deliver_promptly` on the *same*
            // transport: a peer that one transport claims is reachable and another
            // could deliver promptly to is not actually promptly deliverable.
            let promptly = self
                .transports
                .iter()
                .any(|t| t.is_peer_reachable(&peer) && t.can_deliver_promptly(&peer));
            if promptly {
                continue;
            }
            for message in self.outbox.queue(&peer).to_vec() {
                if message.is_expired(now_unix_secs) {
                    continue;
                }
                events.extend(self.request_bridge_deposit(&message, &peer, &recipient_key));
            }
        }
        events
    }
}

//! The Nostr runtime: geohash channels, gift-wrapped DMs, and courier drops.
//!
//! `chatter_mesh_nostr` is sans-IO above the socket and owns no timers, so the
//! relay pool has to be driven by someone. This is that someone: one task that
//! multiplexes commands from the rest of the service against the pool's event
//! stream, keeps subscriptions in step with the cells the user has joined, and
//! maintains the *synchronously readable* relay-connectivity flag that
//! `can_deliver_promptly` depends on.
//!
//! # Identities are per cell, and that is the privacy boundary
//!
//! Every message published into a cell is signed by an identity derived for that
//! cell alone. Two cells never share a pubkey, so a relay operator cannot join a
//! user's movements into a track. Nothing here may ever publish into one cell
//! under another cell's key.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::crypto::os_rng;
use chatter_mesh::ids::PeerId;
use chatter_mesh::packet::{MessageType, NoisePayloadType, decode_noise_payload};
use chatter_mesh::tlv::private_message::PrivateMessagePacket;
use chatter_mesh_nostr::identity::NostrIdentity;
use chatter_mesh_nostr::relay::{RelayEvent, RelayFilter, RelayPool};
use chatter_mesh_nostr::types::{EventKind, NostrEvent};
use chatter_mesh_nostr::{NostrError, builders, embed, geohash, wrap};
use chatter_mesh_store::bridge_drop::DropOperationId;
use chatter_mesh_store::outbox::AckKind;
use tokio::sync::mpsc::UnboundedReceiver;

use super::config::MeshConfig;
use super::events::{MeshEvent, MeshMessage};
use super::identity::NostrIdentities;
use super::mapping::{InboundMessage, MeshConversation};
use super::router::{RouterCommand, RouterHandle};
use super::state::{MeshState, NostrState, now_millis, now_secs};
use super::timeline::MeshEvents;
use super::transport::NostrCommand;

/// How far back to ask for gift wraps on (re)subscribe.
const DM_LOOKBACK_SECS: i64 = 86_400;
/// How far back to ask for courier drops.
const DROP_LOOKBACK_SECS: i64 = 86_400;
/// How many stored drops a relay may replay at once.
const DROP_LIMIT: usize = 200;
/// How many live geohash messages a relay may forward at once.
const GEOHASH_LIMIT: usize = 200;

/// Subscription id for a cell's public channel.
fn geo_subscription(cell: &str) -> String {
    format!("geo:{cell}")
}

/// Subscription id for a cell's gift wraps.
fn dm_subscription(cell: &str) -> String {
    format!("dm:{cell}")
}

/// Subscription id for courier drops addressed to tags we watch.
const DROP_SUBSCRIPTION: &str = "drops";

/// The public identity of a geohash participant.
fn chat_peer(pubkey: &str) -> PeerId {
    PeerId::from_str(&format!("{}{pubkey}", chatter_mesh::ids::PREFIX_GEO_CHAT))
}

/// The DM identity of a geohash participant.
fn dm_peer(pubkey: &str) -> PeerId {
    PeerId::from_str(&format!("{}{pubkey}", chatter_mesh::ids::PREFIX_GEO_DM))
}

/// The relay runtime.
pub struct NostrRuntime {
    pool: Arc<RelayPool>,
    state: Arc<NostrState>,
    /// The radio's state, read only so that a status event can report both
    /// transports at once rather than half of one.
    mesh_state: Arc<MeshState>,
    identities: NostrIdentities,
    config: Arc<MeshConfig>,
    events: MeshEvents,
    router: Arc<RouterHandle>,
    relays: Vec<String>,
    /// Recipient tags currently watched for courier drops.
    watching: Vec<String>,
    /// Published drop event id → the bridge's operation handle, so a relay's
    /// `OK` can be resolved against the attempt that produced it.
    pending_drops: BTreeMap<String, DropOperationId>,
}

impl NostrRuntime {
    /// Wire a runtime around an already-built pool.
    #[must_use]
    pub fn new(
        pool: Arc<RelayPool>,
        state: Arc<NostrState>,
        mesh_state: Arc<MeshState>,
        identities: NostrIdentities,
        config: Arc<MeshConfig>,
        events: MeshEvents,
        router: Arc<RouterHandle>,
    ) -> Self {
        let relays = config.relay_urls();
        Self {
            pool,
            state,
            mesh_state,
            identities,
            config,
            events,
            router,
            relays,
            watching: Vec::new(),
            pending_drops: BTreeMap::new(),
        }
    }

    /// The identity for a cell, as an owned value.
    ///
    /// Owned rather than borrowed on purpose: a `&NostrIdentity` borrowed out of
    /// `self.identities` keeps `&mut self` pinned for as long as it lives, and
    /// every publish path needs `&mut self` again to report a failure. The type
    /// is `Clone` and the clone is a hex string plus 32 bytes.
    fn cell_identity(&mut self, cell: &str) -> Result<NostrIdentity, NostrError> {
        self.identities.for_geohash(cell).cloned()
    }

    /// Whether a pubkey is one of ours, so our own echo is not shown twice.
    fn is_own_pubkey(&self, pubkey: &str) -> bool {
        self.identities
            .geohash_pubkeys()
            .iter()
            .any(|(_, own)| own == pubkey)
    }

    /// Connect, join the configured cells, then multiplex until told to stop.
    ///
    /// The two receivers are parameters rather than fields so that a `select!`
    /// branch borrowing one of them does not also borrow the whole runtime,
    /// which every handler needs mutably.
    pub async fn run(
        mut self,
        mut commands: UnboundedReceiver<NostrCommand>,
        mut relay_events: UnboundedReceiver<RelayEvent>,
    ) {
        self.pool.ensure_relays(&self.relays).await;
        for cell in self.config.geohashes.clone() {
            self.join(&cell).await;
        }

        loop {
            tokio::select! {
                command = commands.recv() => match command {
                    Some(NostrCommand::Stop) | None => break,
                    Some(command) => self.on_command(command).await,
                },
                event = relay_events.recv() => match event {
                    Some(event) => self.on_relay_event(event).await,
                    // The pool's dispatcher is gone; nothing further will arrive.
                    None => break,
                },
            }
        }

        self.pool.shutdown().await;
        self.state.with(|state| state.connected_relays.clear());
    }

    // ---------------------------------------------------------------- commands

    async fn on_command(&mut self, command: NostrCommand) {
        match command {
            NostrCommand::Stop => {}
            NostrCommand::Join(cell) => self.join(&cell).await,
            NostrCommand::Leave(cell) => self.leave(&cell).await,
            NostrCommand::GeohashMessage {
                cell,
                message_id,
                content,
                nickname,
            } => {
                self.publish_geohash(&cell, &message_id, &content, &nickname)
                    .await
            }
            NostrCommand::Dm {
                peer,
                message_id,
                content,
            } => self.publish_dm(&peer, &message_id, &content).await,
            NostrCommand::Ack {
                peer,
                message_id,
                payload_type,
            } => self.publish_ack(&peer, &message_id, payload_type).await,
            NostrCommand::PublishDrop {
                operation,
                tag,
                sealed,
                expires_at_unix_secs,
                publisher_secret,
            } => {
                self.publish_drop(
                    operation,
                    &tag,
                    &sealed,
                    expires_at_unix_secs,
                    publisher_secret,
                )
                .await;
            }
            NostrCommand::WatchTags(tags) => self.watch_tags(&tags).await,
            NostrCommand::CloseWatch => {
                if !self.watching.is_empty() {
                    self.pool.unsubscribe(DROP_SUBSCRIPTION).await;
                    self.watching.clear();
                }
            }
        }
    }

    async fn join(&mut self, cell: &str) {
        if !geohash::is_valid(cell) {
            self.degraded(format!("not a geohash: {cell}"));
            return;
        }
        if self
            .state
            .with(|state| !state.joined.insert(cell.to_string()))
        {
            return;
        }

        let since = now_secs() as i64 - 3_600;
        let chat = RelayFilter::geohash_ephemeral(cell, Some(since), GEOHASH_LIMIT);
        if let Err(error) = self
            .pool
            .subscribe(&geo_subscription(cell), &[chat], &self.relays)
            .await
        {
            self.degraded(format!("could not join {cell}: {error}"));
        }

        // Gift wraps are addressed to the cell's derived pubkey, so the DM
        // subscription is per cell too.
        let pubkey = self
            .identities
            .for_geohash(cell)
            .map(|identity| identity.public_key_hex().to_string());
        match pubkey {
            Ok(pubkey) => {
                let filter = RelayFilter::gift_wraps_for(
                    &pubkey,
                    Some(now_secs() as i64 - DM_LOOKBACK_SECS),
                );
                if let Err(error) = self
                    .pool
                    .subscribe(&dm_subscription(cell), &[filter], &self.relays)
                    .await
                {
                    self.degraded(format!("could not watch DMs in {cell}: {error}"));
                }
            }
            Err(error) => self.degraded(format!("no identity for {cell}: {error}")),
        }

        self.events.send(MeshEvent::GeohashJoined(cell.to_string()));
    }

    async fn leave(&mut self, cell: &str) {
        if self.state.with(|state| !state.joined.remove(cell)) {
            return;
        }
        self.pool.unsubscribe(&geo_subscription(cell)).await;
        self.pool.unsubscribe(&dm_subscription(cell)).await;
        // Drop the derived keys with the cell: a cell the user has walked out of
        // should stop being derivable from live state.
        self.identities.forget(cell);
        self.state.with(|state| {
            state.dm_origin.retain(|_, origin| origin.as_str() != cell);
        });
        self.events.send(MeshEvent::GeohashLeft(cell.to_string()));
    }

    async fn publish_geohash(
        &mut self,
        cell: &str,
        message_id: &str,
        content: &str,
        nickname: &str,
    ) {
        let nickname = (!nickname.trim().is_empty()).then(|| nickname.trim());
        let identity = match self.cell_identity(cell) {
            Ok(identity) => identity,
            Err(error) => {
                self.degraded(format!("no identity for {cell}: {error}"));
                return;
            }
        };

        let event = builders::geohash_message(
            cell,
            content,
            nickname,
            // Not teleported: this service publishes into a cell only when the
            // user is in it, and a teleport flag would be a claim the recipient
            // cannot check.
            false,
            &identity,
            now_secs() as i64,
        );

        match event {
            Ok(event) => {
                if let Err(error) = self.pool.publish(&event, &self.relays).await {
                    self.degraded(format!("could not publish to {cell}: {error}"));
                    return;
                }
                // Mirror it into the timeline immediately: there is no server
                // round trip to echo it back, so the local copy *is* the
                // message.
                let sender = chat_peer(identity.public_key_hex());
                self.emit_local(
                    MeshConversation::Geohash(cell.to_string()),
                    message_id,
                    &sender,
                    content,
                );
            }
            Err(error) => self.degraded(format!("could not build a message for {cell}: {error}")),
        }
    }

    async fn publish_dm(&mut self, peer: &PeerId, message_id: &str, content: &str) {
        let Some(recipient) = super::state::recipient_pubkey(peer) else {
            self.degraded(format!("no nostr key for {peer}"));
            return;
        };
        let Some(cell) = self.state.dm_origin(peer) else {
            // Without knowing which cell the conversation lives in there is no
            // identity to sign as, and signing with the wrong cell's key would
            // link two cells together.
            self.degraded(format!("no geohash context for a DM to {peer}"));
            return;
        };

        let identity = match self.cell_identity(&cell) {
            Ok(identity) => identity,
            Err(error) => {
                self.degraded(format!("no identity for {cell}: {error}"));
                return;
            }
        };
        let sender = PeerId::from_str(identity.public_key_hex());
        let rumor = match embed::embed_private_message_no_recipient(
            content,
            message_id,
            &sender,
            now_millis(),
        ) {
            Ok(rumor) => rumor,
            Err(error) => {
                self.degraded(format!("could not embed a DM: {error}"));
                return;
            }
        };

        let wrapped = wrap::gift_wrap(
            &rumor,
            &recipient,
            &identity,
            now_secs() as i64,
            &mut os_rng(),
        );
        match wrapped {
            Ok(event) => {
                if let Err(error) = self.pool.publish(&event, &self.relays).await {
                    self.degraded(format!("could not publish a DM: {error}"));
                }
                // No local mirror here. Every DM enters through
                // `MeshService::send_dm`, which mirrors it once for whichever
                // transport the router picks — mirroring again would put the
                // message in the timeline twice for Nostr and once for BLE.
            }
            Err(error) => self.degraded(format!("could not gift wrap a DM: {error}")),
        }
    }

    async fn publish_ack(
        &mut self,
        peer: &PeerId,
        message_id: &str,
        payload_type: NoisePayloadType,
    ) {
        let Some(recipient) = super::state::recipient_pubkey(peer) else {
            return;
        };
        let Some(cell) = self.state.dm_origin(peer) else {
            return;
        };
        let Ok(identity) = self.cell_identity(&cell) else {
            return;
        };
        let sender = PeerId::from_str(identity.public_key_hex());
        let Ok(rumor) = embed::embed_ack(payload_type, message_id, None, &sender, now_millis())
        else {
            return;
        };
        if let Ok(event) = wrap::gift_wrap(
            &rumor,
            &recipient,
            &identity,
            now_secs() as i64,
            &mut os_rng(),
        ) {
            let _ = self.pool.publish(&event, &self.relays).await;
        }
    }

    async fn publish_drop(
        &mut self,
        operation: DropOperationId,
        tag: &[u8; 16],
        sealed: &[u8],
        expires_at_unix_secs: u64,
        publisher_secret: [u8; 32],
    ) {
        let identity = match NostrIdentity::from_secret(publisher_secret) {
            Ok(identity) => identity,
            Err(error) => {
                self.resolve_drop(operation, false);
                self.degraded(format!("courier drop key was unusable: {error}"));
                return;
            }
        };
        let event = builders::courier_drop(
            tag,
            sealed,
            expires_at_unix_secs as i64,
            &identity,
            now_secs() as i64,
        );
        match event {
            Ok(event) => {
                let event_id = event.id.clone();
                match self.pool.publish(&event, &self.relays).await {
                    Ok(0) | Err(_) => self.resolve_drop(operation, false),
                    Ok(_) => {
                        // Not resolved yet: acceptance is the relay's `OK`, not
                        // the socket write. Parking the operation here is what
                        // lets that `OK` find it.
                        self.pending_drops.insert(event_id, operation);
                    }
                }
            }
            Err(error) => {
                self.resolve_drop(operation, false);
                self.degraded(format!("could not build a courier drop: {error}"));
            }
        }
    }

    async fn watch_tags(&mut self, tags: &[[u8; 16]]) {
        let hex_tags: Vec<String> = tags.iter().map(hex::encode).collect();
        if hex_tags == self.watching {
            return;
        }
        if hex_tags.is_empty() {
            self.pool.unsubscribe(DROP_SUBSCRIPTION).await;
            self.watching.clear();
            return;
        }
        let filter = RelayFilter::courier_drops(
            &hex_tags,
            Some(now_secs() as i64 - DROP_LOOKBACK_SECS),
            DROP_LIMIT,
        );
        if let Err(error) = self
            .pool
            .subscribe(DROP_SUBSCRIPTION, &[filter], &self.relays)
            .await
        {
            self.degraded(format!("could not watch for courier drops: {error}"));
            return;
        }
        self.watching = hex_tags;
    }

    // ------------------------------------------------------------ relay events

    async fn on_relay_event(&mut self, event: RelayEvent) {
        match event {
            RelayEvent::Connected { url } => {
                let changed = self.state.with(|state| state.connected_relays.insert(url));
                if changed {
                    self.emit_status();
                }
            }
            RelayEvent::Disconnected { url, .. } | RelayEvent::GaveUp { url } => {
                let changed = self.state.with(|state| state.connected_relays.remove(&url));
                if changed {
                    self.emit_status();
                }
            }
            RelayEvent::Ok {
                event_id, accepted, ..
            } => {
                if let Some(operation) = self.pending_drops.remove(&event_id) {
                    self.resolve_drop(operation, accepted);
                }
            }
            RelayEvent::Event {
                subscription_id,
                event,
                ..
            } => self.on_event(&subscription_id, event).await,
            RelayEvent::Notice { url, message } => {
                self.degraded(format!("relay {url}: {message}"));
            }
            RelayEvent::Closed {
                url,
                subscription_id,
                message,
            } => {
                self.degraded(format!(
                    "relay {url} closed subscription {subscription_id}: {message}"
                ));
            }
            RelayEvent::Eose { .. } | RelayEvent::SubscriptionComplete { .. } => {}
        }
    }

    async fn on_event(&mut self, subscription_id: &str, event: NostrEvent) {
        if let Some(cell) = subscription_id.strip_prefix("geo:") {
            let cell = cell.to_string();
            self.on_geohash_event(&cell, &event);
        } else if let Some(cell) = subscription_id.strip_prefix("dm:") {
            let cell = cell.to_string();
            self.on_gift_wrap(&cell, &event).await;
        } else if subscription_id == DROP_SUBSCRIPTION {
            self.on_courier_drop(&event);
        }
    }

    fn on_geohash_event(&mut self, cell: &str, event: &NostrEvent) {
        // Presence heartbeats carry no content; they tell us someone is here so
        // a DM to them can be addressed, and nothing more.
        let nickname = event.tag_value("n").map(str::to_string);
        self.state
            .note_participant(dm_peer(&event.pubkey), cell, nickname.clone());

        if event.event_kind() != Some(EventKind::EphemeralEvent) || event.content.is_empty() {
            return;
        }
        // Our own message, already mirrored locally when it was published.
        if self.is_own_pubkey(&event.pubkey) {
            return;
        }

        let sender = chat_peer(&event.pubkey);
        let nickname = nickname.unwrap_or_else(|| short_name(&event.pubkey));
        let inbound = InboundMessage {
            // Nostr event IDs are already globally unique content hashes, so
            // they are the natural dedup key — no synthetic ID is needed.
            wire_id: event.id.clone(),
            conversation: MeshConversation::Geohash(cell.to_string()),
            sender: sender.clone(),
            nickname: nickname.clone(),
            content: event.content.clone(),
            timestamp_ms: (event.created_at.max(0) as u64).saturating_mul(1_000),
        };
        self.events.send(MeshEvent::Message(Box::new(MeshMessage {
            message: inbound.to_domain(),
            conversation: inbound.conversation,
            sender,
            nickname,
            is_local: false,
            // The event signature proves the *pubkey* authored it. It says
            // nothing about the nickname, which is why that stays a display
            // string.
            sender_verified: false,
        })));
    }

    async fn on_gift_wrap(&mut self, cell: &str, event: &NostrEvent) {
        let Ok(identity) = self.cell_identity(cell) else {
            return;
        };
        let opened = wrap::unwrap_gift(event, identity.secret());
        let (content, sender_pubkey, created_at) = match opened {
            Ok(opened) => opened,
            // Not addressed to this cell's key, or forged. Both are routine on a
            // public relay and neither is worth reporting.
            Err(_) => return,
        };

        let peer = dm_peer(&sender_pubkey);
        self.state.note_participant(peer.clone(), cell, None);
        let nickname = self
            .state
            .nickname(&peer)
            .unwrap_or_else(|| short_name(&sender_pubkey));
        let timestamp_ms = (created_at.max(0) as u64).saturating_mul(1_000);

        // A bitchat DM is a base64url-embedded packet; a plain string is a DM
        // from some other NIP-17 client, which is still a DM.
        let Some((message_id, body, payload_type)) = decode_dm(&content) else {
            let inbound = InboundMessage {
                wire_id: event.id.clone(),
                conversation: MeshConversation::Dm(peer.clone()),
                sender: peer.clone(),
                nickname: nickname.clone(),
                content,
                timestamp_ms,
            };
            self.events.send(MeshEvent::Message(Box::new(MeshMessage {
                message: inbound.to_domain(),
                conversation: inbound.conversation,
                sender: peer,
                nickname,
                is_local: false,
                sender_verified: true,
            })));
            return;
        };

        match payload_type {
            NoisePayloadType::PrivateMessage => {
                let inbound = InboundMessage {
                    wire_id: message_id.clone(),
                    conversation: MeshConversation::Dm(peer.clone()),
                    sender: peer.clone(),
                    nickname: nickname.clone(),
                    content: body,
                    timestamp_ms,
                };
                self.events.send(MeshEvent::Message(Box::new(MeshMessage {
                    message: inbound.to_domain(),
                    conversation: inbound.conversation,
                    sender: peer.clone(),
                    nickname,
                    is_local: false,
                    // The seal's signature was verified inside `unwrap_gift`, so
                    // the sending pubkey really did author this.
                    sender_verified: true,
                })));
                // Acknowledge, so the sender's outbox can stop retaining.
                self.publish_ack(&peer, &message_id, NoisePayloadType::Delivered)
                    .await;
            }
            NoisePayloadType::Delivered => {
                let _ = self.router.send(RouterCommand::Acked {
                    message_id,
                    peer,
                    kind: AckKind::Delivered,
                });
            }
            NoisePayloadType::ReadReceipt => {
                let _ = self.router.send(RouterCommand::Acked {
                    message_id,
                    peer,
                    kind: AckKind::Read,
                });
            }
            // Group, voice, and verification payloads are out of scope for this
            // slice and are dropped rather than half-handled.
            _ => {}
        }
    }

    fn on_courier_drop(&mut self, event: &NostrEvent) {
        let Some(tag_hex) = event.tag_value("x") else {
            return;
        };
        let Some(tag) = decode_tag(tag_hex) else {
            return;
        };
        let Ok(envelope) = base64_decode(&event.content) else {
            return;
        };
        let _ = self.router.send(RouterCommand::InboundDrop {
            event_id: event.id.clone(),
            tag,
            envelope,
        });
    }

    // ------------------------------------------------------------------ helpers

    fn resolve_drop(&self, operation: DropOperationId, accepted: bool) {
        let _ = self.router.send(RouterCommand::ResolveDrop {
            operation,
            accepted,
        });
    }

    fn emit_local(
        &self,
        conversation: MeshConversation,
        message_id: &str,
        sender: &PeerId,
        content: &str,
    ) {
        let message = super::mapping::outbound_message(
            &conversation,
            message_id,
            sender,
            content,
            now_millis(),
        );
        self.events.send(MeshEvent::Message(Box::new(MeshMessage {
            message,
            conversation,
            sender: sender.clone(),
            nickname: self.config.nickname.clone(),
            is_local: true,
            sender_verified: true,
        })));
    }

    /// Report both transports at once. A status carrying only the relay count
    /// would make the UI show the radio as dead every time a relay flapped.
    fn emit_status(&self) {
        let status = self.mesh_state.status(self.state.connected_relay_count());
        self.events.send(MeshEvent::Status(status));
    }

    fn degraded(&self, message: String) {
        tracing::debug!(target: "mesh::nostr", "{message}");
        self.events.send(MeshEvent::Degraded(message));
    }
}

/// Short, stable display name for a pubkey with no nickname attached.
fn short_name(pubkey: &str) -> String {
    format!("anon{}", pubkey.get(..4).unwrap_or(pubkey))
}

fn decode_tag(hex_tag: &str) -> Option<[u8; 16]> {
    let bytes = hex::decode(hex_tag).ok()?;
    <[u8; 16]>::try_from(bytes.as_slice()).ok()
}

fn base64_decode(content: &str) -> Result<Vec<u8>, NostrError> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(content)
        .map_err(|_| NostrError::InvalidEvent("courier drop content is not base64"))
}

/// Pull a bitchat payload out of a decrypted rumor.
///
/// Returns `(message id, body, payload type)`. `None` for a rumor that is not an
/// embedded bitchat packet — a DM from a plain NIP-17 client, which is still a
/// message and is handled as plain text by the caller.
fn decode_dm(content: &str) -> Option<(String, String, NoisePayloadType)> {
    if !embed::is_embedded(content) {
        return None;
    }
    let packet = embed::decode_embedded(content).ok()?;
    if packet.message_type() != Some(MessageType::NoiseEncrypted) {
        return None;
    }
    let (payload_type, body) = decode_noise_payload(&packet.payload)?;
    match payload_type {
        NoisePayloadType::PrivateMessage => {
            let message = PrivateMessagePacket::decode(body).ok()?;
            Some((message.message_id, message.content, payload_type))
        }
        // The ack body is the message ID as raw UTF-8, not a TLV. That asymmetry
        // is bitchat's, and reading it as a TLV would silently drop every ack.
        NoisePayloadType::Delivered | NoisePayloadType::ReadReceipt => {
            let id = String::from_utf8(body.to_vec()).ok()?;
            Some((id, String::new(), payload_type))
        }
        _ => Some((String::new(), String::new(), payload_type)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chatter_mesh_noise::MeshIdentity;

    #[test]
    fn subscription_ids_separate_the_three_streams() {
        assert_eq!(geo_subscription("u4pruy"), "geo:u4pruy");
        assert_eq!(dm_subscription("u4pruy"), "dm:u4pruy");
        assert_ne!(geo_subscription("u4pruy"), dm_subscription("u4pruy"));
        assert_ne!(geo_subscription("u4pruy"), DROP_SUBSCRIPTION);
    }

    #[test]
    fn chat_and_dm_identities_use_different_namespaces() {
        let pubkey = "a".repeat(64);
        let chat = chat_peer(&pubkey);
        let dm = dm_peer(&pubkey);
        assert!(chat.is_geo_chat());
        assert!(dm.is_geo_dm());
        assert_ne!(chat, dm);
        // Only the DM form yields a gift-wrap recipient key.
        assert!(super::super::state::recipient_pubkey(&dm).is_some());
        assert!(super::super::state::recipient_pubkey(&chat).is_none());
    }

    #[test]
    fn an_embedded_private_message_round_trips() {
        let sender = PeerId::from_str(&"ab".repeat(32));
        let rumor = embed::embed_private_message_no_recipient(
            "hello mesh",
            "MSG-1",
            &sender,
            1_700_000_000_000,
        )
        .expect("embedded");
        let (message_id, body, kind) = decode_dm(&rumor).expect("decoded");
        assert_eq!(message_id, "MSG-1");
        assert_eq!(body, "hello mesh");
        assert_eq!(kind, NoisePayloadType::PrivateMessage);
    }

    #[test]
    fn an_embedded_ack_round_trips_as_a_bare_message_id() {
        let sender = PeerId::from_str(&"ab".repeat(32));
        let rumor = embed::embed_ack(
            NoisePayloadType::Delivered,
            "MSG-1",
            None,
            &sender,
            1_700_000_000_000,
        )
        .expect("embedded");
        let (message_id, body, kind) = decode_dm(&rumor).expect("decoded");
        assert_eq!(message_id, "MSG-1");
        assert!(body.is_empty());
        assert_eq!(kind, NoisePayloadType::Delivered);
    }

    #[test]
    fn a_plain_nip17_rumor_is_not_mistaken_for_a_bitchat_packet() {
        assert!(decode_dm("just a message").is_none());
        assert!(decode_dm("bitchat1:not-base64!!").is_none());
    }

    #[test]
    fn a_gift_wrap_survives_the_round_trip_between_two_cell_identities() {
        let mesh = MeshIdentity::generate(&mut os_rng());
        let mut alice = NostrIdentities::from_mesh(&mesh);
        let other = MeshIdentity::generate(&mut os_rng());
        let mut bob = NostrIdentities::from_mesh(&other);

        let alice_id = alice.for_geohash("u4pruy").expect("alice identity");
        let alice_pubkey = alice_id.public_key_hex().to_string();
        let bob_pubkey = bob
            .for_geohash("u4pruy")
            .expect("bob identity")
            .public_key_hex()
            .to_string();

        let rumor = embed::embed_private_message_no_recipient(
            "over nostr",
            "MSG-2",
            &PeerId::from_str(&alice_pubkey),
            1_700_000_000_000,
        )
        .expect("embedded");
        let wrapped = wrap::gift_wrap(&rumor, &bob_pubkey, alice_id, 1_700_000_000, &mut os_rng())
            .expect("wrapped");

        let bob_id = bob.for_geohash("u4pruy").expect("bob identity");
        let (content, sender, _) = wrap::unwrap_gift(&wrapped, bob_id.secret()).expect("unwrapped");
        assert_eq!(sender, alice_pubkey);
        let (message_id, body, _) = decode_dm(&content).expect("decoded");
        assert_eq!(message_id, "MSG-2");
        assert_eq!(body, "over nostr");
    }

    #[test]
    fn a_courier_drop_tag_survives_hex() {
        let tag = [7u8; 16];
        assert_eq!(decode_tag(&hex::encode(tag)), Some(tag));
        assert!(decode_tag("not hex").is_none());
        assert!(decode_tag("abcd").is_none(), "wrong length is not a tag");
    }

    #[test]
    fn anonymous_display_names_are_short_and_stable() {
        let pubkey = "deadbeef".repeat(8);
        assert_eq!(short_name(&pubkey), "anondead");
        assert_eq!(short_name(&pubkey), short_name(&pubkey));
        // A pubkey shorter than the prefix must not panic.
        assert_eq!(short_name("ab"), "anonab");
    }
}

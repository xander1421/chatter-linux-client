//! Two-way anonymity.
//!
//! TwoWayAnonymity service for mailbox message sending and polling.
//! Coordinates polling and sending through ephemeral mailboxes
//! after bootstrap has established the two-way connection.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use ed25519_dalek::{Signer, SigningKey};

use crate::domain::UserId;
use crate::error::MailboxError;
use crate::mailbox::dm::{DmConversationManager, DmMailbox};
use crate::network::grpc::AnonymousMailboxClient;
use crate::storage::Database;

/// Maximum messages to fetch per poll.
const DEFAULT_POLL_LIMIT: i32 = 50;

/// Two-way anonymity handler for DM mailbox operations.
///
/// Coordinates polling and sending through ephemeral mailboxes
/// after bootstrap has established the two-way connection.
pub struct TwoWayAnonymity {
    /// Conversation manager for state persistence.
    conversation_manager: DmConversationManager,
    schedule: parking_lot::Mutex<PageSchedule>,
}

#[derive(Default)]
struct PageSchedule {
    next_peer: Option<UserId>,
    active_mailboxes: HashMap<UserId, String>,
    cursors: HashMap<String, String>,
    deferred_acks: HashMap<String, Vec<String>>,
}

impl PageSchedule {
    /// A scan cursor is volatile and never means that the server copy is safe
    /// to delete. End-of-scan resets it so retained envelopes are retried.
    fn finish_page(&mut self, mailbox_id: &str, last_id: Option<String>, count: usize) -> bool {
        let next = last_id.filter(|id| {
            count == DEFAULT_POLL_LIMIT as usize && self.cursors.get(mailbox_id) != Some(id)
        });
        if let Some(cursor) = next {
            self.cursors.insert(mailbox_id.to_owned(), cursor);
            true
        } else {
            self.cursors.remove(mailbox_id);
            false
        }
    }

    fn ready_acks(&mut self, mailbox_id: &str, message_ids: Vec<String>) -> Vec<String> {
        let mut pending = self.deferred_acks.remove(mailbox_id).unwrap_or_default();
        pending.extend(message_ids);
        let (hold, ready): (Vec<_>, Vec<_>) = pending
            .into_iter()
            .partition(|id| self.cursors.get(mailbox_id) == Some(id));
        if !hold.is_empty() {
            self.deferred_acks.insert(mailbox_id.to_owned(), hold);
        }
        ready
    }
}

/// One mailbox's key identity; retained mailboxes must be ACKed with this key.
pub(crate) struct MailboxPage {
    pub peer_id: UserId,
    public_key: [u8; 32],
    private_key: [u8; 64],
    mailbox_id: String,
}

/// Result of polling a mailbox.
#[derive(Debug)]
pub struct PollResult {
    /// Conversation this poll was for.
    pub peer_id: UserId,
    /// Messages received (encrypted envelopes).
    pub messages: Vec<ReceivedMessage>,
    /// Whether there may be more messages.
    pub has_more: bool,
}

/// A message received from a mailbox.
#[derive(Debug)]
pub struct ReceivedMessage {
    /// Server-assigned message ID (for acknowledgment).
    pub message_id: String,
    /// Encrypted envelope content.
    pub encrypted_envelope: Vec<u8>,
    /// Envelope type (SealedSender, PreKey, Message).
    pub envelope_type: i32,
    /// Server timestamp.
    pub server_timestamp: u64,
}

impl TwoWayAnonymity {
    /// Create a new TwoWayAnonymity handler.
    ///
    /// # Arguments
    ///
    /// * `db` - Shared database connection
    pub fn new(db: Arc<Database>) -> Self {
        let conversation_manager = DmConversationManager::new(db);
        Self {
            conversation_manager,
            schedule: parking_lot::Mutex::new(PageSchedule::default()),
        }
    }

    /// Poll a conversation's mailbox for new messages.
    ///
    /// Authenticates with challenge-response and fetches pending messages.
    ///
    /// # Arguments
    ///
    /// * `peer_id` - The conversation peer
    /// * `mailbox_client` - For mailbox operations
    /// * `after_message_id` - Optional cursor for pagination
    pub async fn poll_mailbox(
        &self,
        peer_id: &UserId,
        mailbox_client: &mut AnonymousMailboxClient,
        after_message_id: Option<&str>,
    ) -> Result<PollResult, MailboxError> {
        // Load conversation state
        let conversation = self
            .conversation_manager
            .load_conversation(peer_id)?
            .ok_or(MailboxError::ConversationNotFound)?;

        let our_mailbox = conversation
            .our_mailbox
            .ok_or(MailboxError::NoMailboxConfigured)?;

        // Check mailbox not expired
        if our_mailbox.is_expired() {
            return Err(MailboxError::TokenExpired);
        }

        // Get challenge from server
        let challenge_response = mailbox_client
            .get_mailbox_challenge(our_mailbox.public_key.to_vec())
            .await
            .map_err(|e| MailboxError::Network(e.to_string()))?;

        // Sign challenge with our mailbox private key
        let challenge_signature =
            self.sign_challenge(&our_mailbox, &challenge_response.challenge)?;

        // Poll for messages
        let poll_response = mailbox_client
            .poll_ephemeral_mailbox(
                our_mailbox.public_key.to_vec(),
                challenge_signature,
                challenge_response.challenge,
                after_message_id,
                DEFAULT_POLL_LIMIT,
            )
            .await
            .map_err(|e| MailboxError::Network(e.to_string()))?;

        // Update poll timestamp
        self.conversation_manager
            .update_poll_time(&our_mailbox.id)?;

        // Convert to ReceivedMessage
        let messages: Vec<ReceivedMessage> = poll_response
            .messages
            .into_iter()
            .map(|m| ReceivedMessage {
                message_id: m.id,
                encrypted_envelope: m.encrypted_envelope,
                envelope_type: m.envelope_type,
                server_timestamp: m.timestamp,
            })
            .collect();

        let has_more = messages.len() == DEFAULT_POLL_LIMIT as usize;

        Ok(PollResult {
            peer_id: *peer_id,
            messages,
            has_more,
        })
    }

    /// Snapshot identities, preserving oldest-first rows within each peer.
    /// Peers rotate fairly; one peer's newer mailbox cannot jump its ratchet
    /// beyond an old backlog whose keys have not been consumed yet.
    pub(crate) fn mailbox_pages(&self) -> Result<VecDeque<VecDeque<MailboxPage>>, MailboxError> {
        let keys = self.conversation_manager.get_active_mailbox_keys()?;
        let active: HashSet<_> = keys.iter().map(|key| key.3.as_str()).collect();
        let mut schedule = self.schedule.lock();
        schedule
            .deferred_acks
            .retain(|id, _| active.contains(id.as_str()));
        schedule
            .cursors
            .retain(|id, _| active.contains(id.as_str()));
        schedule
            .active_mailboxes
            .retain(|_, id| active.contains(id.as_str()));
        let mut peers: VecDeque<VecDeque<MailboxPage>> = VecDeque::new();
        // Repository order is (conversation_id, created_at, id).
        for (peer_id, public_key, private_key, mailbox_id) in keys {
            if peers
                .back()
                .and_then(|group| group.front())
                .is_none_or(|m| m.peer_id != peer_id)
            {
                peers.push_back(VecDeque::new());
            }
            peers
                .back_mut()
                .expect("peer group exists")
                .push_back(MailboxPage {
                    peer_id,
                    public_key,
                    private_key,
                    mailbox_id,
                });
        }
        for group in &mut peers {
            if let Some(current) = schedule.active_mailboxes.get(&group[0].peer_id) {
                if let Some(position) = group.iter().position(|m| &m.mailbox_id == current) {
                    // Earlier rows completed this traversal; revisit them only
                    // after the current/newer rows finish, never ahead of them.
                    group.drain(..position);
                }
            }
        }
        if let Some(next) = schedule.next_peer {
            let offset = peers
                .iter()
                .position(|group| group[0].peer_id.0 >= next.0)
                .unwrap_or(0);
            peers.rotate_left(offset);
        }
        Ok(peers)
    }

    pub(crate) fn complete_mailbox(&self, mailbox: &MailboxPage, next: Option<&MailboxPage>) {
        let mut schedule = self.schedule.lock();
        if let Some(next) = next {
            schedule
                .active_mailboxes
                .insert(mailbox.peer_id, next.mailbox_id.clone());
        } else {
            schedule.active_mailboxes.remove(&mailbox.peer_id);
        }
    }

    pub(crate) async fn poll_page(
        &self,
        mailbox_client: &mut AnonymousMailboxClient,
        mailbox: &MailboxPage,
        next_mailbox: Option<&MailboxPage>,
    ) -> Result<PollResult, MailboxError> {
        // Advance scheduling even on network failure/timeout, so an unavailable
        // mailbox cannot monopolize the next bounded pass.
        {
            let mut schedule = self.schedule.lock();
            schedule.next_peer = next_mailbox.map(|m| m.peer_id);
            schedule
                .active_mailboxes
                .insert(mailbox.peer_id, mailbox.mailbox_id.clone());
        }
        let MailboxPage {
            peer_id,
            public_key,
            private_key,
            mailbox_id,
        } = mailbox;
        // Extract signing key once - 32-byte secret from 64-byte keypair
        let secret_bytes: [u8; 32] = private_key[..32]
            .try_into()
            .map_err(|_| MailboxError::BlindSignature("Invalid private key format".to_string()))?;
        let signing_key = SigningKey::from_bytes(&secret_bytes);

        let after_message_id = self.schedule.lock().cursors.get(mailbox_id).cloned();
        // Get a fresh challenge for each poll (challenges are single-use)
        let challenge_response = match mailbox_client
            .get_mailbox_challenge(public_key.to_vec())
            .await
        {
            Ok(resp) => resp,
            Err(e) => return Err(MailboxError::Network(e.to_string())),
        };

        let signature = signing_key.sign(&challenge_response.challenge);

        // Poll with cursor
        let poll_response = match mailbox_client
            .poll_ephemeral_mailbox(
                public_key.to_vec(),
                signature.to_bytes().to_vec(),
                challenge_response.challenge,
                after_message_id.as_deref(),
                DEFAULT_POLL_LIMIT,
            )
            .await
        {
            Ok(resp) => resp,
            Err(e) => return Err(MailboxError::Network(e.to_string())),
        };

        let has_more = self.schedule.lock().finish_page(
            mailbox_id,
            poll_response.messages.last().map(|m| m.id.clone()),
            poll_response.messages.len(),
        );
        self.conversation_manager.update_poll_time(mailbox_id)?;
        Ok(PollResult {
            peer_id: *peer_id,
            messages: poll_response
                .messages
                .into_iter()
                .map(|m| ReceivedMessage {
                    message_id: m.id,
                    encrypted_envelope: m.encrypted_envelope,
                    envelope_type: m.envelope_type,
                    server_timestamp: m.timestamp,
                })
                .collect(),
            has_more,
        })
    }

    pub(crate) async fn acknowledge_page(
        &self,
        mailbox: &MailboxPage,
        message_ids: Vec<String>,
        client: &mut AnonymousMailboxClient,
    ) -> Result<(), MailboxError> {
        // The server resolves a cursor through its still-existing row, falling
        // back to the head if ACK deleted it. Keep a successfully handled page
        // boundary until the following page has been fetched.
        let message_ids = self
            .schedule
            .lock()
            .ready_acks(&mailbox.mailbox_id, message_ids);
        if message_ids.is_empty() {
            return Ok(());
        }
        let challenge = client
            .get_mailbox_challenge(mailbox.public_key.to_vec())
            .await
            .map_err(|e| MailboxError::Network(e.to_string()))?;
        let secret: [u8; 32] = mailbox.private_key[..32]
            .try_into()
            .map_err(|_| MailboxError::BlindSignature("Invalid private key format".into()))?;
        let signature = SigningKey::from_bytes(&secret).sign(&challenge.challenge);
        client
            .acknowledge_messages(
                mailbox.public_key.to_vec(),
                signature.to_bytes().to_vec(),
                challenge.challenge,
                message_ids,
            )
            .await
            .map_err(|e| MailboxError::Network(e.to_string()))?;
        Ok(())
    }

    /// Sign a challenge with mailbox private key.
    fn sign_challenge(
        &self,
        mailbox: &DmMailbox,
        challenge: &[u8],
    ) -> Result<Vec<u8>, MailboxError> {
        // Use DmMailbox's built-in sign_challenge method
        mailbox.sign_challenge(challenge)
    }

    /// Acknowledge messages (delete from server).
    pub async fn acknowledge_messages(
        &self,
        peer_id: &UserId,
        message_ids: Vec<String>,
        mailbox_client: &mut AnonymousMailboxClient,
    ) -> Result<(), MailboxError> {
        let conversation = self
            .conversation_manager
            .load_conversation(peer_id)?
            .ok_or(MailboxError::ConversationNotFound)?;

        let our_mailbox = conversation
            .our_mailbox
            .ok_or(MailboxError::NoMailboxConfigured)?;

        // Get challenge
        let challenge_response = mailbox_client
            .get_mailbox_challenge(our_mailbox.public_key.to_vec())
            .await
            .map_err(|e| MailboxError::Network(e.to_string()))?;

        // Sign challenge
        let challenge_signature =
            self.sign_challenge(&our_mailbox, &challenge_response.challenge)?;

        // Acknowledge
        mailbox_client
            .acknowledge_messages(
                our_mailbox.public_key.to_vec(),
                challenge_signature,
                challenge_response.challenge,
                message_ids,
            )
            .await
            .map_err(|e| MailboxError::Network(e.to_string()))?;

        Ok(())
    }

    /// Check if conversation is ready for sending.
    pub fn can_send(&self, peer_id: &UserId) -> Result<bool, MailboxError> {
        let conversation = self.conversation_manager.load_conversation(peer_id)?;
        Ok(conversation.map(|c| c.is_ready_to_send()).unwrap_or(false))
    }

    /// Check if conversation is ready for receiving.
    pub fn can_receive(&self, peer_id: &UserId) -> Result<bool, MailboxError> {
        let conversation = self.conversation_manager.load_conversation(peer_id)?;
        Ok(conversation
            .map(|c| c.is_ready_to_receive())
            .unwrap_or(false))
    }

    /// Get conversations that need mailbox refresh.
    pub fn get_expiring_conversations(
        &self,
        within_hours: i64,
    ) -> Result<Vec<UserId>, MailboxError> {
        self.conversation_manager
            .get_expiring_mailboxes(within_hours)
    }

    /// Clean up expired mailbox state.
    pub fn cleanup_expired(&self) -> Result<u32, MailboxError> {
        self.conversation_manager.cleanup_expired()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::mailbox_v1::EnvelopeType;

    #[test]
    fn mailbox_schedule_resumes_after_budget_and_prunes_expired_state() {
        use chrono::{Duration, Utc};
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Database::open(&dir.path().join("schedule.db"), "fixture-only").unwrap());
        db.run_migrations().unwrap();
        {
            let conn = db.connection().unwrap();
            conn.execute("INSERT INTO mailbox_server_keys (key_id,public_key_der,valid_until,created_at) VALUES ('key',X'00',0,?1)", [Utc::now().to_rfc3339()]).unwrap();
            conn.execute("INSERT INTO mailbox_tokens (id,ephemeral_public_key,ephemeral_private_key,signature,server_key_id,created_at) VALUES ('token',X'01',X'02',X'03','key',?1)", [Utc::now().to_rfc3339()]).unwrap();
        }
        let two_way = TwoWayAnonymity::new(db.clone());
        for index in 0..40u8 {
            let peer = format!("00000000-0000-0000-0000-{index:012}")
                .parse::<UserId>()
                .unwrap();
            let mailbox = DmMailbox {
                id: format!("box-{index:02}"),
                public_key: [index; 32],
                private_key: [7; 64],
                encryption_private_key: [7; 32],
                signature: vec![],
                token_id: "token".into(),
                created_at: Utc::now(),
                expires_at: Utc::now() + Duration::days(1),
                last_polled_at: None,
            };
            two_way
                .conversation_manager
                .save_our_mailbox(&peer, &mailbox)
                .unwrap();
            if index == 0 {
                let mut newer = mailbox;
                newer.id = "aaa-newer".into(); // ID sort would incorrectly put it first.
                newer.created_at += Duration::seconds(1);
                newer.public_key = [99; 32];
                two_way
                    .conversation_manager
                    .save_our_mailbox(&peer, &newer)
                    .unwrap();
            }
        }
        let first = two_way.mailbox_pages().unwrap();
        assert_eq!(first.len(), 40);
        assert_eq!(
            first[0][0].mailbox_id, "box-00",
            "oldest row must precede newer ratchet data"
        );
        assert_eq!(first[0][1].mailbox_id, "aaa-newer");
        {
            let mut schedule = two_way.schedule.lock();
            schedule.next_peer = Some(first[32][0].peer_id);
            schedule
                .cursors
                .insert("box-00".into(), "retained-boundary".into());
            schedule
                .deferred_acks
                .insert("box-00".into(), vec!["retained-boundary".into()]);
        }
        let next = two_way.mailbox_pages().unwrap();
        assert_eq!(next[0][0].mailbox_id, "box-32");
        assert_eq!(next[8][0].mailbox_id, "box-00");
        assert_eq!(next[0][0].public_key, [32; 32]);
        // A bounded pass can end after the oldest row finishes but before the
        // newer row drains. Do not reopen the old row ahead of that continuation.
        two_way.complete_mailbox(&first[0][0], Some(&first[0][1]));
        let resumed = two_way.mailbox_pages().unwrap();
        assert_eq!(resumed[8].len(), 1);
        assert_eq!(resumed[8][0].mailbox_id, "aaa-newer");
        db.connection()
            .unwrap()
            .execute(
                "UPDATE dm_mailboxes SET expires_at=?1 WHERE id='box-00'",
                [(Utc::now() - Duration::days(1)).to_rfc3339()],
            )
            .unwrap();
        assert_eq!(two_way.mailbox_pages().unwrap().len(), 40);
        let schedule = two_way.schedule.lock();
        assert!(!schedule.cursors.contains_key("box-00"));
        assert!(!schedule.deferred_acks.contains_key("box-00"));
    }

    #[test]
    fn retained_full_pages_do_not_starve_later_mail_and_are_revisited() {
        let mut schedule = PageSchedule::default();
        // Model the server's cursor-row lookup and deletion semantics. Two
        // entire poison pages precede valid mail and an exact page boundary.
        let mut server: Vec<String> = (0..150).map(|i| format!("id-{i:03}")).collect();
        let mut handled = Vec::new();
        for _ in 0..4 {
            let after = schedule.cursors.get("box");
            let offset = after
                .and_then(|id| server.iter().position(|m| m == id))
                .map_or(0, |index| index + 1);
            let page: Vec<_> = server.iter().skip(offset).take(50).cloned().collect();
            let more = schedule.finish_page("box", page.last().cloned(), page.len());
            let successful: Vec<_> = page
                .into_iter()
                .filter(|id| id.as_str() >= "id-100")
                .collect();
            handled.extend(successful.clone());
            let ready = schedule.ready_acks("box", successful);
            server.retain(|id| !ready.contains(id));
            if !more {
                break;
            }
        }
        assert_eq!(
            handled.len(),
            50,
            "valid mail behind 100 poison envelopes must progress"
        );
        assert_eq!(server.len(), 100, "poison must remain unacknowledged");
        assert_eq!(server.first().map(String::as_str), Some("id-000"));
        assert!(
            !schedule.cursors.contains_key("box"),
            "next pass retries the retained head"
        );
        assert!(schedule.deferred_acks.is_empty());
    }

    #[test]
    fn a_non_advancing_page_stops_the_scan_without_acknowledging_it() {
        let mut schedule = PageSchedule::default();
        assert!(schedule.finish_page("box", Some("same".into()), 50));
        assert!(!schedule.finish_page("box", Some("same".into()), 50));
        assert!(schedule.ready_acks("box", Vec::new()).is_empty());
    }

    #[test]
    fn page_boundary_ack_waits_for_next_fetch_and_preserves_poison() {
        let mut schedule = PageSchedule::default();
        schedule.cursors.insert("mailbox".into(), "boundary".into());
        // The poison envelope is deliberately absent from the successful IDs.
        assert_eq!(
            schedule.ready_acks("mailbox", vec!["handled".into(), "boundary".into()]),
            vec!["handled"]
        );
        assert!(
            schedule.ready_acks("mailbox", vec![]).is_empty(),
            "failed fetch must retain cursor row"
        );
        schedule
            .cursors
            .insert("mailbox".into(), "next-boundary".into());
        assert_eq!(
            schedule.ready_acks("mailbox", vec!["next-boundary".into()]),
            vec!["boundary"]
        );
        schedule.cursors.remove("mailbox");
        assert_eq!(
            schedule.ready_acks("mailbox", vec![]),
            vec!["next-boundary"]
        );
        assert!(schedule.deferred_acks.is_empty());
    }

    #[test]
    fn page_boundary_ack_is_scoped_to_exact_mailbox_not_peer() {
        let mut schedule = PageSchedule::default();
        schedule.cursors.insert("old".into(), "old-boundary".into());
        schedule.cursors.insert("new".into(), "new-boundary".into());
        assert!(
            schedule
                .ready_acks("old", vec!["old-boundary".into()])
                .is_empty()
        );
        assert!(
            schedule
                .ready_acks("new", vec!["new-boundary".into()])
                .is_empty()
        );
        schedule.cursors.remove("old");
        assert_eq!(schedule.ready_acks("old", vec![]), vec!["old-boundary"]);
        assert!(schedule.ready_acks("new", vec![]).is_empty());
    }

    #[test]
    fn test_poll_result_structure() {
        let peer_id = UserId::new();
        let result = PollResult {
            peer_id,
            messages: vec![
                ReceivedMessage {
                    message_id: "msg-1".to_string(),
                    encrypted_envelope: vec![1, 2, 3],
                    envelope_type: EnvelopeType::SealedSender as i32,
                    server_timestamp: 1234567890,
                },
                ReceivedMessage {
                    message_id: "msg-2".to_string(),
                    encrypted_envelope: vec![4, 5, 6],
                    envelope_type: EnvelopeType::Message as i32,
                    server_timestamp: 1234567891,
                },
            ],
            has_more: false,
        };

        assert_eq!(result.peer_id, peer_id);
        assert_eq!(result.messages.len(), 2);
        assert!(!result.has_more);
    }

    #[test]
    fn test_received_message_structure() {
        let msg = ReceivedMessage {
            message_id: "test-msg".to_string(),
            encrypted_envelope: vec![0u8; 512],
            envelope_type: EnvelopeType::PrekeyMessage as i32,
            server_timestamp: 1234567890,
        };

        assert_eq!(msg.message_id, "test-msg");
        assert_eq!(msg.encrypted_envelope.len(), 512);
        assert_eq!(msg.envelope_type, EnvelopeType::PrekeyMessage as i32);
        assert_eq!(msg.server_timestamp, 1234567890);
    }

    #[test]
    fn test_default_poll_limit() {
        assert_eq!(DEFAULT_POLL_LIMIT, 50);
    }

    #[test]
    fn test_has_more_calculation() {
        // has_more should be true when we get exactly the limit
        let messages: Vec<ReceivedMessage> = (0..DEFAULT_POLL_LIMIT)
            .map(|i| ReceivedMessage {
                message_id: format!("msg-{}", i),
                encrypted_envelope: vec![],
                envelope_type: 0,
                server_timestamp: 0,
            })
            .collect();

        let has_more = messages.len() == DEFAULT_POLL_LIMIT as usize;
        assert!(has_more);

        // has_more should be false when we get less than the limit
        let fewer_messages: Vec<ReceivedMessage> = (0..10)
            .map(|i| ReceivedMessage {
                message_id: format!("msg-{}", i),
                encrypted_envelope: vec![],
                envelope_type: 0,
                server_timestamp: 0,
            })
            .collect();

        let has_more_fewer = fewer_messages.len() == DEFAULT_POLL_LIMIT as usize;
        assert!(!has_more_fewer);
    }

    #[test]
    fn test_envelope_types() {
        // Verify envelope type values
        assert_eq!(EnvelopeType::Unspecified as i32, 0);
        assert_eq!(EnvelopeType::SealedSender as i32, 1);
        assert_eq!(EnvelopeType::PrekeyMessage as i32, 2);
        assert_eq!(EnvelopeType::Message as i32, 3);
    }
}

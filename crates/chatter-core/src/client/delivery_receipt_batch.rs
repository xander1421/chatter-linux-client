//! Friend-DM delivery receipts: who gets one, and when it leaves.
//!
//! A sealed-sender DM hides its sender, but the automatic delivery receipt
//! it triggers goes back to that sender. A server that logs each sealed
//! envelope's recipient and arrival time can then link the pair: Martiny et
//! al., "Improving Signal's Sealed Sender" (NDSS 2021) measured a median of
//! 1.48 s from send to receipt and needed fewer than 5 messages using
//! 1-second epochs (§III-IV).
//!
//! Two controls here:
//!
//! - **Accepted senders only.** Signal-Desktop sends a delivery receipt only
//!   when `conversation.getAccepted()` (`ts/messages/handleDataMessage.preload.ts`).
//!   Chatter's equivalent is a DM that shows in the sidebar: a listed friend,
//!   or a peer we have written to (`ChatterClient::dm_peers`, which also drops
//!   blocked peers). A stranger's unanswered DM gets no receipt, so probing
//!   with a message does not even confirm the account is live.
//! - **Batching and random delay.** The first receipt owed to a peer opens a
//!   batch that leaves after a uniform random delay in
//!   [`DELIVERY_RECEIPT_DELAY_MIN`, `DELIVERY_RECEIPT_DELAY_MAX`]; every
//!   message from that peer arriving in the meantime rides in the same
//!   receipt (Signal-Desktop batches for 500 ms, `ts/util/deliveryReceipt.preload.ts`).
//!
//! Limit, stated by the paper itself (§VII-A, Fig. 5): random delays only
//! make the attacker widen the epoch; even hour-long delays still identified
//! the sender within about 60 messages. The delay raises the cost from
//! "under 5 messages" — it does not hide the conversation graph. Secret chats
//! (ephemeral mailboxes, the paper's §VI fix) are what does.
//!
//! The 5-30 s window keeps the second tick useful while forcing epochs ~30x
//! wider than the 1 s the paper's attack used.
use std::collections::HashMap;
use std::hash::Hash;
use std::time::Duration;

use parking_lot::Mutex;

use super::ChatterClient;
use crate::domain::UserId;

pub const DELIVERY_RECEIPT_DELAY_MIN: Duration = Duration::from_secs(5);
pub const DELIVERY_RECEIPT_DELAY_MAX: Duration = Duration::from_secs(30);

/// A uniformly random delay in `[MIN, MAX]` from the thread CSPRNG.
pub fn delivery_receipt_delay() -> Duration {
    use rand::Rng as _;
    rand::rng().random_range(DELIVERY_RECEIPT_DELAY_MIN..=DELIVERY_RECEIPT_DELAY_MAX)
}

/// Timestamps owed per key, collected until the key's batch fires.
pub(crate) struct ReceiptBatch<K> {
    pending: Mutex<HashMap<K, Vec<u64>>>,
}

impl<K: Eq + Hash> Default for ReceiptBatch<K> {
    fn default() -> Self {
        Self { pending: Mutex::new(HashMap::new()) }
    }
}

impl<K: Eq + Hash> ReceiptBatch<K> {
    /// Add `timestamp` under `key`. `true` when this opened the batch: the
    /// caller owns the timer and must [`Self::take`] the key when it fires.
    pub fn add(&self, key: K, timestamp: u64) -> bool {
        let mut pending = self.pending.lock();
        let opened = !pending.contains_key(&key);
        let stamps = pending.entry(key).or_default();
        if !stamps.contains(&timestamp) {
            stamps.push(timestamp);
        }
        opened
    }

    pub fn take(&self, key: &K) -> Vec<u64> {
        self.pending.lock().remove(key).unwrap_or_default()
    }
}

/// A batch belongs to one auth session: a receipt owed by the account that
/// was signed in is never sent by the next one.
pub(crate) type DmReceiptKey = (u64, UserId);

impl ChatterClient {
    /// Whether `peer` may learn that their DM reached us — see the module doc.
    pub(crate) fn dm_delivery_receipt_allowed(&self, peer: &UserId) -> bool {
        self.dm_peers().contains(peer)
    }

    /// Record the receipt owed for `peer`'s DM sent at `timestamp`. Returns
    /// the batch key when this call opened a batch (the caller waits
    /// [`delivery_receipt_delay`] and then sends `take(key)`); `None` when
    /// the peer is not accepted or a batch is already waiting.
    pub(crate) fn queue_dm_delivery_receipt(
        &self,
        peer: UserId,
        timestamp: u64,
    ) -> Option<DmReceiptKey> {
        if !self.dm_delivery_receipt_allowed(&peer) {
            tracing::debug!(%peer, "No delivery receipt: DM sender not accepted");
            return None;
        }
        let key = (self.auth_state().get().session_revision, peer);
        self.dm_delivery_receipts.add(key, timestamp).then_some(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delay_stays_inside_the_window_and_varies() {
        let delays: Vec<_> = (0..500).map(|_| delivery_receipt_delay()).collect();
        assert!(delays
            .iter()
            .all(|d| (DELIVERY_RECEIPT_DELAY_MIN..=DELIVERY_RECEIPT_DELAY_MAX).contains(d)));
        let (lo, hi) = (delays.iter().min().unwrap(), delays.iter().max().unwrap());
        // A constant delay is as predictable as none.
        assert!(*hi - *lo > Duration::from_secs(10), "delay is not random: {lo:?}..{hi:?}");
    }

    #[test]
    fn batch_opens_once_per_key_and_takes_everything_queued() {
        let batch = ReceiptBatch::default();
        assert!(batch.add("a", 1));
        assert!(!batch.add("a", 2));
        assert!(!batch.add("a", 2));
        assert!(batch.add("b", 9));
        assert_eq!(batch.take(&"a"), vec![1, 2]);
        assert!(batch.take(&"a").is_empty());
        // After the take the next message opens a fresh batch (new timer).
        assert!(batch.add("a", 3));
        assert_eq!(batch.take(&"b"), vec![9]);
    }

    /// A stranger's unanswered DM is a message request: no receipt is owed.
    /// A friend, or a peer we wrote to, is accepted; a blocked peer is not.
    #[tokio::test]
    async fn only_accepted_dm_senders_are_owed_a_receipt() {
        use crate::domain::ids::ChannelId;
        use crate::domain::message::{ConversationType, Message};
        use crate::domain::user::{Friendship, FriendshipStatus};
        use crate::storage::repositories::settings_repo::keys;

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
        let [me, friend, replied, blocked, stranger] = std::array::from_fn(|_| UserId::new());
        let users = client.messages();
        users.settings_repo.set_string(keys::CURRENT_USER_ID, &me.to_string()).unwrap();
        client.auth_state().set_logged_in(me, 1, "me".into());
        for u in [me, friend, replied, blocked, stranger] {
            users.user_repo.ensure_user_exists(&u).unwrap();
        }
        for (peer, status) in [(friend, FriendshipStatus::Accepted), (blocked, FriendshipStatus::Blocked)] {
            users.user_repo.insert_friendship(&Friendship {
                requester_id: me, addressee_id: peer, status, created_at: chrono::Utc::now(),
            }).unwrap();
        }
        for (peer, sender) in [(replied, me), (blocked, me), (stranger, stranger)] {
            let m = Message::new_text(ChannelId::from(peer), ConversationType::ServerDm, Some(sender), "hi".into(), 1);
            client.messages().message_repo.insert(&m).unwrap();
        }

        assert!(client.queue_dm_delivery_receipt(stranger, 10).is_none());
        assert!(client.queue_dm_delivery_receipt(blocked, 10).is_none());
        let revision = client.auth_state().get().session_revision;
        assert_eq!(client.queue_dm_delivery_receipt(friend, 10), Some((revision, friend)));
        // A second DM in the window joins the waiting batch.
        assert_eq!(client.queue_dm_delivery_receipt(friend, 11), None);
        assert_eq!(client.queue_dm_delivery_receipt(replied, 12), Some((revision, replied)));
        assert_eq!(client.dm_delivery_receipts.take(&(revision, friend)), vec![10, 11]);
        assert!(client.dm_delivery_receipts.take(&(revision, stranger)).is_empty());

        // Sealed or nothing: we hold no profile key (so no UAK) for `friend`,
        // and an identified receipt would name us. It is dropped before the
        // network — the server here is unreachable, so any send would fail.
        let receipt = crate::services::gateway::build_delivery_receipt_content_with_channel(vec![10, 11], None);
        assert!(client.messages().send_friend_dm_content(&friend, receipt).await.is_ok());
        let read = crate::services::gateway::build_read_receipt_content_with_channel(vec![10], None);
        assert!(
            client.messages().send_friend_dm_content(&friend, read).await.is_err(),
            "only delivery receipts are dropped; others still try the network",
        );
    }
}

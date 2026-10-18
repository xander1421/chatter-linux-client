use super::*;

const MAX_BUFFERED_KEY_SENDERS: usize = 32;

/// A channel group call we were rung for (or are joining unrung), held from
/// the ring until we have joined it, declined it, or it rang out. The ring
/// carries no key; this only collects the keys that reach us mid-join.
pub(crate) struct GroupCallInvite {
    /// Set once the user accepts, so the ring-out timer leaves it alone.
    pub(crate) accepted: bool,
    /// Each sender's newest key `(key, epoch)` that arrived while we were
    /// still joining. Per sender, so no member can displace another's key;
    /// after the join only the key authority's is installed
    /// (`ChatterClient::install_participant_group_key`).
    latest: HashMap<UserId, (Zeroizing<[u8; 32]>, u32)>,
}

/// A participant key that reached us while we were joining its call.
pub struct PendingGroupCallKey {
    pub sender: UserId,
    pub key: Zeroizing<[u8; 32]>,
    pub epoch: u32,
}

/// Shared ringing/accepted/key-buffer ownership. The worker holds the same
/// lock as cancellation while committing presented group acceptance.
#[derive(Default)]
pub(crate) struct GroupCallInvites(Mutex<HashMap<String, GroupCallInvite>>);

impl GroupCallInvites {
    pub(crate) fn lock(&self) -> parking_lot::MutexGuard<'_, HashMap<String, GroupCallInvite>> {
        self.0.lock()
    }
    /// Hold a group call we are being rung for (or joining unrung). False
    /// when this call is already held — a repeated ring.
    pub fn store_group_call_invite(&self, call_id: &str) -> bool {
        let mut invites = self.lock();
        if invites.contains_key(call_id) {
            return false;
        }
        invites.insert(
            call_id.to_string(),
            GroupCallInvite {
                accepted: false,
                latest: HashMap::new(),
            },
        );
        true
    }

    /// Keep a participant key for a call we were rung for and have not yet
    /// finished joining. True when such a call is held — the key is then
    /// kept if it is the newest from its sender, else dropped; false when it
    /// is not.
    pub fn buffer_group_call_key(
        &self,
        call_id: &str,
        sender: UserId,
        key: [u8; 32],
        epoch: u32,
    ) -> bool {
        let mut invites = self.lock();
        let Some(invite) = invites.get_mut(call_id) else {
            return false;
        };
        buffer_newest_per_sender(&mut invite.latest, sender, key, epoch);
        true
    }

    /// The user is joining: keep the invite (collecting keys) until
    /// [`Self::take_group_call_invite`], whatever the ring timer does. An
    /// unrung join (from the open channel) holds one from here.
    pub fn accept_group_call_invite(&self, call_id: &str) {
        self.lock()
            .entry(call_id.to_string())
            .or_insert_with(|| GroupCallInvite {
                accepted: false,
                latest: HashMap::new(),
            })
            .accepted = true;
    }

    /// Forget a group call invite (joined, declined or failed), returning
    /// the newest key from each sender that arrived while we were joining.
    pub fn take_group_call_invite(&self, call_id: &str) -> Vec<PendingGroupCallKey> {
        let Some(invite) = self.lock().remove(call_id) else {
            return Vec::new();
        };
        invite
            .latest
            .into_iter()
            .map(|(sender, (key, epoch))| PendingGroupCallKey { sender, key, epoch })
            .collect()
    }

    /// Whether the invite is still waiting for an answer (not collecting keys
    /// for an accepted join). Ring cancellation only owns this state.
    pub fn is_ringing_group_call_invite(&self, call_id: &str) -> bool {
        self.lock().get(call_id).is_some_and(|i| !i.accepted)
    }

    /// The ring ran out: forget the invite unless the user has answered.
    pub fn drop_ringing_group_call_invite(&self, call_id: &str) {
        self.lock()
            .retain(|id, invite| id != call_id || invite.accepted);
    }
}

/// Handles call signaling (key exchange, hangup) over the Signal Protocol.
///
/// Every signaling message goes through `MessageService::send_friend_dm_content`,
/// so its transport follows the mode rule for friend DMs: Shield → the mailbox,
/// normal → sealed-sender streaming (a call needs signaling that arrives now,
/// not at the next mailbox poll).
/// Keep `(key, epoch)` as `sender`'s buffered key if it is newer than the
/// one held from them; a new sender only while under the cap.
pub(super) fn buffer_newest_per_sender(
    held: &mut HashMap<UserId, (Zeroizing<[u8; 32]>, u32)>,
    sender: UserId,
    key: [u8; 32],
    epoch: u32,
) {
    match held.get(&sender) {
        Some((_, newest)) if epoch <= *newest => {}
        None if held.len() >= MAX_BUFFERED_KEY_SENDERS => {}
        _ => {
            held.insert(sender, (Zeroizing::new(key), epoch));
        }
    }
}

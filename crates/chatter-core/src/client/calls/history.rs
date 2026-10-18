use super::*;

/// One load of the Calls screen: the rows to render plus the unseen
/// missed-call count for the tab badge. Both come from the same blocking read so the
/// list and the badge can never disagree.
#[derive(Clone, Debug, Default)]
pub struct CallHistoryPage {
    pub rows: Vec<crate::ui::CallHistoryRowData>,
    pub missed_count: u32,
    /// Taken just before the read. Clearing "this list" passes it back so a
    /// call that arrived after the user saw the list is never deleted unseen.
    pub snapshot_at: chrono::DateTime<chrono::Utc>,
    /// Where the next page starts, when this one filled its `limit`: the
    /// `(started_at, id)` of the last stored entry read, which the
    /// repository treats as strictly-older-than. `None` = the list ends here.
    pub next_cursor: Option<CallHistoryCursor>,
}

/// Opaque to the UI: it hands the cursor back unchanged to page further.
pub type CallHistoryCursor = (chrono::DateTime<chrono::Utc>, String);

impl ChatterClient {
    // ── Call history ──────────────────────────────────────────────────

    // ── Local call history (v053) ──────────────────────────────────
    //
    // These methods talk to the `call_history` SQLite table via
    // `CallHistoryRepository`. Purely client-local state: the server exposes
    // no call-history RPC yet. Rows are opened by `start_call` (outgoing)
    // and `call_history_recorder` (incoming), and read by the Calls screen.

    /// Build the Calls screen's rows, newest first, plus the missed-call
    /// count the tab badge shows.
    ///
    /// Async because `CallHistoryRepository` is a synchronous SQLite handle:
    /// the read (and all the row shaping) runs on a blocking worker, never on
    /// the caller's thread. The Calls screen is painted from the Slint event
    /// loop, and a SQLite read there stalls every frame in the app.
    ///
    /// Returns display-ready data — the UI renders it without re-deriving
    /// wording, durations or routing.
    ///
    /// Only the Calls screen loads a page, so loading one is the user seeing
    /// their missed calls: it marks them seen and the badge clears.
    ///
    /// `cursor` is the previous page's [`CallHistoryPage::next_cursor`];
    /// `None` reads from the newest call.
    pub async fn call_history_page(
        &self,
        filter: crate::domain::call::CallHistoryFilter,
        limit: u32,
        cursor: Option<CallHistoryCursor>,
    ) -> Result<CallHistoryPage, ClientError> {
        let auth = self.auth_state().clone();
        let revision = auth.get().session_revision;
        let repo = self.call_history_repo();
        let mut page = tokio::task::spawn_blocking(move || {
            let snapshot_at = chrono::Utc::now();
            let entries = repo
                .list(filter, limit, cursor)
                .map_err(|e| ClientError::Inbox(format!("call history list: {}", e)))?;
            // From the stored rows, before the live-row filter below: a page
            // short by a hidden in-progress call still has older calls.
            let next_cursor = (entries.len() as u32 >= limit)
                .then(|| entries.last().map(|e| (e.started_at, e.id.clone())))
                .flatten();
            if auth.get().session_revision != revision {
                return Err(ClientError::Validation(
                    "call history session changed".into(),
                ));
            }
            let previous_missed = repo
                .missed_count()
                .map_err(|e| ClientError::Inbox(format!("call history count: {e}")))?
                as u32;
            let missed_count = match repo.mark_missed_seen() {
                Ok(()) => repo
                    .missed_count()
                    .map_err(|e| ClientError::Inbox(format!("call history count: {e}")))?
                    as u32,
                Err(e) => {
                    tracing::warn!(error = %e, "call history: mark missed seen failed");
                    previous_missed
                }
            };
            Ok::<_, ClientError>(CallHistoryPage {
                rows: entries
                    .iter()
                    .filter(|entry| {
                        // An open row created in this process may still be live;
                        // its provisional result is not a missed-call outcome.
                        filter != crate::domain::call::CallHistoryFilter::Missed
                            || entry.ended_at.is_some()
                            || entry.started_at < history_live_since()
                    })
                    .map(|entry| {
                        let mut row = crate::ui::CallHistoryRowData::from(entry);
                        if entry.ended_at.is_none() && entry.started_at >= history_live_since() {
                            row.in_progress = true;
                            row.is_missed = false;
                            row.can_call_back = false;
                        }
                        row
                    })
                    .collect(),
                missed_count,
                snapshot_at,
                next_cursor,
            })
        })
        .await
        .map_err(|e| ClientError::Inbox(format!("call history join: {}", e)))??;
        for row in &mut page.rows {
            row.avatar_bytes = self.call_peer_avatar(&row.conversation_type, &row.conversation_id);
            if let Some(name) = self.call_peer_name(&row.conversation_type, &row.conversation_id) {
                row.name = name;
            }
        }
        Ok(page)
    }

    /// A 1:1 call row's peer, named as the chat list names them. The name
    /// stored with the entry is whatever was known when the call happened —
    /// empty for most outgoing calls, which then read "Unknown" forever.
    /// Group and channel calls keep the stored conversation name → `None`.
    fn call_peer_name(&self, conversation_type: &str, conversation_id: &str) -> Option<String> {
        use crate::domain::message::ConversationType as CT;
        match CT::from_str_value(conversation_type).ok()? {
            CT::ServerDm => Some(self.display_name(&conversation_id.parse().ok()?)),
            _ => None,
        }
    }

    /// Avatar for a call-history row: the peer's profile for a server DM or
    /// the channel icon for a channel call. Anonymous groups have no stored
    /// group icon and must not borrow an unrelated channel's icon.
    fn call_peer_avatar(&self, conversation_type: &str, conversation_id: &str) -> Option<Vec<u8>> {
        use crate::domain::message::ConversationType as CT;
        match CT::from_str_value(conversation_type).ok()? {
            CT::ServerDm => self.user_avatar(&conversation_id.parse().ok()?),
            CT::Channel => self
                .get_channel_local(&conversation_id.parse().ok()?)
                .filter(|channel| channel.conversation_type() == CT::Channel)
                .and_then(|channel| channel.icon),
            _ => None,
        }
    }

    /// Unseen missed incoming calls on their own, for painting the tab badge
    /// at startup without loading the list (which would mark them seen).
    pub async fn missed_call_count(&self) -> u32 {
        let repo = self.call_history_repo();
        tokio::task::spawn_blocking(move || repo.missed_count().unwrap_or(0) as u32)
            .await
            .unwrap_or(0)
    }

    /// Fallible badge read; UI callers retain their last successful badge on error.
    pub async fn try_missed_call_count(&self) -> Result<u32, ClientError> {
        let repo = self.call_history_repo();
        tokio::task::spawn_blocking(move || {
            repo.missed_count()
                .map(|count| count as u32)
                .map_err(|e| ClientError::Inbox(format!("call history count: {e}")))
        })
        .await
        .map_err(|e| ClientError::Inbox(format!("call history count task: {e}")))?
    }

    /// Resolve a Calls-screen row back to the conversation it happened in,
    /// so the UI can reopen it and redial. `None` when the row's key no
    /// longer names a reachable conversation.
    pub fn call_history_conversation(
        &self,
        conversation_type: &str,
        conversation_id: &str,
    ) -> Option<crate::state::ActiveConversation> {
        let ct =
            crate::domain::message::ConversationType::from_str_value(conversation_type).ok()?;
        crate::state::ActiveConversation::from_routing_key(&ct, conversation_id)
    }

    /// Delete every call `filter` lists, on this device only, that started
    /// no later than `snapshot_at` (the [`CallHistoryPage`] the user is
    /// looking at). Calls history has no server copy and no peer-side delete;
    /// chat Call bubbles are separate messages and are not touched. A call
    /// still in progress is kept. Returns the number deleted.
    pub async fn clear_call_history(
        &self,
        filter: crate::domain::call::CallHistoryFilter,
        snapshot_at: chrono::DateTime<chrono::Utc>,
    ) -> Result<u64, ClientError> {
        let repo = self.call_history_repo();
        tokio::task::spawn_blocking(move || {
            repo.delete_matching(filter, snapshot_at, history_live_since())
                .map_err(|e| ClientError::Inbox(format!("call history clear: {}", e)))
        })
        .await
        .map_err(|e| ClientError::Inbox(format!("call history join: {}", e)))?
    }

    /// Delete exactly the selected Calls-screen rows, on this device only;
    /// same scope and in-progress rule as
    /// [`clear_call_history`](Self::clear_call_history). Returns the number
    /// deleted, which is short of `ids.len()` when a row was kept.
    pub async fn delete_call_history_entries(&self, ids: Vec<String>) -> Result<u64, ClientError> {
        let repo = self.call_history_repo();
        tokio::task::spawn_blocking(move || {
            repo.delete_ids(&ids, history_live_since())
                .map_err(|e| ClientError::Inbox(format!("call history delete: {}", e)))
        })
        .await
        .map_err(|e| ClientError::Inbox(format!("call history join: {}", e)))?
    }

    /// Accessor used by the history helpers above and by the signaling
    /// service when it wants to record events from inside core.
    pub(super) fn call_history_repo(&self) -> crate::storage::repositories::CallHistoryRepository {
        crate::storage::repositories::CallHistoryRepository::new(self.services.database.clone())
    }
}

/// Open rows started at or after this may be live calls; see
/// `call_history_recorder::LIVE_SINCE`. Without media no call can be live.
fn history_live_since() -> chrono::DateTime<chrono::Utc> {
    #[cfg(feature = "media")]
    {
        crate::services::call_history_recorder::LIVE_SINCE
            .get()
            .copied()
            .unwrap_or(chrono::DateTime::<chrono::Utc>::MIN_UTC)
    }
    #[cfg(not(feature = "media"))]
    {
        chrono::DateTime::<chrono::Utc>::MAX_UTC
    }
}

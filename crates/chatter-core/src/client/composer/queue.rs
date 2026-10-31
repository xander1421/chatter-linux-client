//! Bounded FIFO admission before attachment upload. Waiting intents live in
//! SQLCipher; the normal send journal takes ownership atomically at claim.
//! Restart restores only never-submitted intents as drafts, never auto-resends.
use super::*;
use std::sync::{Arc, Weak};
use tokio::sync::watch;

const MAX_COMPOSED_SENDS: usize = 32;

#[derive(Debug)]
pub(crate) struct QueueTicket {
    conversation: ActiveConversation,
    revision: u64,
    predecessors: Vec<watch::Receiver<bool>>,
    cancelled: std::sync::atomic::AtomicBool,
    done: watch::Sender<bool>,
}
impl QueueTicket {
    pub(super) fn is_waiting(&self) -> bool {
        self.predecessors
            .iter()
            .any(|rx| !*rx.borrow() && rx.has_changed().is_ok())
    }
    async fn wait(&self, client: &ChatterClient) -> bool {
        for mut predecessor in self.predecessors.clone() {
            while !*predecessor.borrow() {
                tokio::select! {
                    changed = predecessor.changed() => { if changed.is_err() { break; } },
                    _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {
                        if !client.composer_session_is_current(self.revision) || self.cancelled.load(Ordering::SeqCst) { return false; }
                    }
                }
            }
        }
        client.composer_session_is_current(self.revision) && !self.cancelled.load(Ordering::SeqCst)
    }
}
impl Drop for QueueTicket {
    fn drop(&mut self) {
        self.done.send_replace(true);
    }
}

pub(in crate::client) fn queued_key(owner: &str, attempt: &str) -> String {
    format!("ui.composer_queue.{owner}:{attempt}")
}

impl ChatterClient {
    pub(super) fn reserve_composer_turn(
        &self,
        conversation: ActiveConversation,
        revision: u64,
    ) -> Result<Arc<QueueTicket>, String> {
        // Load durable recovery before taking the admission lock.
        let _ = self.composer_drafts_for_session();
        if !self.composer_session_is_current(revision) {
            return Err("The account session changed. Nothing was sent.".into());
        }
        let mut queue = self.composer_queue.lock();
        queue.retain(|entry| entry.strong_count() > 0);
        if queue.len() >= MAX_COMPOSED_SENDS {
            return Err(
                "The send queue is full. Your draft is kept; wait for an earlier send.".into(),
            );
        }
        let predecessor = queue
            .iter()
            .rev()
            .filter_map(Weak::upgrade)
            .find(|entry| entry.conversation == conversation && entry.revision == revision);
        // A live predecessor owns an actual task. Unknown durable claims from
        // prior processes still fail closed through the existing journal.
        if predecessor.is_none() {
            self.ensure_send_available(conversation)
                .map_err(|e| e.to_string())?;
        }
        let mut predecessors = Vec::new();
        if let Some(entry) = predecessor {
            predecessors.extend(
                entry
                    .predecessors
                    .iter()
                    .filter(|rx| !*rx.borrow() && rx.has_changed().is_ok())
                    .cloned(),
            );
            predecessors.push(entry.done.subscribe());
        }
        let ticket = Arc::new(QueueTicket {
            conversation,
            revision,
            predecessors,
            cancelled: std::sync::atomic::AtomicBool::new(false),
            done: watch::channel(false).0,
        });
        queue.push(Arc::downgrade(&ticket));
        Ok(ticket)
    }

    pub(super) fn persist_queued_composer(&self, send: &ComposerSend) -> Result<(), String> {
        let auth = self.auth_state().get();
        // Offline/pre-auth fixtures may freeze a draft, but cannot submit it.
        let Some(owner) = auth.current_user_id else {
            return Ok(());
        };
        if auth.session_revision != send.session_revision {
            return Err("The account session changed. Nothing was sent.".into());
        }
        let parked = ParkedSend::of(send).ok_or("This can't be sent here.")?;
        let save = (|| -> Result<(), crate::error::StorageError> {
            let raw = serde_json::to_string(&parked).map_err(|_| {
                crate::error::StorageError::InvalidData("Cannot save queued send".into())
            })?;
            let conn = self.services.database.connection()?;
            let current = self.auth_state().get();
            if current.session_revision != send.session_revision
                || current.current_user_id != Some(owner)
            {
                return Err(crate::error::StorageError::InvalidData(
                    "Composer session retired".into(),
                ));
            }
            let retained: i64 = conn.query_row(
                "SELECT count(*) FROM settings WHERE key GLOB ?1",
                rusqlite::params![format!("ui.composer_queue.{owner}:*")],
                |r| r.get(0),
            )?;
            if retained >= MAX_COMPOSED_SENDS as i64 {
                return Err(crate::error::StorageError::InvalidData(
                    "Send queue is full".into(),
                ));
            }
            conn.execute("INSERT INTO settings(key,value,updated_at) VALUES(?1,?2,?3) ON CONFLICT(key) DO UPDATE SET value=excluded.value,updated_at=excluded.updated_at",
                rusqlite::params![queued_key(&owner.to_string(), &send.attempt_id.to_string()), raw, chrono::Utc::now().to_rfc3339()])?;
            Ok(())
        })();
        save.map_err(|_| "Could not save the queued send. Your draft is kept.".to_string())
    }

    pub async fn await_composer_turn(&self, send: &ComposerSend) -> bool {
        match &send.queue_ticket {
            Some(ticket) => ticket.wait(self).await,
            None => self.composer_session_is_current(send.session_revision),
        }
    }

    pub(super) fn delete_queued_composers(&self, owner: UserId) {
        if let Ok(conn) = self.services.database.connection() {
            let _ = conn.execute(
                "DELETE FROM settings WHERE key GLOB ?1",
                rusqlite::params![format!("ui.composer_queue.{owner}:*")],
            );
        }
    }

    /// Called under the once-per-session draft loader. No auto-submission: a
    /// queued intent that never reached `claim` returns through normal recovery.
    pub(super) fn restore_queued_composers(&self, owner: UserId, revision: u64) {
        if self.composer_recovery_unread.load(Ordering::SeqCst) {
            return;
        }
        let restored = (|| -> Result<(), crate::error::StorageError> {
            let mut parked = self.parked_sends.lock();
            let mut conn = self.services.database.connection()?;
            let tx = conn.transaction()?;
            let entries: Vec<(String, String)> = {
                let mut statement = tx.prepare(
                    "SELECT key,value FROM settings WHERE key GLOB ?1 ORDER BY updated_at,key",
                )?;
                statement
                    .query_map(
                        rusqlite::params![format!("ui.composer_queue.{owner}:*")],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )?
                    .collect::<Result<_, _>>()?
            };
            if entries.is_empty() {
                return Ok(());
            }
            let mut next = parked.clone();
            for (_, raw) in &entries {
                let send: ParkedSend = serde_json::from_str(raw).map_err(|_| {
                    crate::error::StorageError::InvalidData("Unreadable queued send".into())
                })?;
                next.push(ComposerSend {
                    attempt_id: uuid::Uuid::new_v4(),
                    session_revision: revision,
                    conversation: send.conversation.into_active(),
                    text: send.text,
                    reply: send.reply,
                    attachments: send
                        .attachments
                        .into_iter()
                        .map(StagedAttachment::into_pending)
                        .collect(),
                    queue_ticket: None,
                });
            }
            let snapshot = self.composer_recovery_snapshot(&next);
            let raw = serde_json::to_string(&snapshot).map_err(|_| {
                crate::error::StorageError::InvalidData("Cannot save queued recovery".into())
            })?;
            tx.execute("INSERT INTO settings(key,value,updated_at) VALUES(?1,?2,?3) ON CONFLICT(key) DO UPDATE SET value=excluded.value,updated_at=excluded.updated_at",
                rusqlite::params![Self::composer_recovery_key(owner), raw, chrono::Utc::now().to_rfc3339()])?;
            for (key, _) in entries {
                tx.execute("DELETE FROM settings WHERE key=?1", rusqlite::params![key])?;
            }
            tx.commit()?;
            *parked = next;
            Ok(())
        })();
        if restored.is_err() {
            self.composer_recovery_unread.store(true, Ordering::SeqCst);
            tracing::warn!("queued composer recovery unread; persisted intents retained");
        }
    }
}

impl ChatterClient {
    /// A refusal before the normal journal claimed a queued intent leaves no
    /// wire uncertainty. Transfer it to parked recovery atomically; cancellation
    /// instead leaves the original queue row for restart recovery.
    pub(super) fn park_unstarted_queued_send(&self, send: &ComposerSend) -> Result<bool, ()> {
        if send.queue_ticket.is_none() {
            return Ok(false);
        }
        let owner = self.auth_state().get().current_user_id.ok_or(())?;
        let key = queued_key(&owner.to_string(), &send.attempt_id.to_string());
        if self.composer_recovery_unread.load(Ordering::SeqCst) {
            return Err(());
        }
        let result = (|| -> Result<bool, crate::error::StorageError> {
            let mut parked = self.parked_sends.lock();
            let mut conn = self.services.database.connection()?;
            let tx = conn.transaction()?;
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM settings WHERE key=?1)",
                rusqlite::params![key],
                |r| r.get(0),
            )?;
            if !exists {
                return Ok(false);
            }
            let mut next = parked.clone();
            let mut recovered = send.clone();
            recovered.queue_ticket = None;
            next.push(recovered);
            let snapshot = self.composer_recovery_snapshot(&next);
            let raw = serde_json::to_string(&snapshot).map_err(|_| {
                crate::error::StorageError::InvalidData("Cannot park queued send".into())
            })?;
            tx.execute("INSERT INTO settings(key,value,updated_at) VALUES(?1,?2,?3) ON CONFLICT(key) DO UPDATE SET value=excluded.value,updated_at=excluded.updated_at",
                rusqlite::params![Self::composer_recovery_key(owner), raw, chrono::Utc::now().to_rfc3339()])?;
            tx.execute("DELETE FROM settings WHERE key=?1", rusqlite::params![key])?;
            tx.commit()?;
            *parked = next;
            Ok(true)
        })();
        result.map_err(|_| ())
    }

    pub(super) fn cancel_queued_conversation(&self, conversation: ActiveConversation) {
        for ticket in self.composer_queue.lock().iter().filter_map(Weak::upgrade) {
            if ticket.conversation == conversation {
                ticket.cancelled.store(true, Ordering::SeqCst);
            }
        }
        let Some(owner) = self.auth_state().get().current_user_id else {
            return;
        };
        let Some(conversation) = ConversationKey::of(conversation) else {
            return;
        };
        let removed = (|| -> Result<Vec<PendingAttachment>, crate::error::StorageError> {
            let mut conn = self.services.database.connection()?;
            let tx = conn.transaction()?;
            let entries: Vec<(String, String)> = {
                let mut statement =
                    tx.prepare("SELECT key,value FROM settings WHERE key GLOB ?1")?;
                statement
                    .query_map(
                        rusqlite::params![format!("ui.composer_queue.{owner}:*")],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )?
                    .collect::<Result<_, _>>()?
            };
            let mut files = Vec::new();
            for (key, raw) in entries {
                let Ok(send) = serde_json::from_str::<ParkedSend>(&raw) else {
                    continue;
                };
                if send.conversation == conversation {
                    tx.execute("DELETE FROM settings WHERE key=?1", rusqlite::params![key])?;
                    files.extend(
                        send.attachments
                            .into_iter()
                            .map(StagedAttachment::into_pending),
                    );
                }
            }
            tx.commit()?;
            Ok(files)
        })();
        if let Ok(files) = removed {
            self.release_staged_files(&files);
        }
    }
}

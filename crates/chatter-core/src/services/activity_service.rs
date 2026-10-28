//! Account-scoped Activity Center feed in the encrypted client database.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Local, Utc};
use rusqlite::{OptionalExtension, params};

use crate::domain::UserId;
use crate::error::StorageError;
use crate::storage::Database;

#[derive(Clone, Debug)]
pub struct ActivityItem {
    pub id: String,
    pub kind: String,
    pub title: String,
    pub preview: String,
    pub time_label: String,
    pub section: String,
    pub unread: bool,
}

#[derive(Clone, Debug)]
pub struct ActivityTarget {
    pub kind: String,
    pub conversation_id: Option<String>,
    pub conversation_type: Option<String>,
    pub message_id: Option<String>,
}

pub struct NewActivity<'a> {
    pub id: &'a str,
    pub kind: &'a str,
    pub actor_id: Option<UserId>,
    pub actor_name: &'a str,
    pub detail: &'a str,
    pub preview: &'a str,
    pub conversation_id: Option<&'a str>,
    pub conversation_type: Option<&'a str>,
    pub message_id: Option<&'a str>,
    pub created_at_ms: i64,
}

/// Id prefix of the single Activity row kept per thread of mine.
pub const THREAD_ACTIVITY_PREFIX: &str = "thread:";

pub struct ActivityService {
    db: Arc<Database>,
    changed: tokio::sync::broadcast::Sender<()>,
}

impl ActivityService {
    pub fn new(db: Arc<Database>) -> Self {
        Self { db, changed: tokio::sync::broadcast::channel(32).0 }
    }

    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<()> {
        self.changed.subscribe()
    }

    pub fn record(&self, owner: UserId, mut item: NewActivity<'_>) -> Result<(), StorageError> {
        // Times come from the sender (a message's own timestamp); a skewed or
        // forged one in the future would hold the row at the top as "now".
        item.created_at_ms = item.created_at_ms.min(Utc::now().timestamp_millis());
        let conn = self.db.connection()?;
        // One row per thread: a newer reply refreshes it and marks it unread
        // again. Every other row is recorded once (a re-delivered message or a
        // re-synced invite changes nothing).
        let inserted = conn.execute(
            "INSERT INTO activity_items
             (owner_id,id,kind,actor_id,actor_name,detail,preview,conversation_id,conversation_type,message_id,created_at_ms)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)
             ON CONFLICT(owner_id,id) DO UPDATE SET kind=excluded.kind, actor_id=excluded.actor_id, actor_name=excluded.actor_name,
                 preview=excluded.preview, message_id=excluded.message_id, created_at_ms=excluded.created_at_ms, unread=1
             WHERE excluded.id LIKE ?12 AND excluded.created_at_ms > activity_items.created_at_ms",
            params![owner.to_string(), item.id, item.kind, item.actor_id.map(|id| id.to_string()),
                item.actor_name, item.detail, item.preview, item.conversation_id,
                item.conversation_type, item.message_id, item.created_at_ms,
                format!("{THREAD_ACTIVITY_PREFIX}%")],
        )?;
        if inserted != 0 {
            Self::prune(&conn, owner)?;
            let _ = self.changed.send(());
        }
        Ok(())
    }

    fn prune(conn: &rusqlite::Connection, owner: UserId) -> Result<(), StorageError> {
        let owner = owner.to_string();
        let cutoff = Utc::now().timestamp_millis() - 30 * 24 * 60 * 60 * 1000;
        conn.execute("DELETE FROM activity_items WHERE owner_id=?1 AND created_at_ms<?2", params![owner, cutoff])?;
        // A row stamped in the future (before `record` capped times) would stay
        // first and read "now" for good; bring it back to the present.
        conn.execute("UPDATE activity_items SET created_at_ms=?2 WHERE owner_id=?1 AND created_at_ms>?2",
            params![owner, Utc::now().timestamp_millis()])?;
        conn.execute(
            "DELETE FROM activity_items WHERE owner_id=?1 AND id NOT IN
             (SELECT id FROM activity_items WHERE owner_id=?1 ORDER BY created_at_ms DESC,id DESC LIMIT 500)",
            [&owner],
        )?;
        Ok(())
    }

    pub fn list(&self, owner: UserId, show_preview: bool, resolve_names: impl Fn(&[UserId]) -> HashMap<UserId, String>) -> Result<Vec<ActivityItem>, StorageError> {
        Self::prune(&*self.db.connection()?, owner)?;
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT id,kind,actor_id,actor_name,detail,preview,created_at_ms,unread,conversation_type FROM activity_items
             WHERE owner_id=?1 ORDER BY created_at_ms DESC,id DESC LIMIT 500",
        )?;
        let now = Local::now();
        let rows = stmt.query_map([owner.to_string()], |row| {
            let created_ms: i64 = row.get(6)?;
            let created = DateTime::<Utc>::from_timestamp_millis(created_ms)
                .unwrap_or_else(Utc::now).with_timezone(&Local);
            let today = created.date_naive() == now.date_naive();
            let minutes = (now.timestamp_millis() - created_ms).max(0) / 60_000;
            let time_label = if minutes < 1 { "now".to_owned() }
                else if minutes < 60 { format!("{minutes}m") }
                else if today { created.format("%H:%M").to_string() }
                else if minutes < 2 * 24 * 60 { "Yesterday".to_owned() }
                else { created.format("%d %b").to_string() };
            Ok((ActivityItem {
                id: row.get(0)?, kind: row.get(1)?, title: String::new(),
                preview: if show_preview && !matches!(row.get::<_, Option<String>>(8)?.as_deref(), Some("anon_dm" | "anon_group")) {
                    row.get(5)?
                } else { String::new() },
                time_label, section: if today { "Today" } else { "Earlier" }.to_owned(),
                unread: row.get::<_, i64>(7)? != 0,
            }, row.get::<_, Option<String>>(2)?, row.get::<_, String>(3)?, row.get::<_, String>(4)?))
        })?;
        let rows = rows.collect::<Result<Vec<_>, _>>()?;
        drop(stmt);
        drop(conn);
        let actor_ids: Vec<UserId> = rows.iter().filter_map(|(_, actor_id, _, _): &(ActivityItem, Option<String>, String, String)|
            actor_id.as_deref().and_then(|id| id.parse().ok())).collect();
        let names = resolve_names(&actor_ids);
        Ok(rows.into_iter().map(|(mut item, actor_id, actor_name, detail)| {
            item.title = activity_title(&item.kind, actor_id.as_deref(), &actor_name, &detail, &names);
            item
        }).collect())
    }

    pub fn unread_count(&self, owner: UserId) -> Result<u32, StorageError> {
        Self::prune(&*self.db.connection()?, owner)?;
        let conn = self.db.read_connection()?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM activity_items WHERE owner_id=?1 AND unread=1",
            [owner.to_string()], |row| row.get(0),
        )?;
        Ok(count.min(u32::MAX as i64) as u32)
    }

    pub fn target(&self, owner: UserId, id: &str) -> Result<Option<ActivityTarget>, StorageError> {
        let conn = self.db.read_connection()?;
        conn.query_row(
            "SELECT kind,conversation_id,conversation_type,message_id FROM activity_items
             WHERE owner_id=?1 AND id=?2",
            params![owner.to_string(), id],
            |row| Ok(ActivityTarget { kind: row.get(0)?, conversation_id: row.get(1)?, conversation_type: row.get(2)?, message_id: row.get(3)? }),
        ).optional().map_err(Into::into)
    }

    /// Drop one row: what it reported no longer happened (a withdrawn reaction).
    pub fn forget(&self, owner: UserId, id: &str) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        if conn.execute("DELETE FROM activity_items WHERE owner_id=?1 AND id=?2", params![owner.to_string(), id])? != 0 {
            let _ = self.changed.send(());
        }
        Ok(())
    }

    pub fn mark_read(&self, owner: UserId, id: &str) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        if conn.execute("UPDATE activity_items SET unread=0 WHERE owner_id=?1 AND id=?2 AND unread=1", params![owner.to_string(), id])? != 0 {
            let _ = self.changed.send(());
        }
        Ok(())
    }

    /// Mark read the message rows (mention, reply, reaction) of one
    /// conversation, as reading it does in Slack and Discord. Thread rows are
    /// left for [`Self::mark_thread_read`]: their replies are read in the
    /// thread, not the conversation.
    pub fn mark_conversation_read(&self, owner: UserId, conversation_id: &str) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        if conn.execute(
            "UPDATE activity_items SET unread=0 WHERE owner_id=?1 AND conversation_id=?2 AND unread=1
               AND kind IN ('mention','reply','reaction') AND id NOT LIKE ?3",
            params![owner.to_string(), conversation_id, format!("{THREAD_ACTIVITY_PREFIX}%")],
        )? != 0 {
            let _ = self.changed.send(());
        }
        Ok(())
    }

    /// Mark read the Activity row of a thread rooted at `root` (opened).
    pub fn mark_thread_read(&self, owner: UserId, root: &str) -> Result<(), StorageError> {
        self.mark_read(owner, &format!("{THREAD_ACTIVITY_PREFIX}{root}"))
    }

    pub fn mark_all_read(&self, owner: UserId) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        if conn.execute("UPDATE activity_items SET unread=0 WHERE owner_id=?1 AND unread=1", [owner.to_string()])? != 0 {
            let _ = self.changed.send(());
        }
        Ok(())
    }

    pub fn sync_missed_calls(&self, owner: UserId) -> Result<(), StorageError> {
        let calls = {
            let conn = self.db.read_connection()?;
            let mut stmt = conn.prepare(
                "SELECT id,conversation_id,conversation_type,peer_name,started_at
                 FROM call_history WHERE direction='incoming' AND result='missed'",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?, row.get::<_, String>(3)?, row.get::<_, String>(4)?))
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        for (id, conversation_id, conversation_type, peer_name, started_at) in calls {
            let created_at_ms = DateTime::parse_from_rfc3339(&started_at)
                .map(|time| time.timestamp_millis()).unwrap_or_else(|_| Utc::now().timestamp_millis());
            if created_at_ms < Utc::now().timestamp_millis() - 30 * 24 * 60 * 60 * 1000 { continue; }
            let actor_id = if conversation_type == "server_dm" { conversation_id.parse().ok() } else { None };
            self.record(owner, NewActivity {
                id: &format!("missed-call:{id}"), kind: "missed-call", actor_id,
                actor_name: &peer_name, detail: "", preview: "", conversation_id: Some(&conversation_id),
                conversation_type: Some(&conversation_type), message_id: None, created_at_ms,
            })?;
        }
        Ok(())
    }

    /// One activity row per outage. Recovery rewrites that row rather than
    /// adding one, and a drop within `OUTAGE_REOPEN_MS` of recovery reopens it,
    /// so a reconnect that flaps reads as one outage, not a list of them.
    pub fn server_reachability(&self, owner: UserId, unreachable: bool) -> Result<(), StorageError> {
        const OUTAGE_REOPEN_MS: i64 = 120_000;
        let conn = self.db.connection()?;
        let owner_id = owner.to_string();
        let current: Option<(String, Option<i64>)> = conn.query_row(
            "SELECT outage_id, resolved_at_ms FROM activity_outages WHERE owner_id=?1",
            [&owner_id], |row| Ok((row.get(0)?, row.get(1)?)),
        ).optional()?;
        let now = Utc::now().timestamp_millis();
        match (unreachable, current) {
            (true, Some((_, None))) | (false, None) | (false, Some((_, Some(_)))) => return Ok(()),
            (true, Some((id, Some(resolved)))) if now - resolved < OUTAGE_REOPEN_MS => {
                conn.execute("UPDATE activity_outages SET resolved_at_ms=NULL WHERE owner_id=?1", [&owner_id])?;
                conn.execute("UPDATE activity_items SET kind='server', detail='', unread=1 WHERE owner_id=?1 AND id=?2",
                    params![owner_id, id])?;
            }
            (true, _) => {
                let id = format!("server-down:{now}");
                conn.execute("INSERT OR REPLACE INTO activity_outages(owner_id,outage_id,resolved_at_ms) VALUES (?1,?2,NULL)",
                    params![owner_id, id])?;
                conn.execute("INSERT OR IGNORE INTO activity_items(owner_id,id,kind,created_at_ms) VALUES (?1,?2,'server',?3)",
                    params![owner_id, id, now])?;
                Self::prune(&conn, owner)?;
            }
            (false, Some((id, None))) => {
                let started: i64 = conn.query_row(
                    "SELECT created_at_ms FROM activity_items WHERE owner_id=?1 AND id=?2",
                    params![owner_id, id], |row| row.get(0),
                ).optional()?.unwrap_or(now);
                let secs = ((now - started) / 1000).max(1);
                let span = if secs < 120 { format!("{secs} s") } else { format!("{} min", secs / 60) };
                conn.execute("UPDATE activity_outages SET resolved_at_ms=?2 WHERE owner_id=?1", params![owner_id, now])?;
                // A blip of a few seconds stays in the log but must not raise the badge.
                const BLIP_SECS: i64 = 10;
                conn.execute("UPDATE activity_items SET kind='server-ok', detail=?3, unread=CASE WHEN ?4 THEN 0 ELSE unread END WHERE owner_id=?1 AND id=?2",
                    params![owner_id, id, span, secs < BLIP_SECS])?;
            }
        }
        let _ = self.changed.send(());
        Ok(())
    }
}

fn activity_title(kind: &str, actor_id: Option<&str>, actor_name: &str, detail: &str, names: &HashMap<UserId, String>) -> String {
    let resolved = actor_id.and_then(|id| id.parse::<UserId>().ok())
        .and_then(|id| names.get(&id).map(|name| (id, name)));
    let name = resolved.as_ref()
        .filter(|(id, name)| name.as_str() != crate::services::user::friend_display_name_placeholder(id).as_str())
        .map(|(_, name)| name.as_str())
        .or_else(|| (!actor_name.is_empty()).then_some(actor_name)).unwrap_or("Someone");
    match kind {
        "mention" => format!("{name} mentioned you{detail}"),
        "reply" => format!("{name} replied to you{detail}"),
        "thread" => format!("{name} replied in a thread{detail}"),
        "reaction" => format!("{name} reacted to your message"),
        "contact-request" => format!("{name} sent you a contact request"),
        "invite" | "mesh-invite" => format!("{name} invited you to {detail}"),
        "missed-call" => if actor_id.is_some() || !actor_name.is_empty() {
            format!("Missed call from {name}")
        } else { "Missed call".to_owned() },
        "device-linked" => if detail.is_empty() { "New device linked".to_owned() }
            else { format!("New device linked: {detail}") },
        "server" => "Server unreachable — messages will send later".to_owned(),
        "server-ok" => format!("Server was unreachable for {detail} — back online"),
        _ => kind.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item<'a>(id: &'a str, preview: &'a str, at: i64) -> NewActivity<'a> {
        NewActivity {
            id, kind: "reply", actor_id: None, actor_name: "", detail: "", preview,
            conversation_id: None, conversation_type: None, message_id: None, created_at_ms: at,
        }
    }

    #[test]
    fn reading_a_conversation_reads_its_message_rows_but_not_its_threads() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("activity.db"), "test-only").unwrap();
        db.run_migrations().unwrap();
        let service = ActivityService::new(Arc::new(db));
        let owner = UserId::new();
        let now = Utc::now().timestamp_millis();
        let mut mention = item("mention:m1", "", now);
        mention.kind = "mention";
        mention.conversation_id = Some("c1");
        let mut thread = item("thread:root", "", now);
        thread.conversation_id = Some("c1");
        let mut other = item("reply:m2", "", now);
        other.conversation_id = Some("c2");
        for row in [mention, thread, other] {
            service.record(owner, row).unwrap();
        }
        service.mark_conversation_read(owner, "c1").unwrap();
        let unread: Vec<_> = service
            .list(owner, true, |_| HashMap::new())
            .unwrap()
            .into_iter()
            .filter(|i| i.unread)
            .map(|i| i.id)
            .collect();
        assert_eq!(unread.len(), 2);
        assert!(unread.contains(&"thread:root".to_string()));
        assert!(unread.contains(&"reply:m2".to_string()));
        service.mark_thread_read(owner, "root").unwrap();
        assert_eq!(service.unread_count(owner).unwrap(), 1);
    }

    #[test]
    fn thread_rows_refresh_and_other_rows_are_recorded_once() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("activity.db"), "test-only").unwrap();
        db.run_migrations().unwrap();
        let service = ActivityService::new(Arc::new(db));
        let owner = UserId::new();
        let now = Utc::now().timestamp_millis();
        service.record(owner, item("thread:root", "first", now - 2)).unwrap();
        service.record(owner, item("reply:m1", "once", now - 2)).unwrap();
        service.mark_all_read(owner).unwrap();
        service.record(owner, item("thread:root", "second", now - 1)).unwrap();
        service.record(owner, item("reply:m1", "again", now - 1)).unwrap();
        let items = service.list(owner, true, |_| HashMap::new()).unwrap();
        let thread = items.iter().find(|i| i.id == "thread:root").unwrap();
        let reply = items.iter().find(|i| i.id == "reply:m1").unwrap();
        assert_eq!((thread.preview.as_str(), thread.unread), ("second", true));
        assert_eq!((reply.preview.as_str(), reply.unread), ("once", false));
        assert_eq!(items.len(), 2);
    }
}

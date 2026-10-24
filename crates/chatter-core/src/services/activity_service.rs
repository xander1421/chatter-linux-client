//! Account-scoped Activity Center feed in the encrypted client database.

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
    pub title: &'a str,
    pub preview: &'a str,
    pub conversation_id: Option<&'a str>,
    pub conversation_type: Option<&'a str>,
    pub message_id: Option<&'a str>,
    pub created_at_ms: i64,
}

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

    pub fn record(&self, owner: UserId, item: NewActivity<'_>) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let inserted = conn.execute(
            "INSERT OR IGNORE INTO activity_items
             (owner_id,id,kind,title,preview,conversation_id,conversation_type,message_id,created_at_ms)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![owner.to_string(), item.id, item.kind, item.title, item.preview,
                item.conversation_id, item.conversation_type, item.message_id, item.created_at_ms],
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
        conn.execute(
            "DELETE FROM activity_items WHERE owner_id=?1 AND id NOT IN
             (SELECT id FROM activity_items WHERE owner_id=?1 ORDER BY created_at_ms DESC,id DESC LIMIT 500)",
            [&owner],
        )?;
        Ok(())
    }

    pub fn list(&self, owner: UserId, show_preview: bool) -> Result<Vec<ActivityItem>, StorageError> {
        Self::prune(&*self.db.connection()?, owner)?;
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT id,kind,title,preview,created_at_ms,unread,conversation_type FROM activity_items
             WHERE owner_id=?1 ORDER BY created_at_ms DESC,id DESC LIMIT 500",
        )?;
        let now = Local::now();
        let rows = stmt.query_map([owner.to_string()], |row| {
            let created_ms: i64 = row.get(4)?;
            let created = DateTime::<Utc>::from_timestamp_millis(created_ms)
                .unwrap_or_else(Utc::now).with_timezone(&Local);
            let today = created.date_naive() == now.date_naive();
            let minutes = (now.timestamp_millis() - created_ms).max(0) / 60_000;
            let time_label = if minutes < 1 { "now".to_owned() }
                else if minutes < 60 { format!("{minutes}m") }
                else if today { created.format("%H:%M").to_string() }
                else if minutes < 2 * 24 * 60 { "Yesterday".to_owned() }
                else { created.format("%d %b").to_string() };
            Ok(ActivityItem {
                id: row.get(0)?, kind: row.get(1)?, title: row.get(2)?,
                preview: if show_preview && !matches!(row.get::<_, Option<String>>(6)?.as_deref(), Some("anon_dm" | "anon_group")) {
                    row.get(3)?
                } else { String::new() },
                time_label, section: if today { "Today" } else { "Earlier" }.to_owned(),
                unread: row.get::<_, i64>(5)? != 0,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
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

    pub fn mark_read(&self, owner: UserId, id: &str) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        if conn.execute("UPDATE activity_items SET unread=0 WHERE owner_id=?1 AND id=?2 AND unread=1", params![owner.to_string(), id])? != 0 {
            let _ = self.changed.send(());
        }
        Ok(())
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
            let title = if peer_name.is_empty() { "Missed call".to_owned() } else { format!("Missed call from {peer_name}") };
            self.record(owner, NewActivity {
                id: &format!("missed-call:{id}"), kind: "missed-call", title: &title,
                preview: "", conversation_id: Some(&conversation_id),
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
        let down_title = "Server unreachable — messages will send later";
        match (unreachable, current) {
            (true, Some((_, None))) | (false, None) | (false, Some((_, Some(_)))) => return Ok(()),
            (true, Some((id, Some(resolved)))) if now - resolved < OUTAGE_REOPEN_MS => {
                conn.execute("UPDATE activity_outages SET resolved_at_ms=NULL WHERE owner_id=?1", [&owner_id])?;
                conn.execute("UPDATE activity_items SET kind='server', title=?3, unread=1 WHERE owner_id=?1 AND id=?2",
                    params![owner_id, id, down_title])?;
            }
            (true, _) => {
                let id = format!("server-down:{now}");
                conn.execute("INSERT OR REPLACE INTO activity_outages(owner_id,outage_id,resolved_at_ms) VALUES (?1,?2,NULL)",
                    params![owner_id, id])?;
                conn.execute("INSERT OR IGNORE INTO activity_items(owner_id,id,kind,title,created_at_ms) VALUES (?1,?2,'server',?3,?4)",
                    params![owner_id, id, down_title, now])?;
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
                conn.execute("UPDATE activity_items SET kind='server-ok', title=?3 WHERE owner_id=?1 AND id=?2",
                    params![owner_id, id, format!("Server was unreachable for {span} — back online")])?;
            }
        }
        let _ = self.changed.send(());
        Ok(())
    }
}

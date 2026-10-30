//! Shared-content façade for Photos, Videos, Audio, Links, Voice, GIFs, and Files.
//!
//! Reads the `shared_content` index (migration v064) and returns display-ready
//! rows. Categorisation, ordering and paging all happen below this line; the
//! UI picks a tab id and an offset and renders what comes back.

use std::sync::Arc;
use std::sync::Mutex;

use crate::client::ChatterClient;
use crate::domain::attachment::SharedCategory;
use crate::services::search::{SearchCategory, SearchDocument, SearchQuery};
use crate::state::ActiveConversation;
use crate::storage::repositories::SharedContentRepository;
use crate::storage::repositories::SharedContentRow;
use crate::storage::repositories::settings_repo::SettingsRepository;
use crate::ui::{SharedContentItemData, SharedTabData};

static SEARCH_HISTORY_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Debug)]
pub struct SearchFilterOption {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub avatar: Option<Vec<u8>>,
}

impl ChatterClient {
    /// The open conversation as a search "In:" filter (id, label), when it is
    /// one search can scope to: a friend DM or a channel.
    pub fn active_search_scope(&self) -> Option<(String, String)> {
        use crate::state::ActiveConversation;
        match self.active_conversation() {
            ActiveConversation::ServerDm { peer_id } => {
                Some((peer_id.to_string(), self.display_name(&peer_id)))
            }
            ActiveConversation::Channel(id) => {
                let name = self.get_channel_local(&id)?.name;
                Some((id.to_string(), name))
            }
            _ => None,
        }
    }

    /// Eligible local picker rows, shaped in core using the same visibility gates as search.
    pub fn search_filter_options(&self, kind: &str, query: &str) -> Vec<SearchFilterOption> {
        let needle = query.trim().to_lowercase();
        let mut rows = if kind == "from" {
            self.dm_peers()
                .into_iter()
                .map(|id| SearchFilterOption {
                    id: id.to_string(),
                    name: self.display_name(&id),
                    kind: "person".into(),
                    avatar: self.user_avatar(&id),
                })
                .collect::<Vec<_>>()
        } else {
            self.quick_switcher_entries("")
                .into_iter()
                .filter(|entry| matches!(entry.conv_type.as_str(), "dm" | "channel"))
                .map(|entry| {
                    let kind = match entry.conv_type.as_str() {
                        "dm" => crate::domain::message::ConversationType::ServerDm,
                        "channel" => crate::domain::message::ConversationType::Channel,
                        _ => unreachable!("search filter only lists DMs and channels"),
                    };
                    let avatar = self.forward_destination_avatar(&kind, &entry.id);
                    SearchFilterOption {
                        id: entry.id,
                        name: entry.name,
                        kind: entry.conv_type,
                        avatar,
                    }
                })
                .collect::<Vec<_>>()
        };
        rows.retain(|row| row.name.to_lowercase().contains(&needle));
        rows.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        rows.dedup_by(|a, b| a.id == b.id);
        rows
    }
    fn search_history_key(&self) -> Option<String> {
        if self.anon_mode_enabled()
            || self
                .active_conversation_type()
                .is_some_and(|kind| kind.is_secret_chat())
        {
            return None;
        }
        let id = self.auth_state().get().current_user_id?;
        Some(format!("ui.search_history.{}", id.0))
    }

    /// Device-local, encrypted, per-account search history; absent in Shield mode.
    pub fn recent_searches(&self) -> Vec<String> {
        let _lock = SEARCH_HISTORY_LOCK.lock().unwrap();
        let Some(key) = self.search_history_key() else {
            return Vec::new();
        };
        let rows = SettingsRepository::new(self.services.database.clone())
            .get::<Vec<String>>(&key)
            .ok()
            .flatten()
            .unwrap_or_default();
        if self.search_history_key().as_deref() == Some(key.as_str()) {
            rows
        } else {
            Vec::new()
        }
    }

    pub fn remember_search(&self, query: &str) -> Vec<String> {
        self.change_recent_searches(|rows| {
            let query = query.trim();
            if !query.is_empty() {
                rows.retain(|item| item != query);
                rows.insert(0, query.to_owned());
                rows.truncate(6);
            }
        })
    }

    pub fn remove_recent_search(&self, query: &str) -> Vec<String> {
        self.change_recent_searches(|rows| rows.retain(|item| item != query))
    }

    pub fn clear_recent_searches(&self) -> Vec<String> {
        self.change_recent_searches(Vec::clear)
    }

    fn change_recent_searches(&self, change: impl FnOnce(&mut Vec<String>)) -> Vec<String> {
        let _lock = SEARCH_HISTORY_LOCK.lock().unwrap();
        let Some(key) = self.search_history_key() else {
            return Vec::new();
        };
        let repository = SettingsRepository::new(self.services.database.clone());
        let mut rows = repository
            .get::<Vec<String>>(&key)
            .ok()
            .flatten()
            .unwrap_or_default();
        change(&mut rows);
        if self.search_history_key().as_deref() != Some(key.as_str()) {
            return Vec::new();
        }
        if repository.set(&key, &rows).is_err() {
            tracing::warn!("recent searches not saved");
        }
        rows
    }
}

impl ChatterClient {
    /// Count visible search rows without retaining the index or its thumbnails.
    pub fn count_shared_content_visible(
        &self,
        query: &SearchQuery,
        exclude: &std::collections::HashSet<crate::domain::ids::MessageId>,
    ) -> [i32; 7] {
        let joined_channels: std::collections::HashSet<String> = self
            .list_channels().unwrap_or_default().into_iter()
            .map(|channel| channel.id.to_string()).collect();
        let mut seen = exclude.clone();
        let mut counts = [0; 7];
        let mut offset = 0;
        const BATCH: u32 = 250;
        loop {
            let batch = match self.shared_content_repo().search_global_scoped(
                &query.text,
                query.from.as_deref(),
                query.in_chat.as_deref(),
                query.since.as_ref().and_then(|date| date.timestamp_nanos_opt()),
                query.until.as_ref().and_then(|date| date.timestamp_nanos_opt()),
                None, "newest", BATCH, offset,
            ) {
                Ok(batch) => batch,
                Err(error) => {
                    tracing::error!(%error, "shared content search count");
                    break;
                }
            };
            let count = batch.len();
            for row in batch {
                let Ok(message_id) = row.message_id.parse() else { continue };
                if !seen.insert(message_id) { continue; }
                let Some(message) = self.get_message_local(&message_id) else { continue };
                if message.conversation_type == crate::domain::message::ConversationType::Channel
                    && !joined_channels.contains(&message.conversation_id.to_string())
                {
                    continue;
                }
                if message.conversation_type.is_secret_chat()
                    || !self.visible_in_mode(&message.conversation_type)
                    || self.is_blocked_conversation(
                        &message.conversation_type,
                        &message.conversation_id.to_string(),
                    )
                {
                    continue;
                }
                let category = SearchCategory::from_shared_kind(&row.category);
                if query.category != SearchCategory::All && query.category != category {
                    continue;
                }
                counts[0] += 1;
                let slot = match category {
                    SearchCategory::Media => 3,
                    SearchCategory::Files => 4,
                    SearchCategory::Links => 5,
                    SearchCategory::Voice => 6,
                    _ => continue,
                };
                counts[slot] += 1;
            }
            if count < BATCH as usize { break; }
            offset += BATCH;
        }
        counts
    }

    /// Search the local shared-content index across only conversations that
    /// are visible in the current Shield mode. No server query is involved.
    pub fn search_shared_content_visible(
        &self,
        query: &SearchQuery,
        limit: usize,
        exclude: &std::collections::HashSet<crate::domain::ids::MessageId>,
    ) -> Vec<SharedContentRow> {
        let joined_channels: std::collections::HashSet<String> = self
            .list_channels()
            .unwrap_or_default()
            .into_iter()
            .map(|channel| channel.id.to_string())
            .collect();
        let mut candidates = Vec::with_capacity(limit.min(250));
        let mut seen = exclude.clone();
        let mut offset = 0;
        const BATCH: u32 = 250;
        while candidates.len() < limit {
            let fetch = BATCH;
            let batch = match self.shared_content_repo().search_global_scoped(
                &query.text,
                query.from.as_deref(),
                query.in_chat.as_deref(),
                query.since.as_ref().and_then(|date| date.timestamp_nanos_opt()),
                query.until.as_ref().and_then(|date| date.timestamp_nanos_opt()),
                match query.category {
                    SearchCategory::Media => Some("media"),
                    SearchCategory::Files => Some("files"),
                    SearchCategory::Links => Some("links"),
                    SearchCategory::Voice => Some("voice"),
                    _ => None,
                },
                match query.sort {
                    crate::services::search::SearchSort::Oldest => "oldest",
                    crate::services::search::SearchSort::Largest => "largest",
                    _ => "newest",
                },
                fetch,
                offset,
            ) {
                Ok(batch) => batch,
                Err(error) => {
                    tracing::error!(%error, "shared content search");
                    break;
                }
            };
            let count = batch.len();
            for row in batch {
                let Ok(message_id) = row.message_id.parse() else { continue };
                if !seen.insert(message_id) { continue; }
                let Some(message) = self.get_message_local(&message_id) else { continue };
                if message.conversation_type == crate::domain::message::ConversationType::Channel
                    && !joined_channels.contains(&message.conversation_id.to_string())
                {
                    continue;
                }
                if message.conversation_type.is_secret_chat()
                    || !self.visible_in_mode(&message.conversation_type)
                    || self.is_blocked_conversation(
                        &message.conversation_type,
                        &message.conversation_id.to_string(),
                    )
                {
                    continue;
                }
                let category = SearchCategory::from_shared_kind(&row.category);
                candidates.push(SharedSearchHit {
                    sender_id: message.sender_id.map(|id| id.to_string()),
                    chat_id: message.conversation_id.to_string(),
                    row,
                    category,
                });
                if candidates.len() == limit { break; }
            }
            if count < fetch as usize {
                break;
            }
            offset += fetch;
        }
        query
            .apply(candidates)
            .into_iter()
            .take(limit)
            .map(|hit| hit.row)
            .collect()
    }
    /// One page of a conversation's shared-content tab, newest first.
    ///
    /// Async because `SharedContentRepository` is a synchronous SQLite handle:
    /// the read and the row shaping run on a blocking worker. The profile
    /// screen is painted from the Slint event loop, and a SQLite read there
    /// stalls every frame in the app.
    ///
    /// An unknown `category`, or a conversation with no routing key, yields an
    /// empty page. Storage and worker failures are returned to the caller.
    pub async fn shared_content_page(
        self: &Arc<Self>,
        conv: ActiveConversation,
        category: &str,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<SharedContentItemData>, crate::error::StorageError> {
        self.shared_content_page_scoped(conv, category, limit, offset, None)
            .await
    }

    /// Shared content optionally restricted to the selected sender in this chat.
    pub async fn shared_content_page_scoped(
        self: &Arc<Self>,
        conv: ActiveConversation,
        category: &str,
        limit: u32,
        offset: u32,
        sender: Option<crate::domain::ids::UserId>,
    ) -> Result<Vec<SharedContentItemData>, crate::error::StorageError> {
        let Some(category) = SharedCategory::from_str_value(category) else {
            return Ok(Vec::new());
        };
        let Some((conv_type, conv_id)) = conv.routing_key() else {
            return Ok(Vec::new());
        };
        let client = self.clone();
        #[cfg(target_os = "android")]
        let revision = self.auth_snapshot().session_revision;
        tokio::task::spawn_blocking(move || {
            client.shared_content_repo().list_scoped(
                &conv_type, &conv_id, category, limit, offset, sender.as_ref(),
            ).map(|rows| rows
                    .iter()
                    .map(|row| {
                        let mut item = SharedContentItemData::from(row);
                        item.is_video = category == SharedCategory::Video;
                        #[cfg(target_os = "android")]
                        if item.is_video
                            && item.thumbnail.is_empty()
                            && client.auth_snapshot().session_revision == revision
                        {
                            if let Ok(id) = item.attachment_id.parse() {
                                if client
                                    .get_attachment_metadata(&id)
                                    .is_some_and(|attachment| !attachment.needs_download())
                                {
                                    if let Ok(result) =
                                        client.decrypt_and_extract_video_thumbnail(&id)
                                    {
                                        item.thumbnail = result.image_bytes;
                                    }
                                }
                            }
                        }
                        item
                    })
                    .collect())
        })
        .await
        .map_err(|error| crate::error::StorageError::ConnectionPool(error.to_string()))?
    }

    /// The tab strip for a conversation: label + count per non-empty category,
    /// in fixed display order. Empty when nothing has ever been shared.
    ///
    /// This is also where history that arrived without being indexed (a backup
    /// restore or a linked-device history import writes rows directly) gets
    /// backfilled — once, gated on a `settings` marker. Opening a profile is
    /// the natural moment: it is the only screen that reads the index, and the
    /// scan is already off the UI thread here.
    pub async fn shared_content_tabs(
        self: &Arc<Self>,
        conv: ActiveConversation,
    ) -> Result<Vec<SharedTabData>, crate::error::StorageError> {
        self.shared_content_tabs_scoped(conv, None).await
    }

    /// Counts use the same conversation and sender scope as paged results.
    pub async fn shared_content_tabs_scoped(
        self: &Arc<Self>,
        conv: ActiveConversation,
        sender: Option<crate::domain::ids::UserId>,
    ) -> Result<Vec<SharedTabData>, crate::error::StorageError> {
        let Some((conv_type, conv_id)) = conv.routing_key() else {
            return Ok(Vec::new());
        };
        let repo = self.shared_content_repo();
        let db = self.services.database.clone();
        tokio::task::spawn_blocking(move || {
            backfill_once(&repo, &db, &conv_type, &conv_id);
            repo.counts_scoped(&conv_type, &conv_id, sender.as_ref())
                .map(|counts| crate::ui::shared_tabs(&counts))
        })
        .await
        .map_err(|error| crate::error::StorageError::ConnectionPool(error.to_string()))?
    }

    fn shared_content_repo(&self) -> SharedContentRepository {
        SharedContentRepository::new(self.services.database.clone())
    }

    /// Synchronous: indexed point-lookups on an already-open connection,
    /// cheap enough to run while building the modal. None for a blocked
    /// user (their presence is hidden, so is when it last changed); the
    /// screen then shows the offline presence it is given.
    pub fn peer_last_seen(
        self: &Arc<Self>,
        user_id: &crate::domain::ids::UserId,
    ) -> Option<(crate::domain::user::PresenceStatus, Option<i64>)> {
        if self.is_profile_user_blocked(user_id) {
            return None;
        }
        let repo =
            crate::storage::repositories::UserRepository::new(self.services.database.clone());
        Some((
            self.peer_presence(user_id),
            repo.last_seen_at(user_id).ok().flatten(),
        ))
    }
}

struct SharedSearchHit {
    row: SharedContentRow,
    category: SearchCategory,
    sender_id: Option<String>,
    chat_id: String,
}

impl SearchDocument for SharedSearchHit {
    fn category(&self) -> SearchCategory {
        self.category
    }
    fn search_text(&self) -> &str {
        &self.row.title
    }
    fn created_at(&self) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from_timestamp_nanos(self.row.sent_at as i64)
    }
    fn size_bytes(&self) -> u64 {
        self.row.size_bytes
    }
    fn sender_id(&self) -> Option<&str> {
        self.sender_id.as_deref()
    }
    fn chat_id(&self) -> &str {
        &self.chat_id
    }
}

/// Index a conversation's unindexed (restored) history, at most once per conversation.
///
/// Blocking — callers are already on a worker. Failures are logged and
/// swallowed: a drawer missing its old items is worse than an error dialog,
/// but not worth failing the screen over.
fn backfill_once(
    repo: &SharedContentRepository,
    db: &Arc<crate::storage::Database>,
    conv_type: &crate::domain::message::ConversationType,
    conv_id: &str,
) {
    let settings = crate::storage::repositories::SettingsRepository::new(db.clone());
    let key = format!(
        "shared_content.backfilled.{}.{}",
        conv_type.as_str(),
        conv_id
    );
    if settings.get::<bool>(&key).ok().flatten().unwrap_or(false) {
        return;
    }
    let Ok(channel_id) = conv_id.parse::<crate::domain::ids::ChannelId>() else {
        return;
    };
    let messages = crate::storage::repositories::UnifiedMessageRepository::new(db.clone());
    match repo.backfill(&messages, conv_type, &channel_id) {
        Ok(n) => {
            tracing::info!(count = n, conversation = %conv_id, "Backfilled shared-content index");
            let _ = settings.set(&key, &true);
        }
        Err(e) => {
            tracing::warn!(error = %e, conversation = %conv_id, "shared-content backfill failed")
        }
    }
}

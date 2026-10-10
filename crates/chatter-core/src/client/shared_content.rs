//! Shared-content façade — the profile screen's Media / Files / Links / Voice
//! / GIFs / Music tabs.
//!
//! Reads the `shared_content` index (migration v064) and returns display-ready
//! rows. Categorisation, ordering and paging all happen below this line; the
//! UI picks a tab id and an offset and renders what comes back.

use std::sync::Arc;

use crate::client::ChatterClient;
use crate::domain::attachment::SharedCategory;
use crate::state::ActiveConversation;
use crate::storage::repositories::SharedContentRepository;
use crate::ui::{SharedContentItemData, SharedTabData};

impl ChatterClient {
    /// One page of a conversation's shared-content tab, newest first.
    ///
    /// Async because `SharedContentRepository` is a synchronous SQLite handle:
    /// the read and the row shaping run on a blocking worker. The profile
    /// screen is painted from the Slint event loop, and a SQLite read there
    /// stalls every frame in the app.
    ///
    /// An unknown `category`, or a conversation with no routing key, yields an
    /// empty page rather than an error — there is nothing a caller could do
    /// with the error that differs from rendering an empty tab.
    pub async fn shared_content_page(
        self: &Arc<Self>,
        conv: ActiveConversation,
        category: &str,
        limit: u32,
        offset: u32,
    ) -> Vec<SharedContentItemData> {
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
    ) -> Vec<SharedContentItemData> {
        let Some(category) = SharedCategory::from_str_value(category) else {
            return Vec::new();
        };
        let Some((conv_type, conv_id)) = conv.routing_key() else {
            return Vec::new();
        };
        let repo = self.shared_content_repo();
        tokio::task::spawn_blocking(move || {
            match repo.list_scoped(
                &conv_type,
                &conv_id,
                category,
                limit,
                offset,
                sender.as_ref(),
            ) {
                Ok(rows) => rows.iter().map(SharedContentItemData::from).collect(),
                Err(e) => {
                    tracing::error!(error = %e, "shared content page");
                    Vec::new()
                }
            }
        })
        .await
        .unwrap_or_default()
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
    ) -> Vec<SharedTabData> {
        self.shared_content_tabs_scoped(conv, None).await
    }

    /// Counts use the same conversation and sender scope as paged results.
    pub async fn shared_content_tabs_scoped(
        self: &Arc<Self>,
        conv: ActiveConversation,
        sender: Option<crate::domain::ids::UserId>,
    ) -> Vec<SharedTabData> {
        let Some((conv_type, conv_id)) = conv.routing_key() else {
            return Vec::new();
        };
        let repo = self.shared_content_repo();
        let db = self.services.database.clone();
        tokio::task::spawn_blocking(move || {
            backfill_once(&repo, &db, &conv_type, &conv_id);
            crate::ui::shared_tabs(
                &repo
                    .counts_scoped(&conv_type, &conv_id, sender.as_ref())
                    .unwrap_or_default(),
            )
        })
        .await
        .unwrap_or_default()
    }

    fn shared_content_repo(&self) -> SharedContentRepository {
        SharedContentRepository::new(self.services.database.clone())
    }

    /// The line under a contact's name on the profile screen — "online",
    /// "last seen yesterday 4:12 pm", or "last seen recently" when no presence
    /// timestamp has ever reached us for them.
    ///
    /// Synchronous: indexed point-lookups on an already-open connection,
    /// cheap enough to run while building the modal. Empty for a blocked
    /// user (their presence is hidden, so is when it last changed); the
    /// screen then shows the offline presence it is given.
    pub fn peer_last_seen_label(self: &Arc<Self>, user_id: &crate::domain::ids::UserId) -> String {
        if self.is_profile_user_blocked(user_id) {
            return String::new();
        }
        let repo =
            crate::storage::repositories::UserRepository::new(self.services.database.clone());
        crate::ui::chat_data::last_seen_label(
            self.peer_presence(user_id),
            repo.last_seen_at(user_id).ok().flatten(),
        )
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

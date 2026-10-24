use super::*;

impl ChatterClient {
    /// Local global search excludes device-bound secret chats even though
    /// their rows remain in the sidebar in either mode. Returns matching
    /// regular messages, best match first.
    pub fn search_messages_visible(
        &self,
        query: &str,
        limit: u32,
    ) -> Vec<crate::domain::message::Message> {
        let request = crate::services::search::SearchQuery::from_controls(
            query,
            "all",
            "relevance",
            "all",
        );
        self.search_messages_visible_page(&request, limit, 0).0
    }

    pub(crate) fn search_messages_visible_page(
        &self,
        request: &crate::services::search::SearchQuery,
        limit: u32,
        offset: u32,
    ) -> (Vec<crate::domain::message::Message>, usize) {
        let joined_channels: std::collections::HashSet<_> = self
            .list_channels()
            .unwrap_or_default()
            .into_iter()
            .map(|channel| channel.id)
            .collect();
        let hits = match self.messages().search_local_scoped(
            &request.text,
            request.from.as_deref(),
            request.in_chat.as_deref(),
            request.since.as_ref().and_then(|date| date.timestamp_nanos_opt()),
            request.until.as_ref().and_then(|date| date.timestamp_nanos_opt()),
            limit,
            offset,
        ) {
            Ok(hits) => hits,
            Err(e) => {
                tracing::error!("Search failed: {}", e);
                return (Vec::new(), 0);
            }
        };
        let fetched = hits.len();
        let ids = hits.iter().map(|hit| hit.message_id).collect::<Vec<_>>();
        let messages = match self.messages().get_messages_by_ids_local(&ids) {
            Ok(messages) => messages,
            Err(error) => {
                tracing::error!(%error, "Search hydration failed");
                return (Vec::new(), 0);
            }
        };
        let mut by_id = messages
            .into_iter()
            .map(|msg| (msg.id, msg))
            .collect::<std::collections::HashMap<_, _>>();
        let visible = hits.into_iter()
            .filter_map(|hit| by_id.remove(&hit.message_id))
            .filter(|msg| {
                !msg.conversation_type.is_secret_chat()
                    && self.visible_in_mode(&msg.conversation_type)
                    && (msg.conversation_type != crate::domain::message::ConversationType::Channel
                        || joined_channels.contains(&msg.conversation_id))
                    && !self.is_blocked_conversation(
                        &msg.conversation_type,
                        &msg.conversation_id.to_string(),
                    )
            })
            .collect();
        (visible, fetched)
    }

    /// Jump-to / quick-switcher rows for `query`, ranked by the core rule
    /// (`services::search::filter_conversations`). Built from the same
    /// mode-filtered sources the sidebar renders — loose rows AND folder rows.
    /// Secret chats stay in the sidebar, but their names must not appear in
    /// the regular global search palette.
    ///
    /// `conv_type` uses the palette's vocabulary: "channel", "dm", "anon_dm",
    /// "private_group", "saved_messages" (id is the sentinel
    /// "saved-messages"), "mesh".
    pub fn quick_switcher_entries(
        &self,
        query: &str,
    ) -> Vec<crate::services::quick_switcher::QuickSwitcherEntry> {
        use crate::domain::message::ConversationType as CT;
        use crate::services::quick_switcher::QuickSwitcherEntry;

        let stats = self
            .services
            .conversation_repo
            .conversation_stats()
            .unwrap_or_default();
        let unread = |id: &str, kind: CT| {
            stats
                .get(&(id.to_string(), kind))
                .map_or(0, |s| s.unread_count)
        };
        let entry =
            |id: String, name: String, conv_type: &str, presence: String, unread_count: u32| {
                QuickSwitcherEntry {
                    id,
                    name,
                    conv_type: conv_type.to_string(),
                    presence_status: presence,
                    unread_count: i32::try_from(unread_count).unwrap_or(i32::MAX),
                }
            };

        let mut entries = Vec::new();
        if !self.anon_mode_enabled() {
            entries.push(entry(
                "saved-messages".into(),
                "Saved Messages".into(),
                "saved_messages",
                String::new(),
                0,
            ));
        }
        for dm in self.list_dm_sidebar() {
            if dm.is_anon_dm() {
                continue;
            }
            entries.push(entry(
                dm.id,
                dm.display_name,
                "dm",
                dm.presence_status,
                dm.unread_count,
            ));
        }
        for channel in self.sidebar_channels() {
            let id = channel.id.to_string();
            let count = unread(&id, channel.conversation_type());
            entries.push(entry(id, channel.name, "channel", String::new(), count));
        }
        for section in self.list_sections_with_conversations().unwrap_or_default() {
            for conv in section.conversations {
                if conv.conversation_type.is_secret_chat() {
                    continue;
                }
                let kind = match conv.conversation_type {
                    CT::Channel => "channel",
                    CT::ServerDm => "dm",
                    CT::AnonDm => "anon_dm",
                    CT::AnonGroup => "private_group",
                    CT::Mesh | CT::MeshDm => "mesh",
                    // Saved Messages has its own row.
                    CT::SavedMessages => continue,
                };
                entries.push(entry(
                    conv.conversation_id,
                    conv.display_name,
                    kind,
                    String::new(),
                    conv.unread_count,
                ));
            }
        }
        #[cfg(feature = "mesh")]
        let sectioned = self
            .sections()
            .all_sectioned_conversation_ids()
            .unwrap_or_default();
        #[cfg(feature = "mesh")]
        for conv in self.mesh_conversations().unwrap_or_default() {
            // Foldered rows came from the section loop; unresolvable ones
            // would open nothing.
            if sectioned.contains(&conv.id) || self.mesh_conversation_for(&conv.id).is_none() {
                continue;
            }
            entries.push(entry(
                conv.id,
                conv.name,
                "mesh",
                String::new(),
                conv.unread_count,
            ));
        }

        crate::services::search::filter_conversations(query, entries)
    }

    /// Where a message search hit opens, as the `(conv_type, id)` pair a
    /// [`Self::quick_switcher_entries`] row for its conversation carries — so
    /// a message hit and a conversation hit open through one path. A friend
    /// DM's `conversation_id` is the peer (the DM row's id); Saved Messages
    /// opens by its sentinel.
    pub fn search_hit_target(
        &self,
        msg: &crate::domain::message::Message,
    ) -> (&'static str, String) {
        use crate::domain::message::ConversationType as CT;
        let kind = match msg.conversation_type {
            CT::Channel => "channel",
            CT::ServerDm => "dm",
            CT::AnonDm => "anon_dm",
            CT::AnonGroup => "private_group",
            CT::SavedMessages => return ("saved_messages", "saved-messages".to_string()),
            CT::Mesh | CT::MeshDm => "mesh",
        };
        (kind, msg.conversation_id.to_string())
    }

    /// Enriched mention suggestion — everything the composer autocomplete
    /// needs without having to N+1 fetch profiles from the UI layer.
    pub async fn search_mention_suggestions(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
        query: &str,
    ) -> Vec<MentionSuggestionInfo> {
        let query_lower = query.to_lowercase();
        let mut out: Vec<MentionSuggestionInfo> = Vec::new();

        // Broadcast mentions always surface first while they fuzzy-match.
        if "here".starts_with(&query_lower) {
            out.push(MentionSuggestionInfo {
                id: "here".into(),
                display: "@here".into(),
                kind: MentionSuggestionKind::Broadcast,
                has_avatar: false,
            });
        }
        if "all".starts_with(&query_lower) || "everyone".starts_with(&query_lower) {
            out.push(MentionSuggestionInfo {
                id: "all".into(),
                display: "@all".into(),
                kind: MentionSuggestionKind::Broadcast,
                has_avatar: false,
            });
        }

        let members = match self.channels().get_channel_members(channel_id, 50, 0).await {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(error = %e, "search_mention_suggestions: members fetch failed");
                return out;
            }
        };

        // Cap the per-member profile fetch to the first 20 members and short-
        // circuit once we have enough matches. UI cap is 8 total.
        for member in members.into_iter().take(20) {
            if out.len() >= 8 {
                break;
            }
            let profile = match self.users().get_user_profile(&member.user_id).await {
                Ok(p) => p,
                Err(e) => {
                    tracing::debug!(
                        user = %member.user_id,
                        error = %e,
                        "search_mention_suggestions: profile fetch failed, falling back to id"
                    );
                    continue;
                }
            };
            let display_name = if profile.display_name.is_empty() {
                member.user_id.to_string()
            } else {
                profile.display_name.clone()
            };
            let user_id_str = member.user_id.to_string();
            let name_lower = display_name.to_lowercase();
            if name_lower.contains(&query_lower)
                || user_id_str.to_lowercase().contains(&query_lower)
            {
                out.push(MentionSuggestionInfo {
                    id: user_id_str,
                    display: format!("@{}", display_name),
                    kind: MentionSuggestionKind::User,
                    has_avatar: false,
                });
            }
        }

        out.truncate(8);
        out
    }
}

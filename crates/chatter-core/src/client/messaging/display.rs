use super::*;

impl ChatterClient {
    // ── Resolved display fields (all business rules live here, not in the UI) ──
    //
    // The presentation layer copies these straight onto its display struct and
    // must not branch on conversation type or re-resolve names itself.

    /// Header shown when an attachment is opened from the current conversation:
    /// sender, sent time and the message's text (the caption, raw markdown).
    /// Resolves the owner from the message rather than from attachment metadata,
    /// which has no sender or sent time.
    pub fn media_viewer_header(
        &self,
        conversation: &crate::state::ActiveConversation,
        message_id: Option<&crate::domain::ids::MessageId>,
        attachment_id: &crate::domain::ids::AttachmentId,
    ) -> Option<(String, String, String)> {
        let message = message_id
            .and_then(|id| self.get_message_local(id))
            .filter(|message| !message.is_deleted && message.attachments.contains(attachment_id))
            .or_else(|| {
                self.get_messages(conversation, i32::MAX as u32)
                    .ok()?
                    .into_iter()
                    .find(|message| !message.is_deleted && message.attachments.contains(attachment_id))
            })?;
        let sender = self.message_sender_display_name(&message);
        let sender = match message.sender_id.as_ref() {
            Some(id) => self.quote_author_name(id, || sender),
            None => sender,
        };
        let date = message.created_at.with_timezone(&chrono::Local).format("%b %-d, %Y");
        let time = crate::util::time::format_message_time(message.created_at.clone());
        let caption = message.as_text().unwrap_or_default().trim().to_owned();
        Some((sender, format!("{date} · {time}"), caption))
    }

    /// Sender display name, scoped to the conversation so anon channels show
    /// the per-channel name instead of the global profile. `None` sender →
    /// "Anonymous".
    pub fn message_sender_display_name(&self, msg: &crate::domain::message::Message) -> String {
        self.message_sender_display_names(std::slice::from_ref(msg))
            .pop()
            .unwrap_or_default()
    }

    /// Re-resolve a stored join notice after its subject's profile arrives.
    /// The notice stores the user ID, so a name unavailable at insertion time
    /// does not become permanent text in the message history.
    pub fn refreshed_join_notice(
        &self,
        message_id: &crate::domain::ids::MessageId,
        user_id: &crate::domain::ids::UserId,
    ) -> Option<(String, String)> {
        use crate::domain::message::{MessageContent, SystemMessage};

        let message = self.get_message_local(message_id)?;
        let MessageContent::System(system @ SystemMessage::UserJoined(subject)) = &message.content
        else {
            return None;
        };
        if subject != user_id || message.sender_id != Some(*user_id) {
            return None;
        }
        let name = self.message_sender_display_name(&message);
        let content = format!(
            "{} {}",
            name,
            system.description_for(&message.conversation_type)
        );
        Some((name, content))
    }

    /// [`Self::message_sender_display_name`] for a whole page, in order. Each
    /// distinct sender is resolved once, and every sender's directory name in
    /// a single query (`UserService::get_display_names`) — opening a
    /// conversation used to run that lookup per row.
    pub fn message_sender_display_names(
        &self,
        messages: &[crate::domain::message::Message],
    ) -> Vec<String> {
        use crate::domain::message::ConversationType;
        let me = self.session_current_user_id();
        let mut seen = std::collections::HashSet::new();
        let senders: Vec<crate::domain::ids::UserId> = messages
            .iter()
            .filter_map(|m| m.sender_id)
            .filter(|id| seen.insert(*id))
            .collect();
        let names = self.users().get_display_names(&senders);
        // Anon-group names are per group, so they key on the conversation too.
        let mut group_names = std::collections::HashMap::new();
        messages
            .iter()
            .map(|msg| {
                if let crate::domain::message::MessageContent::System(
                    crate::domain::message::SystemMessage::AnonGroupUserLeft {
                        left_name: Some(name),
                    },
                ) = &msg.content
                {
                    return name.clone();
                }
                // System lines about us read "You left the channel", as Signal's do.
                if matches!(
                    msg.content,
                    crate::domain::message::MessageContent::System(_)
                ) && msg.sender_id.is_some()
                    && msg.sender_id == me
                {
                    return "You".to_string();
                }
                let Some(id) = msg.sender_id else {
                    return "Anonymous".to_string();
                };
                if msg.conversation_type == ConversationType::AnonGroup {
                    let group_name = group_names
                        .entry((msg.conversation_id, id))
                        .or_insert_with(|| {
                            let group_id = crate::domain::anon_group::AnonGroupId::from_uuid(
                                *msg.conversation_id.as_uuid(),
                            );
                            self.get_anon_group_member_display_name(&group_id, &id)
                                .filter(|n| !n.is_empty())
                        })
                        .clone();
                    if let Some(name) = group_name {
                        return name;
                    }
                }
                names
                    .get(&id)
                    .cloned()
                    .unwrap_or_else(|| self.display_name(&id))
            })
            .collect()
    }

    /// Reply-quote author display name, scoped like the sender. Empty when the
    /// message is not a reply or carries no quoted author.
    pub fn reply_author_display_name(&self, msg: &crate::domain::message::Message) -> String {
        match msg.reply_to_author_id {
            Some(ref id) => self.quote_author_name(id, || {
                self.display_name_in(&msg.conversation_type, &msg.conversation_id, id)
            }),
            None => String::new(),
        }
    }

    /// Who a quote or the composer's reply bar names: "You" for our own
    /// message, as every messenger does — our public name there read as if
    /// someone else wrote it. Otherwise the name `resolve` gives.
    pub fn quote_author_name(
        &self,
        author: &crate::domain::ids::UserId,
        resolve: impl FnOnce() -> String,
    ) -> String {
        if self.session_current_user_id().as_ref() == Some(author) {
            "You".to_string()
        } else {
            resolve()
        }
    }

    /// Whether to show the "Original message not found" warning for a reply.
    ///
    /// Anon replies carry a self-contained quote snapshot and reference their
    /// parent via a synthetic timestamp-derived id that never matches the
    /// parent's stored id — so the by-id lookup is meaningless and we never
    /// warn. Non-anon replies use the real parent lookup.
    pub fn reply_parent_missing_for(&self, msg: &crate::domain::message::Message) -> bool {
        match msg.reply_to {
            Some(ref parent) if !msg.conversation_type.is_anon() => {
                self.reply_parent_missing(parent)
            }
            _ => false,
        }
    }

    /// Media descriptor of a reply's quoted parent — see [`ReplyParentMedia`].
    /// Lets the quote bubble render the parent's media richly (an inline audio
    /// player for audio parents, a thumbnail for visual parents) instead of
    /// plain text. Returns a default (all-empty) descriptor when the message is
    /// not a reply, the parent or its first attachment can't be resolved
    /// locally, or the parent carries no renderable media.
    pub fn reply_parent_media(&self, msg: &crate::domain::message::Message) -> ReplyParentMedia {
        use crate::domain::attachment::FileCategory;

        let Some(parent_id) = msg.reply_to.as_ref() else {
            return ReplyParentMedia::default();
        };
        let Some(parent) = self.get_message_local(parent_id) else {
            return ReplyParentMedia::default();
        };
        let Some(att_id) = parent.attachments.first() else {
            return ReplyParentMedia::default();
        };
        let Some(att) = self.get_attachment(att_id) else {
            return ReplyParentMedia::default();
        };

        let meta = &att.metadata;
        // GIFs are an image MIME but read as animated — split them out before
        // the generic image arm so the quote can badge them distinctly.
        let category = if meta.mime_type == "image/gif" {
            "gif"
        } else {
            match meta.category() {
                FileCategory::Audio => "audio",
                FileCategory::Image => "image",
                FileCategory::Video => "video",
                _ => return ReplyParentMedia::default(),
            }
        };

        let duration = meta
            .duration_seconds
            .map(|secs| {
                let total = secs as u64;
                format!("{}:{:02}", total / 60, total % 60)
            })
            .unwrap_or_default();

        // Local thumbnail fallback for visual quotes (image/gif/video); audio
        // has no thumbnail and renders its inline player instead.
        let local_thumbnail = match category {
            "image" | "gif" | "video" => meta.thumbnail.clone(),
            _ => None,
        };

        // Audio waveform (32 values, 0-100) mapped to i32 for the inline quote
        // player; empty when the parent carries none.
        let waveform = meta
            .waveform_samples
            .as_ref()
            .map(|samples| samples.iter().map(|&s| s as i32).collect())
            .unwrap_or_default();

        ReplyParentMedia {
            category: category.to_string(),
            attachment_id: att.id.to_string(),
            duration,
            thumbnail: local_thumbnail,
            filename: meta.name.clone(),
            waveform,
        }
    }

    /// Sender avatar bytes for a message row, resolved per conversation type
    /// (anon-group member; channels our own avatar or the sender's stored
    /// profile). `None` = initials.
    ///
    /// DMs (server and anon) get `None`: a DM row draws no sender avatar —
    /// there are two people and the header already shows the other one
    /// (`message_item.slint`, `!root.is-dm`). Resolving and decoding one per
    /// row anyway was 85% of the time it took to open a DM (400 ms of an
    /// 870 ms open on a Galaxy S9, `conversation_open_bench`).
    pub fn message_sender_avatar(&self, msg: &crate::domain::message::Message) -> Option<Vec<u8>> {
        use crate::domain::message::ConversationType;
        match msg.conversation_type {
            ConversationType::ServerDm | ConversationType::AnonDm => None,
            ConversationType::AnonGroup => {
                let group_id = crate::domain::anon_group::AnonGroupId::from_uuid(
                    *msg.conversation_id.as_uuid(),
                );
                let sid = msg.sender_id?;
                self.get_anon_group_member_avatar(&group_id, &sid)
            }
            _ => self.user_avatar(&msg.sender_id?),
        }
    }

    /// A user's avatar as this device knows it: ours from settings (our own
    /// profile row carries none), anyone else's from their stored, decrypted
    /// profile. `None` = initials.
    pub fn user_avatar(&self, user_id: &crate::domain::ids::UserId) -> Option<Vec<u8>> {
        let avatar = if self.session_current_user_id().as_ref() == Some(user_id) {
            self.my_avatar()
        } else {
            self.get_profile_local(user_id).and_then(|p| p.avatar_bytes)
        };
        avatar.filter(|bytes| !bytes.is_empty())
    }

    /// Stable `User <id8>` placeholder for a user with no resolvable name.
    /// Single source of truth so the UI never hand-rolls the truncation.
    pub fn fallback_display_name(&self, user_id: &crate::domain::ids::UserId) -> String {
        crate::services::user::friend_display_name_placeholder(user_id)
    }
}

use super::*;

impl ChatterClient {
    /// Whether `peer` shares an anon group with us and we have no identified
    /// relationship to it — not a friend, no server DM history. For such a
    /// peer the identified DM is a NEW link between the two identities; for a
    /// friend or an existing DM that link already exists. Unreadable anon
    /// membership is treated conservatively until storage is available again.
    pub(super) fn known_only_from_anon_group(&self, peer: &crate::domain::ids::UserId) -> bool {
        if self.is_friend(peer) {
            return false;
        }
        let has_server_dm = self
            .get_messages_local_asc(
                &crate::domain::ids::ChannelId::from(*peer),
                &ConversationType::ServerDm,
                None,
                1,
            )
            .is_ok_and(|m| !m.is_empty());
        if has_server_dm {
            return false;
        }
        // The sidebar façade turns storage errors into empty lists. Privacy
        // routing must distinguish absence from an unreadable membership:
        // without an established identified relationship, uncertainty may
        // only open an existing anonymous conversation.
        let Ok(groups) = self.anon_groups().list_groups() else {
            return true;
        };
        for group in groups {
            let Ok(members) = self.anon_groups().get_members(&group.id) else {
                return true;
            };
            if members.iter().any(|member| &member.user_id == peer) {
                return true;
            }
        }
        false
    }

    /// Every conversation a message can be forwarded to, filtered by a
    /// case-insensitive name substring and ordered by name.
    ///
    /// Sources are the same mode-exclusive sidebar builders the sidebar
    /// renders — anon mode never offers an identified destination and vice
    /// versa — PLUS the rows those builders drop because they live in a custom
    /// folder (filing a conversation is a layout choice, not a reason it cannot
    /// receive a forward), PLUS Saved Messages once its channel is known
    /// locally. Rows that cannot be resolved back to a conversation to send to
    /// (mesh, mailbox DMs, a stale id) are not destinations and are dropped.
    ///
    /// Not the picker's list on its own: the secret-chat rules depend on the
    /// messages being forwarded, so the picker asks
    /// [`crate::services::conversation::ConversationHandle::forward_destinations`].
    pub(crate) fn forward_destinations(&self, query: &str) -> Vec<ForwardDestination> {
        use crate::domain::message::ConversationType as CT;

        let mut out: Vec<ForwardDestination> = Vec::new();

        for dm in self.list_dm_sidebar() {
            out.push(ForwardDestination {
                kind: dm.kind,
                id: dm.id,
                name: dm.display_name,
            });
        }
        for channel in self
            .sidebar_channels()
            .into_iter()
            .filter(|c| !self.is_former_channel(&c.id))
        {
            out.push(ForwardDestination {
                kind: CT::Channel,
                id: channel.id.to_string(),
                name: channel.name,
            });
        }
        for group in self.sidebar_private_groups() {
            if !group.status.can_send() {
                continue;
            }
            out.push(ForwardDestination {
                kind: CT::AnonGroup,
                id: group.id.to_string(),
                name: group.name,
            });
        }
        for section in self.list_sections_with_conversations().unwrap_or_default() {
            for conv in section.conversations {
                out.push(ForwardDestination {
                    kind: conv.conversation_type,
                    id: conv.conversation_id,
                    name: conv.display_name,
                });
            }
        }
        // Saved Messages ("note to self"). Only once the channel exists
        // locally — the send path recognises it by that row's channel_type,
        // and without it the forward would route as a plain channel send.
        if let Some(user_id) = self.auth_state().get().current_user_id {
            let channel_id: crate::domain::ids::ChannelId = user_id.into();
            if !self.anon_mode_enabled() && self.get_channel_local(&channel_id).is_some() {
                out.push(ForwardDestination {
                    kind: CT::SavedMessages,
                    id: channel_id.to_string(),
                    name: "Saved messages".to_string(),
                });
            }
        }

        let needle = query.trim().to_lowercase();
        out.retain(|d| {
            !d.name.is_empty()
                && (needle.is_empty() || d.name.to_lowercase().contains(&needle))
                && ActiveConversation::from_routing_key(&d.kind, &d.id).is_some()
        });
        out.sort_by_key(|d| d.name.to_lowercase());
        out
    }

    /// Rows for the Friends tab, mode-exclusive like the DM sidebar: shield on
    /// lists only active anon-DM peers (and makes no identified server call);
    /// off lists accepted friends. Anon rows carry the anon DM id in
    /// `user_id` (see `FriendDataSendData`).
    ///
    /// Identified friends are local-first. [`Self::local_friend_tab_entries`]
    /// is what cold start paints; this call replaces those rows when
    /// `ListFriends` succeeds and returns them unchanged when it does not.
    /// An error used to publish an empty vec, so Contacts showed "No friends
    /// yet" while the chat list still rendered the same people from storage.
    pub async fn friends_tab_entries(&self) -> Vec<crate::ui::sidebar_data::FriendDataSendData> {
        if self.anon_mode_enabled() {
            return self.local_friend_tab_entries();
        }
        match self.list_friends_with_profiles().await {
            Ok(pairs) => {
                let mut out = Vec::with_capacity(pairs.len());
                for (friendship, _) in pairs {
                    // The server already leaves out its own blocks; this also
                    // drops a friend blocked through their anon DM.
                    if self.is_profile_user_blocked(&friendship.addressee_id) {
                        continue;
                    }
                    out.push(crate::ui::sidebar_data::FriendDataSendData {
                        user_id: friendship.addressee_id.to_string(),
                        // The resolver, not the raw row: a row written
                        // without the profile key holds `User <id8>`, and a
                        // known username reads better (REVIEW-R4 P0-3).
                        display_name: self.display_name(&friendship.addressee_id),
                        username: self.known_username(&friendship.addressee_id).unwrap_or_default(),
                        avatar_bytes: self.user_avatar(&friendship.addressee_id),
                        presence_status: self
                            .peer_presence(&friendship.addressee_id)
                            .as_str()
                            .to_string(),
                        custom_status: String::new(),
                        is_anon: false,
                    });
                }
                sort_by_name(&mut out);
                out
            }
            Err(e) => {
                tracing::warn!(error = %e, "Friends list: server unreachable, showing the local store");
                self.local_friend_tab_entries()
            }
        }
    }

    /// Friends-tab rows from local storage. No network.
    ///
    /// Shield on: active anon-DM peers. Shield off: accepted friends who are
    /// not blocked (`listed_friends`, either direction of the row). Presence
    /// uses the same privacy-shaped local presence as the DM sidebar. Live
    /// presence events update both views after this snapshot is published.
    pub fn local_friend_tab_entries(&self) -> Vec<crate::ui::sidebar_data::FriendDataSendData> {
        use crate::domain::anon_dm::AnonDmStatus;
        use crate::ui::sidebar_data::FriendDataSendData;

        if self.anon_mode_enabled() {
            let mut out = Vec::new();
            for dm in self.list_anon_dm_conversations() {
                if dm.status != AnonDmStatus::Active {
                    continue;
                }
                out.push(FriendDataSendData {
                    user_id: dm.id.to_string(),
                    display_name: self
                        .contact_name(&dm.peer_id)
                        .or(dm.peer_display_name.clone())
                        .unwrap_or_else(|| self.fallback_display_name(&dm.peer_id)),
                    username: String::new(),
                    avatar_bytes: self.anon_dm_avatar(&dm),
                    presence_status: self.anon_peer_presence().as_str().to_string(),
                    custom_status: String::new(),
                    is_anon: true,
                });
            }
            sort_by_name(&mut out);
            return out;
        }

        let mut out = self
            .listed_friends()
            .into_iter()
            .map(|peer| {
                let avatar_bytes = self.user_avatar(&peer);
                FriendDataSendData {
                    user_id: peer.to_string(),
                    display_name: self.display_name(&peer),
                    username: self.known_username(&peer).unwrap_or_default(),
                    avatar_bytes,
                    presence_status: self.peer_presence(&peer).as_str().to_string(),
                    custom_status: String::new(),
                    is_anon: false,
                }
            })
            .collect::<Vec<_>>();
        sort_by_name(&mut out);
        out
    }
}

/// Friends-tab order: by name, case-insensitively, as Signal lists contacts.
fn sort_by_name(rows: &mut [crate::ui::sidebar_data::FriendDataSendData]) {
    rows.sort_by_cached_key(|f| f.display_name.to_lowercase());
}

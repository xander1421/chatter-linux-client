use super::*;
use crate::{SharedContentItem, SharedTab};
use slint::{Model, ModelRc, VecModel};
use std::cell::RefCell;

thread_local! {
    /// Paging cursor for the profile's shared-content drawer. UI-thread only —
    /// every reader and writer runs inside the Slint event loop.
    pub(super) static SHARED_PAGE: RefCell<SharedPage> = RefCell::new(SharedPage::default());
}

/// How many items one page of a shared-content tab pulls. Big enough that a
/// 3-column grid fills several screens, small enough that decoding that many
/// thumbnails does not stall a frame.
const SHARED_PAGE_SIZE: u32 = 60;

/// Where the open profile's shared-content list has got to.
///
/// `exhausted` is what stops the Flickable's scroll callback from asking for
/// page after page at the bottom of a finished list, and `in_flight` stops it
/// from asking again while the previous request is still running.
#[derive(Default)]
pub(super) struct SharedPage {
    pub(super) peer: Option<UserId>,
    pub(super) conversation: Option<chatter_core::state::ActiveConversation>,
    pub(super) generation: u64,
    pub(super) tab: String,
    pub(super) loaded: usize,
    pub(super) exhausted: bool,
    pub(super) in_flight: bool,
}

/// The conversation whose shared content a contact's profile shows: the DM
/// with them. Same key `routing_key()` produces, so it matches the rows the
/// index stored.
pub(super) fn dm_conversation(peer_id: &UserId) -> chatter_core::state::ActiveConversation {
    chatter_core::state::ActiveConversation::ServerDm { peer_id: *peer_id }
}

/// Group/channel avatar profiles show that sender's contributions only.
/// DMs retain their existing two-participant shared history.
pub(super) fn shared_sender(
    conversation: chatter_core::state::ActiveConversation,
    peer: UserId,
) -> Option<UserId> {
    use chatter_core::state::ActiveConversation;
    match conversation {
        ActiveConversation::Channel(_) | ActiveConversation::AnonGroup(_) => Some(peer),
        _ => None,
    }
}

/// Load the tab strip, then open whichever tab comes first.
///
/// Core decides which tabs exist and in what order — an empty strip means
/// nothing has ever been shared, and the UI renders the empty state.
pub(super) fn load_shared_tabs(
    weak: slint::Weak<MainWindow>,
    ui_ctx: Arc<ChatterClient>,
    peer_id: UserId,
    conversation: chatter_core::state::ActiveConversation,
) {
    let generation = SHARED_PAGE.with(|p| p.borrow().generation);
    tokio::spawn(async move {
        let tabs = ui_ctx
            .shared_content_tabs_scoped(conversation, shared_sender(conversation, peer_id))
            .await;
        let first = tabs.first().map(|t| t.id.clone());
        let rows: Vec<SharedTab> = tabs
            .into_iter()
            .map(|t| {
                let label = crate::bin_ui::converters::shared_tab_label(&t.id);
                SharedTab {
                    id: t.id.into(),
                    label: label.into(),
                    count: t.count,
                }
            })
            .collect();
        let _ = slint::invoke_from_event_loop(move || {
            let Some(w) = weak.upgrade() else { return };
            // A different profile may have been opened while this was in
            // flight — do not paint its tabs over the current one.
            if SHARED_PAGE.with(|p| {
                (
                    p.borrow().peer,
                    p.borrow().conversation,
                    p.borrow().generation,
                )
            }) != (Some(peer_id), Some(conversation), generation)
            {
                return;
            }
            w.global::<crate::ModalHostState>()
                .set_profile_shared_tabs(ModelRc::new(VecModel::from(rows)));
            match first {
                Some(tab) => {
                    w.global::<crate::ModalHostState>()
                        .set_profile_shared_tab(tab.clone().into());
                    // `request_shared_page` owns the loading flag from here.
                    request_shared_page(&w, &ui_ctx, peer_id, tab, false);
                }
                // Nothing shared at all — clear the flag ourselves, or the
                // spinner outlives the query that would have cleared it.
                None => w
                    .global::<crate::ModalHostState>()
                    .set_profile_shared_loading(false),
            }
        });
    });
}

/// Fetch one page of a tab and paint it. `append` continues the open tab;
/// otherwise the list is replaced (a tab switch).
///
/// Must be called on the UI thread — it owns the paging cursor.
fn request_shared_page(
    w: &MainWindow,
    ui_ctx: &Arc<ChatterClient>,
    peer_id: UserId,
    tab: String,
    append: bool,
) {
    let Some(conversation) = SHARED_PAGE.with(|p| p.borrow().conversation) else {
        return;
    };
    let offset = SHARED_PAGE.with(|p| {
        let mut p = p.borrow_mut();
        if append && (p.exhausted || p.in_flight || p.tab != tab) {
            return None;
        }
        if !append {
            p.generation = p.generation.wrapping_add(1);
            p.tab = tab.clone();
            p.loaded = 0;
            p.exhausted = false;
        }
        p.in_flight = true;
        Some((p.loaded as u32, p.generation))
    });
    let Some((offset, generation)) = offset else {
        return;
    };

    w.global::<crate::ModalHostState>()
        .set_profile_shared_loading(true);
    let weak = w.as_weak();
    let ui_ctx = ui_ctx.clone();
    let revision = ui_ctx.auth_snapshot().session_revision;
    tokio::spawn(async move {
        let page = ui_ctx
            .shared_content_page_scoped(
                conversation,
                &tab,
                SHARED_PAGE_SIZE,
                offset,
                shared_sender(conversation, peer_id),
            )
            .await;
        #[cfg(target_os = "android")]
        let page = {
            let client = ui_ctx.clone();
            tokio::task::spawn_blocking(move || {
                let mut page = page;
                for (index, id) in
                    ChatterClient::shared_content_video_thumbnail_candidates(&page, 4)
                {
                    if let Ok(result) = client.decrypt_and_extract_video_thumbnail(&id) {
                        page[index].thumbnail = result.image_bytes;
                    }
                }
                page
            })
            .await
            .unwrap_or_default()
        };
        let short = page.len() < SHARED_PAGE_SIZE as usize;
        let count = page.len();
        let _ = slint::invoke_from_event_loop(move || {
            if ui_ctx.auth_snapshot().session_revision != revision {
                return;
            }
            let Some(w) = weak.upgrade() else { return };
            let current = SHARED_PAGE.with(|p| {
                let mut p = p.borrow_mut();
                if p.peer != Some(peer_id)
                    || p.conversation != Some(conversation)
                    || p.tab != tab
                    || p.generation != generation
                {
                    return false;
                }
                p.in_flight = false;
                p.loaded += count;
                p.exhausted = short;
                true
            });
            if !current {
                return;
            }
            if SHARED_PAGE.with(|p| {
                (
                    p.borrow().peer,
                    p.borrow().conversation,
                    p.borrow().generation,
                )
            }) != (Some(peer_id), Some(conversation), generation)
            {
                return;
            }
            w.global::<crate::ModalHostState>()
                .set_profile_shared_loading(false);

            // Thumbnails decode here, on the UI thread, because a
            // `slint::Image` cannot cross threads.
            let mut rows: Vec<SharedContentItem> = if append {
                let cur = w
                    .global::<crate::ModalHostState>()
                    .get_profile_shared_items();
                cur.iter().collect()
            } else {
                Vec::new()
            };
            rows.extend(page.iter().map(|item| {
                let (title, subtitle, is_video) =
                    crate::bin_ui::converters::shared_content_lines(item);
                let thumbnail = crate::bin_ui::converters::avatar_bytes_to_image(&item.thumbnail)
                    .unwrap_or_default();
                let has_thumbnail = thumbnail.size().width > 0;
                SharedContentItem {
                    message_id: item.message_id.clone().into(),
                    attachment_id: item.attachment_id.clone().into(),
                    category: item.category.clone().into(),
                    title: shared_content_display_title(&item.category, &title).into(),
                    subtitle: subtitle.into(),
                    thumbnail,
                    has_thumbnail,
                    is_video,
                }
            }));
            w.global::<crate::ModalHostState>()
                .set_profile_shared_items(ModelRc::new(VecModel::from(rows)));
        });
    });
}

/// Only the painted profile title changes; indexed filenames and jump IDs stay intact.
fn shared_content_display_title<'a>(category: &str, title: &'a str) -> &'a str {
    if category == "voice" {
        // Recorded labels have appeared with spaces and underscores. Reuse the
        // attachment predicate after normalizing those separators for display only.
        let normalized = title
            .split(|c: char| c.is_whitespace() || c == '_')
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        if chatter_core::domain::attachment::is_generic_voice_note_filename(&normalized) {
            return "Voice message";
        }
    }
    title
}

/// Wire the shared-content drawer: tab switches, paging, and opening an item.
pub(super) fn wire_shared_content(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ui_ctx_tab = ui_ctx.clone();
    window
        .global::<crate::ModalHostState>()
        .on_profile_shared_tab_selected(move |tab| {
            let Some(w) = weak_window.upgrade() else {
                return;
            };
            let Some(peer) = SHARED_PAGE.with(|p| p.borrow().peer) else {
                return;
            };
            w.global::<crate::ModalHostState>()
                .set_profile_shared_items(ModelRc::new(VecModel::<SharedContentItem>::from(
                    vec![],
                )));
            request_shared_page(&w, &ui_ctx_tab, peer, tab.to_string(), false);
        });

    let weak_window = window.as_weak();
    let ui_ctx_more = ui_ctx.clone();
    window
        .global::<crate::ModalHostState>()
        .on_profile_shared_load_more(move || {
            let Some(w) = weak_window.upgrade() else {
                return;
            };
            let Some(peer) = SHARED_PAGE.with(|p| p.borrow().peer) else {
                return;
            };
            let tab = w
                .global::<crate::ModalHostState>()
                .get_profile_shared_tab()
                .to_string();
            if tab.is_empty() {
                return;
            }
            request_shared_page(&w, &ui_ctx_more, peer, tab, true);
        });

    // Opening an item: a link goes to the browser, everything else jumps the
    // conversation to the message it arrived in — which is the existing
    // quote-click path, including its "not loaded yet, fetch older history"
    // retry.
    let weak_window = window.as_weak();
    let ui_ctx_jump = ui_ctx.clone();
    window
        .global::<crate::ModalHostState>()
        .on_profile_open_shared_item(move |message_id, attachment_id, category, selected_url| {
            let Some(w) = weak_window.upgrade() else {
                return;
            };
            if category.as_str() == "link" {
                // Preserve the clicked row identity, including two URLs in one
                // message. The message-link callback owns validation and the
                // platform opener (Android intent or desktop browser).
                open_shared_link(
                    w.global::<crate::ModalHostState>()
                        .get_profile_shared_items()
                        .iter(),
                    message_id.as_str(),
                    attachment_id.as_str(),
                    selected_url.as_str(),
                    |url| {
                        w.global::<crate::ChatPane>()
                            .invoke_open_message_link(url.into())
                    },
                );
                return;
            }
            let Some(conversation) = SHARED_PAGE.with(|p| p.borrow().conversation) else {
                return;
            };
            crate::bin_ui::features::back::reset_to_chats(&w);
            crate::bin_ui::features::header::jump_to_conversation_message(
                &w,
                &ui_ctx_jump,
                conversation,
                &message_id,
            );
        });
}

/// Push a username-link URL + its QR image into the profile-modal properties.
/// Must be called on the UI thread (creates a `slint::Image`).

#[cfg(test)]
mod shared_content_title_tests {
    use super::shared_content_display_title;

    #[test]
    fn recorded_profile_voice_titles_hide_generated_extensions() {
        for filename in [
            "Voice message.wav",
            "VOICE MESSAGE.WAV",
            "voice_message.wav",
            "  Voice   message.wav  ",
            "Voice__message.wav",
        ] {
            assert_eq!(
                shared_content_display_title("voice", filename),
                "Voice message"
            );
        }
    }

    #[test]
    fn authored_audio_and_non_voice_titles_remain_exact() {
        for filename in [
            "interview.wav",
            " My recording.wav ",
            "Voice message remix.wav",
        ] {
            assert_eq!(shared_content_display_title("voice", filename), filename);
        }
        for category in ["music", "file", "media", "link"] {
            assert_eq!(
                shared_content_display_title(category, "Voice message.wav"),
                "Voice message.wav"
            );
        }
    }
}

/// Resolve a click against the currently painted rows before delegating to the
/// existing safe message-link action. Injecting the action keeps regressions
/// independent of real browsers and platform intents.
fn open_shared_link(
    items: impl IntoIterator<Item = SharedContentItem>,
    message_id: &str,
    attachment_id: &str,
    selected_url: &str,
    open: impl FnOnce(&str),
) -> bool {
    let selected = items.into_iter().find(|item| {
        item.category.as_str() == "link"
            && item.message_id.as_str() == message_id
            && item.attachment_id.as_str() == attachment_id
            && item.subtitle.as_str() == selected_url
    });
    if let Some(item) = selected {
        open(item.subtitle.as_str());
        true
    } else {
        false
    }
}

#[cfg(test)]
mod shared_link_tests {
    use super::*;

    #[test]
    fn selected_second_url_uses_injected_message_link_opener() {
        let rows = [
            "https://example.invalid/first",
            "https://example.invalid/second",
        ]
        .map(|url| SharedContentItem {
            message_id: "same-message".into(),
            attachment_id: "".into(),
            category: "link".into(),
            subtitle: url.into(),
            ..Default::default()
        });
        let mut opened = String::new();
        assert!(open_shared_link(
            rows.clone(),
            "same-message",
            "",
            rows[1].subtitle.as_str(),
            |url| opened = url.to_owned()
        ));
        assert_eq!(opened, "https://example.invalid/second");
        assert!(!open_shared_link(
            rows,
            "same-message",
            "",
            "https://example.invalid/stale",
            |_| panic!("stale row must not invoke opener")
        ));
    }
}

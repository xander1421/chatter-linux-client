//! Unified search ordering rules.
//!
//! The unified search palette merges conversation navigation ("Jump to") with
//! message-content search ("Messages"). The conversation side reuses the quick
//! switcher entry type and its ordering rule; this module owns the query filter
//! so the UI layer stays a thin presenter. It also owns the shortcut-preference
//! value type so parsing/serialization lives in one place.

use crate::services::quick_switcher::QuickSwitcherEntry;
use chrono::TimeZone;
use chrono::{DateTime, Utc};

/// The searchable local content classes. Attachment metadata determines the
/// class; callers must only supply documents visible in the current mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchCategory {
    All,
    Chats,
    Messages,
    Media,
    Files,
    Links,
    Voice,
}

impl SearchCategory {
    pub fn from_str(value: &str) -> Self {
        match value {
            "chats" => Self::Chats,
            "messages" => Self::Messages,
            "media" => Self::Media,
            "files" => Self::Files,
            "links" => Self::Links,
            "voice" => Self::Voice,
            _ => Self::All,
        }
    }

    pub fn from_message(mime: Option<&str>, has_waveform: bool, text: Option<&str>) -> Self {
        if let Some(mime) = mime {
            if mime.starts_with("image/") || mime.starts_with("video/") {
                return Self::Media;
            }
            if mime.starts_with("audio/") && has_waveform {
                return Self::Voice;
            }
            return Self::Files;
        }
        if text.is_some_and(|text| text.contains("https://") || text.contains("http://")) {
            Self::Links
        } else {
            Self::Messages
        }
    }

    pub fn from_shared_kind(kind: &str) -> Self {
        match kind {
            "media" | "gif" => Self::Media,
            "voice" => Self::Voice,
            "link" => Self::Links,
            _ => Self::Files,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchSort {
    Relevance,
    Newest,
    Oldest,
    Largest,
}

impl SearchSort {
    pub fn from_str(value: &str) -> Self {
        match value {
            "newest" => Self::Newest,
            "oldest" => Self::Oldest,
            "largest" => Self::Largest,
            _ => Self::Relevance,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SearchQuery {
    pub text: String,
    pub category: SearchCategory,
    pub sort: SearchSort,
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
    pub from: Option<String>,
    pub in_chat: Option<String>,
}

impl SearchQuery {
    pub fn from_controls(text: &str, category: &str, sort: &str, date: &str) -> Self {
        let now = Utc::now();
        let local_midnight = |day: chrono::NaiveDate| {
            day.and_hms_opt(0, 0, 0)
                .and_then(|time| chrono::Local.from_local_datetime(&time).earliest())
                .map(|time| time.with_timezone(&Utc))
        };
        let mut until = None;
        let since = match date {
            "today" => local_midnight(chrono::Local::now().date_naive()),
            "week" => Some(now - chrono::Duration::days(7)),
            "month" => Some(now - chrono::Duration::days(30)),
            custom if custom.starts_with("custom:") => {
                let (start, end) = custom
                    .trim_start_matches("custom:")
                    .split_once("..")
                    .unwrap_or(("", ""));
                until = chrono::NaiveDate::parse_from_str(end, "%Y-%m-%d")
                    .ok()
                    .and_then(|day| day.succ_opt())
                    .and_then(local_midnight);
                chrono::NaiveDate::parse_from_str(start, "%Y-%m-%d")
                    .ok()
                    .and_then(local_midnight)
            }
            _ => None,
        };
        Self {
            text: text.to_string(),
            category: SearchCategory::from_str(category),
            sort: SearchSort::from_str(sort),
            since,
            until,
            from: None,
            in_chat: None,
        }
    }

    /// Run over already visible, locally decrypted candidates. This method is
    /// deliberately unaware of transport and cannot trigger network lookup.
    pub fn apply<T: SearchDocument>(&self, documents: impl IntoIterator<Item = T>) -> Vec<T> {
        let needle = self.text.trim().to_lowercase();
        let mut out: Vec<T> = documents
            .into_iter()
            .filter(|doc| {
                (self.category == SearchCategory::All || self.category == doc.category())
                    && (needle.is_empty() || doc.search_text().to_lowercase().contains(&needle))
                    && self.since.is_none_or(|date| doc.created_at() >= date)
                    && self.until.is_none_or(|date| doc.created_at() < date)
                    && self
                        .from
                        .as_ref()
                        .is_none_or(|id| doc.sender_id() == Some(id.as_str()))
                    && self.in_chat.as_ref().is_none_or(|id| doc.chat_id() == id)
            })
            .collect();
        match self.sort {
            SearchSort::Relevance => {} // Preserve the local FTS index's ranking.
            SearchSort::Newest => out.sort_by_key(|doc| std::cmp::Reverse(doc.created_at())),
            SearchSort::Oldest => out.sort_by_key(|doc| doc.created_at()),
            SearchSort::Largest => out.sort_by_key(|doc| std::cmp::Reverse(doc.size_bytes())),
        }
        out
    }
}

pub trait SearchDocument {
    fn category(&self) -> SearchCategory;
    fn search_text(&self) -> &str;
    fn created_at(&self) -> DateTime<Utc>;
    fn size_bytes(&self) -> u64;
    fn sender_id(&self) -> Option<&str>;
    fn chat_id(&self) -> &str;
}

/// Filter conversation entries by a case-insensitive name substring, then
/// order unread-first and alphabetically. Global search must not inherit the
/// old quick-switcher's twenty-result navigation cap.
///
/// An empty/whitespace query returns every entry, arranged (the palette's
/// "recent conversations" empty state).
pub fn filter_conversations(
    query: &str,
    entries: Vec<QuickSwitcherEntry>,
) -> Vec<QuickSwitcherEntry> {
    let needle = query.trim().to_lowercase();
    let mut filtered = if needle.is_empty() {
        entries
    } else {
        entries
            .into_iter()
            .filter(|e| e.name.to_lowercase().contains(&needle))
            .collect()
    };
    filtered.sort_by(|a, b| {
        b.unread_count
            .cmp(&a.unread_count)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    filtered
}

/// Which keyboard combo opens the unified search bar. User-selectable; persisted
/// as a short string in `SettingsRepository` under `keys::SEARCH_SHORTCUT`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchShortcut {
    CtrlK,
    CtrlF,
    Both,
}

impl Default for SearchShortcut {
    fn default() -> Self {
        SearchShortcut::Both
    }
}

impl SearchShortcut {
    /// Parse the persisted value. Unknown/missing → [`SearchShortcut::Both`].
    pub fn from_str(s: &str) -> Self {
        match s {
            "k" => SearchShortcut::CtrlK,
            "f" => SearchShortcut::CtrlF,
            _ => SearchShortcut::Both,
        }
    }

    /// The persisted string form.
    pub fn as_str(self) -> &'static str {
        match self {
            SearchShortcut::CtrlK => "k",
            SearchShortcut::CtrlF => "f",
            SearchShortcut::Both => "both",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[derive(Clone)]
    struct Doc {
        text: &'static str,
        category: SearchCategory,
        time: i64,
        size: u64,
        sender: &'static str,
        chat: &'static str,
    }
    impl SearchDocument for Doc {
        fn category(&self) -> SearchCategory {
            self.category
        }
        fn search_text(&self) -> &str {
            self.text
        }
        fn created_at(&self) -> DateTime<Utc> {
            Utc.timestamp_opt(self.time, 0).unwrap()
        }
        fn size_bytes(&self) -> u64 {
            self.size
        }
        fn sender_id(&self) -> Option<&str> {
            Some(self.sender)
        }
        fn chat_id(&self) -> &str {
            self.chat
        }
    }

    #[test]
    fn query_filters_category_date_sender_chat_and_sorts_size() {
        let docs = vec![
            Doc {
                text: "holiday.jpg",
                category: SearchCategory::Media,
                time: 100,
                size: 20,
                sender: "alice",
                chat: "trip",
            },
            Doc {
                text: "holiday.mov",
                category: SearchCategory::Media,
                time: 200,
                size: 200,
                sender: "alice",
                chat: "trip",
            },
            Doc {
                text: "holiday.pdf",
                category: SearchCategory::Files,
                time: 150,
                size: 300,
                sender: "alice",
                chat: "trip",
            },
            Doc {
                text: "holiday.png",
                category: SearchCategory::Media,
                time: 250,
                size: 400,
                sender: "bob",
                chat: "trip",
            },
        ];
        let query = SearchQuery {
            text: "HOLIDAY".into(),
            category: SearchCategory::Media,
            sort: SearchSort::Largest,
            since: Some(Utc.timestamp_opt(99, 0).unwrap()),
            until: Some(Utc.timestamp_opt(220, 0).unwrap()),
            from: Some("alice".into()),
            in_chat: Some("trip".into()),
        };
        let out = query.apply(docs);
        assert_eq!(
            out.iter().map(|doc| doc.text).collect::<Vec<_>>(),
            ["holiday.mov", "holiday.jpg"]
        );
    }

    #[test]
    fn chronological_order_is_stable_for_equal_timestamps() {
        let docs = vec![
            Doc {
                text: "first",
                category: SearchCategory::Messages,
                time: 10,
                size: 0,
                sender: "a",
                chat: "c",
            },
            Doc {
                text: "second",
                category: SearchCategory::Messages,
                time: 20,
                size: 0,
                sender: "a",
                chat: "c",
            },
            Doc {
                text: "third",
                category: SearchCategory::Messages,
                time: 20,
                size: 0,
                sender: "a",
                chat: "c",
            },
        ];
        let mut query = SearchQuery {
            text: String::new(),
            category: SearchCategory::All,
            sort: SearchSort::Newest,
            since: None,
            until: None,
            from: None,
            in_chat: None,
        };
        assert_eq!(
            query
                .apply(docs.clone())
                .iter()
                .map(|d| d.text)
                .collect::<Vec<_>>(),
            ["second", "third", "first"]
        );
        query.sort = SearchSort::Oldest;
        assert_eq!(
            query.apply(docs).iter().map(|d| d.text).collect::<Vec<_>>(),
            ["first", "second", "third"]
        );
    }

    #[test]
    fn controls_parse_local_custom_range_and_attachment_kinds() {
        let query = SearchQuery::from_controls(
            "report",
            "files",
            "largest",
            "custom:2026-10-01..2026-10-03",
        );
        assert_eq!(query.category, SearchCategory::Files);
        assert_eq!(query.sort, SearchSort::Largest);
        assert!(query.until.unwrap() > query.since.unwrap());
        assert_eq!(
            SearchCategory::from_message(Some("image/jpeg"), false, None),
            SearchCategory::Media
        );
        assert_eq!(
            SearchCategory::from_message(Some("audio/ogg"), true, None),
            SearchCategory::Voice
        );
        assert_eq!(
            SearchCategory::from_message(None, false, Some("see https://example.com")),
            SearchCategory::Links
        );
        assert_eq!(
            SearchCategory::from_shared_kind("file"),
            SearchCategory::Files
        );
    }

    fn entry(name: &str, unread: i32) -> QuickSwitcherEntry {
        QuickSwitcherEntry {
            id: name.to_string(),
            name: name.to_string(),
            conv_type: "channel".into(),
            presence_status: String::new(),
            unread_count: unread,
        }
    }

    #[test]
    fn empty_query_returns_all_arranged() {
        let out = filter_conversations("  ", vec![entry("Zeta", 0), entry("Beta", 5)]);
        let names: Vec<&str> = out.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["Beta", "Zeta"]); // unread-first
    }

    #[test]
    fn filters_by_case_insensitive_substring() {
        let out = filter_conversations("ET", vec![entry("Zeta", 0), entry("Alpha", 0)]);
        let names: Vec<&str> = out.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["Zeta"]);
    }

    #[test]
    fn conversation_search_keeps_matches_beyond_old_palette_cap() {
        let entries = (0..30)
            .map(|index| entry(&format!("chat {index:02}"), 0))
            .collect();
        assert_eq!(filter_conversations("chat", entries).len(), 30);
    }

    #[test]
    fn shortcut_roundtrip_and_default() {
        assert_eq!(SearchShortcut::from_str("k"), SearchShortcut::CtrlK);
        assert_eq!(SearchShortcut::from_str("f"), SearchShortcut::CtrlF);
        assert_eq!(SearchShortcut::from_str("both"), SearchShortcut::Both);
        assert_eq!(SearchShortcut::from_str("garbage"), SearchShortcut::Both);
        assert_eq!(SearchShortcut::default().as_str(), "both");
    }
}

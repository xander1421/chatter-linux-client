//! Unified search ordering rules.
//!
//! The unified search palette merges conversation navigation ("Jump to") with
//! message-content search ("Messages"). The conversation side reuses the quick
//! switcher entry type and its ordering rule; this module owns the query filter
//! so the UI layer stays a thin presenter. It also owns the shortcut-preference
//! value type so parsing/serialization lives in one place.

use crate::services::quick_switcher::{QuickSwitcherEntry, arrange_entries};

/// Filter conversation entries by a case-insensitive name substring, then apply
/// the canonical quick-switcher ordering (unread-first, alphabetical, capped).
///
/// An empty/whitespace query returns every entry, arranged (the palette's
/// "recent conversations" empty state).
pub fn filter_conversations(
    query: &str,
    entries: Vec<QuickSwitcherEntry>,
) -> Vec<QuickSwitcherEntry> {
    let needle = query.trim().to_lowercase();
    let filtered = if needle.is_empty() {
        entries
    } else {
        entries
            .into_iter()
            .filter(|e| e.name.to_lowercase().contains(&needle))
            .collect()
    };
    arrange_entries(filtered)
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
    fn shortcut_roundtrip_and_default() {
        assert_eq!(SearchShortcut::from_str("k"), SearchShortcut::CtrlK);
        assert_eq!(SearchShortcut::from_str("f"), SearchShortcut::CtrlF);
        assert_eq!(SearchShortcut::from_str("both"), SearchShortcut::Both);
        assert_eq!(SearchShortcut::from_str("garbage"), SearchShortcut::Both);
        assert_eq!(SearchShortcut::default().as_str(), "both");
    }
}

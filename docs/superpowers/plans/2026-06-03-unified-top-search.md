# Unified Top Search (Change A) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the sidebar search field, `QuickSwitcher` (Ctrl+K) and `SearchModal` (Ctrl+F) with one Slack-style search bar at the top of the chat column that opens a single dropdown palette merging conversation navigation ("Jump to") and message-content search ("Messages"), behind a user-selectable shortcut.

**Architecture:** Conversation filtering/ordering lives in `chatter-core` (`services::search`, reusing `quick_switcher::arrange_entries`); message search already lives in core (`search_messages_local`). A new `ui/components/unified_search.slint` renders the bar + palette; a new `features/unified_search.rs` wiring module subsumes the old `quick_switcher.rs` and the search half of `header.rs`. The shortcut choice persists via the existing `SettingsRepository`.

**Tech Stack:** Rust, Slint, `chatter-core` SDK, `SettingsRepository` (SQLite-backed key/value).

**Scope:** Change A only. Change B (fold Join Channel into the Create modal as a tab) is a separate plan.

---

## File Structure

**chatter-core (Rust, unit-testable):**
- Create `crates/chatter-core/src/services/search.rs` — pure conversation filter+arrange and the `SearchShortcut` enum.
- Modify `crates/chatter-core/src/services/mod.rs` — register the new module.
- Modify `crates/chatter-core/src/storage/repositories/settings_repo.rs` — add `keys::SEARCH_SHORTCUT`.
- Modify `crates/chatter-core/src/client/mod.rs` — `get_search_shortcut()` / `set_search_shortcut()` client façade.

**Slint (UI, verified via `cargo run`):**
- Create `ui/components/unified_search.slint` — `SearchHeader` bar + dropdown palette (reuses row visuals from the old switcher/search modals).
- Modify `ui/app.slint` — import + mount the bar at top of chat column; add window properties/callbacks; rewrite the FocusScope shortcut block; remove the `QuickSwitcher`/`SearchModal` mounts and their properties.
- Modify `ui/sidebar/sidebar.slint` — remove the top "Search…" field (expanded + collapsed) and the `search-clicked()` callback.
- Modify `ui/settings/settings_screen.slint` — add a "Search shortcut" selector.
- Delete `ui/modals/quick_switcher.slint` and `ui/modals/search_modal.slint`.

**Rust wiring (UI glue, verified via `cargo run`):**
- Create `crates/chatter-ui/src/bin_ui/features/unified_search.rs`.
- Modify `crates/chatter-ui/src/bin_ui/features/mod.rs` — register `unified_search`, drop `quick_switcher`.
- Delete `crates/chatter-ui/src/bin_ui/features/quick_switcher.rs`.
- Modify `crates/chatter-ui/src/bin_ui/features/header.rs` — remove the search callbacks (`search_clicked`, `search_messages`, `search_result_clicked`, `search_toggle_scope`).
- Modify settings hydration + `features/modals/settings.rs` — load/save the shortcut pref.

---

## Task 1: Core — conversation filter + `SearchShortcut` enum

**Files:**
- Create: `crates/chatter-core/src/services/search.rs`
- Modify: `crates/chatter-core/src/services/mod.rs`
- Test: inline `#[cfg(test)]` in `search.rs`

- [ ] **Step 1: Write the failing tests**

Create `crates/chatter-core/src/services/search.rs`:

```rust
//! Unified search ordering rules.
//!
//! The unified search palette merges conversation navigation ("Jump to") with
//! message-content search ("Messages"). The conversation side reuses the quick
//! switcher entry type and its ordering rule; this module owns the query filter
//! so the UI layer stays a thin presenter. It also owns the shortcut-preference
//! value type so parsing/serialization lives in one place.

use crate::services::quick_switcher::{arrange_entries, QuickSwitcherEntry};

/// Filter conversation entries by a case-insensitive name substring, then apply
/// the canonical quick-switcher ordering (unread-first, alphabetical, capped).
///
/// An empty/whitespace query returns every entry, arranged (the palette's
/// "recent conversations" empty state).
pub fn filter_conversations(query: &str, entries: Vec<QuickSwitcherEntry>) -> Vec<QuickSwitcherEntry> {
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
```

Register the module — add to `crates/chatter-core/src/services/mod.rs` (alphabetical, next to `quick_switcher`):

```rust
pub mod search;
```

- [ ] **Step 2: Run tests to verify they fail (module not yet wired)**

Run: `cargo test -p chatter-core services::search`
Expected: FAIL — compile error until `pub mod search;` is added, then PASS once added. (If you added the `mod` line in Step 1, expect PASS; this step confirms the test names run.)

- [ ] **Step 3: Run tests to verify they pass**

Run: `cargo test -p chatter-core services::search`
Expected: PASS — `empty_query_returns_all_arranged`, `filters_by_case_insensitive_substring`, `shortcut_roundtrip_and_default`.

- [ ] **Step 4: Commit**

```bash
git add crates/chatter-core/src/services/search.rs crates/chatter-core/src/services/mod.rs
git commit -m "feat(core): unified-search conversation filter + SearchShortcut enum"
```

---

## Task 2: Core — persist the search-shortcut preference

**Files:**
- Modify: `crates/chatter-core/src/storage/repositories/settings_repo.rs` (`keys` module, ~line 75)
- Modify: `crates/chatter-core/src/client/mod.rs` (façade near other `set_*`/`get_*` UI prefs)

- [ ] **Step 1: Add the settings key**

In `crates/chatter-core/src/storage/repositories/settings_repo.rs`, inside `pub mod keys`, after `COMPACT_MODE` (~line 75):

```rust
    pub const SEARCH_SHORTCUT: &str = "ui.search_shortcut";
```

- [ ] **Step 2: Add the client façade**

In `crates/chatter-core/src/client/mod.rs`, near the other settings-backed UI pref methods, add:

```rust
    /// Read the user's search-shortcut preference (defaults to `Both`).
    pub fn get_search_shortcut(&self) -> crate::services::search::SearchShortcut {
        use crate::storage::repositories::settings_repo::keys;
        let raw = self
            .signal()
            .settings_repo
            .get_string(keys::SEARCH_SHORTCUT)
            .ok()
            .flatten()
            .unwrap_or_default();
        crate::services::search::SearchShortcut::from_str(&raw)
    }

    /// Persist the user's search-shortcut preference.
    pub fn set_search_shortcut(
        &self,
        shortcut: crate::services::search::SearchShortcut,
    ) -> Result<(), crate::storage::StorageError> {
        use crate::storage::repositories::settings_repo::keys;
        self.signal()
            .settings_repo
            .set_string(keys::SEARCH_SHORTCUT, shortcut.as_str())
    }
```

> NOTE: confirm the accessor used by neighbouring methods to reach `settings_repo`. `client/auth.rs:290` uses `self.signal().settings_repo` and `client/auth.rs:385` uses `self.ui.auth_service.settings_repo`. Match whichever the surrounding `client/mod.rs` methods already use; if `signal()` is not in scope there, use `self.ui.auth_service.settings_repo`. Also confirm `StorageError` is re-exported at `crate::storage::StorageError` (it is referenced from `settings_repo.rs`); if not, use the path `crate::storage::repositories::settings_repo::StorageError`.

- [ ] **Step 3: Verify it compiles**

Run: `cargo build -p chatter-core`
Expected: builds clean. (No unit test — this is a thin DB-backed façade; the parse logic is covered by Task 1.)

- [ ] **Step 4: Commit**

```bash
git add crates/chatter-core/src/storage/repositories/settings_repo.rs crates/chatter-core/src/client/mod.rs
git commit -m "feat(core): persist search-shortcut preference via SettingsRepository"
```

---

## Task 3: Slint — `UnifiedSearch` component (bar + palette)

**Files:**
- Create: `ui/components/unified_search.slint`

This component is created here but only rendered after Task 4 mounts it. Verification for this task is "the project still runs"; behavioural verification is in Task 4.

- [ ] **Step 1: Create the component**

Create `ui/components/unified_search.slint`. It reuses the `QuickSwitcherItem` and `SearchResult` structs (re-exported from `app.slint`), so import them from their defining modules:

```slint
// Unified search — Slack-style top-bar search + dropdown palette.
// Replaces QuickSwitcher (Ctrl+K) and SearchModal (Ctrl+F).

import { Colors } from "../theme/colors.slint";
import { Spacing } from "../theme/spacing.slint";
import { Typography } from "../theme/typography.slint";
import { Icons } from "../theme/icons.slint";
import { QuickSwitcherItem } from "../modals/quick_switcher.slint";
import { SearchResult } from "../modals/search_modal.slint";

// One "Jump to" conversation row.
component ConvRow inherits Rectangle {
    in property <string> item-id;
    in property <string> name;
    in property <string> conv-type;
    in property <int> unread-count;
    in property <bool> selected: false;
    callback clicked(string, string);

    height: 40px;
    border-radius: Spacing.radius-sm;
    background: root.selected ? Colors.neo-inset-bg : touch.has-hover ? Colors.surface-hover : transparent;

    touch := TouchArea {
        mouse-cursor: pointer;
        clicked => { root.clicked(root.item-id, root.conv-type); }
    }

    HorizontalLayout {
        padding-left: Spacing.md;
        padding-right: Spacing.md;
        spacing: Spacing.sm;
        Text {
            text: root.conv-type == "channel" ? "#" : root.conv-type == "private_group" ? "🔒" : root.conv-type == "anon_dm" ? "👤" : "@";
            color: root.selected ? Colors.primary : Colors.text-muted;
            font-size: Typography.sm;
            font-weight: Typography.semibold;
            width: 20px;
            horizontal-alignment: center;
            vertical-alignment: center;
        }
        Text {
            text: root.name;
            color: root.selected ? Colors.text-primary : Colors.text-secondary;
            font-size: Typography.sm;
            vertical-alignment: center;
            horizontal-stretch: 1;
            overflow: elide;
        }
        if root.unread-count > 0: Rectangle {
            width: 20px; height: 20px; border-radius: 10px;
            background: Colors.primary; y: (parent.height - self.height) / 2;
            Text {
                text: root.unread-count > 99 ? "99+" : root.unread-count;
                color: Colors.text-primary; font-size: Typography.xs;
                horizontal-alignment: center; vertical-alignment: center;
                width: 100%; height: 100%;
            }
        }
    }
}

// One "Messages" content row.
component MsgRow inherits Rectangle {
    in property <string> message-id;
    in property <string> channel-name;
    in property <string> sender-name;
    in property <string> content-preview;
    in property <string> timestamp;
    in property <bool> selected: false;
    callback clicked(string);

    height: 56px;
    border-radius: Spacing.radius-sm;
    background: root.selected ? Colors.neo-inset-bg : touch.has-hover ? Colors.surface-hover : transparent;

    touch := TouchArea {
        mouse-cursor: pointer;
        clicked => { root.clicked(root.message-id); }
    }

    VerticalLayout {
        padding-left: Spacing.md;
        padding-right: Spacing.md;
        padding-top: Spacing.xs;
        padding-bottom: Spacing.xs;
        spacing: 2px;
        HorizontalLayout {
            spacing: Spacing.sm;
            Text {
                text: root.sender-name;
                color: Colors.text-primary;
                font-size: Typography.xs;
                font-weight: Typography.semibold;
            }
            Text {
                text: root.channel-name;
                color: Colors.text-muted;
                font-size: Typography.xs;
                horizontal-stretch: 1;
                overflow: elide;
            }
            Text {
                text: root.timestamp;
                color: Colors.text-muted;
                font-size: Typography.xs;
            }
        }
        Text {
            text: root.content-preview;
            color: Colors.text-secondary;
            font-size: Typography.sm;
            overflow: elide;
        }
    }
}

// Top-bar search. Resting = pill; open = dropdown palette anchored beneath.
export component UnifiedSearch inherits Rectangle {
    in property <[QuickSwitcherItem]> conversations: [];
    in property <[SearchResult]> messages: [];
    in property <bool> searching: false;
    in-out property <string> query: "";
    in-out property <bool> open: false;
    in-out property <int> selected-index: 0;

    callback opened();
    callback query-changed(string);
    callback conversation-selected(string, string);
    callback message-selected(string);

    // Combined navigable count: conversations first, then messages.
    property <int> total-count: root.conversations.length + root.messages.length;

    height: 44px;

    // Activate the highlighted row (Enter / accepted).
    function activate-selected() {
        if root.selected-index < root.conversations.length {
            root.conversation-selected(
                root.conversations[root.selected-index].id,
                root.conversations[root.selected-index].conv-type);
        } else if root.selected-index < root.total-count {
            root.message-selected(
                root.messages[root.selected-index - root.conversations.length].message-id);
        }
    }

    // Resting bar (centered, max-width capped).
    bar := Rectangle {
        width: min(640px, parent.width - Spacing.lg * 2);
        height: 36px;
        x: (parent.width - self.width) / 2;
        y: (parent.height - self.height) / 2;
        border-radius: Spacing.radius-md;
        background: Colors.neo-inset-bg;
        border-width: 1px;
        border-color: input.has-focus ? Colors.glow-cyan.with-alpha(0.4) : Colors.neo-inset-shadow;

        HorizontalLayout {
            padding-left: Spacing.md;
            padding-right: Spacing.md;
            spacing: Spacing.sm;
            Image {
                source: Icons.search;
                width: 16px; height: 16px;
                colorize: Colors.text-muted;
                y: (parent.height - self.height) / 2;
            }
            input := TextInput {
                text <=> root.query;
                font-size: Typography.sm;
                color: Colors.text-primary;
                vertical-alignment: center;
                horizontal-stretch: 1;
                edited => {
                    root.selected-index = 0;
                    root.open = true;
                    root.query-changed(root.query);
                }
                accepted => { root.activate-selected(); }
            }
            if root.query == "" && !input.has-focus: Text {
                text: "Search…";
                color: Colors.text-muted;
                font-size: Typography.sm;
                vertical-alignment: center;
            }
        }

        // Open the palette on focus/click.
        open-touch := TouchArea {
            clicked => {
                root.open = true;
                input.focus();
                root.opened();
            }
        }
    }

    // Dropdown palette anchored directly under the bar.
    if root.open: Rectangle {
        x: bar.x;
        y: bar.y + bar.height + Spacing.xs;
        width: bar.width;
        height: min(palette.preferred-height, 420px);
        background: Colors.surface;
        border-radius: Spacing.radius-lg;
        border-width: 1px;
        border-color: Colors.neo-highlight-strong;
        drop-shadow-blur: Spacing.neo-blur-lg;
        drop-shadow-color: Colors.neo-shadow-strong;

        TouchArea {} // swallow clicks so the outside-catcher doesn't close it

        palette := VerticalLayout {
            padding: Spacing.sm;
            spacing: Spacing.xxs;

            if root.query == "": Text {
                text: "Recent conversations";
                color: Colors.text-muted;
                font-size: Typography.xs;
                horizontal-alignment: left;
            }

            Flickable {
                vertical-stretch: 1;
                VerticalLayout {
                    spacing: Spacing.xxs;

                    // Jump to (conversations)
                    if root.conversations.length > 0: Text {
                        text: "Jump to";
                        color: Colors.text-muted;
                        font-size: Typography.xs;
                    }
                    for c[i] in root.conversations: ConvRow {
                        item-id: c.id;
                        name: c.name;
                        conv-type: c.conv-type;
                        unread-count: c.unread-count;
                        selected: i == root.selected-index;
                        clicked(id, t) => { root.conversation-selected(id, t); }
                    }

                    // Messages (content)
                    if root.messages.length > 0: Text {
                        text: "Messages";
                        color: Colors.text-muted;
                        font-size: Typography.xs;
                    }
                    for m[j] in root.messages: MsgRow {
                        message-id: m.message-id;
                        channel-name: m.channel-name;
                        sender-name: m.sender-name;
                        content-preview: m.content-preview;
                        timestamp: m.timestamp;
                        selected: (root.conversations.length + j) == root.selected-index;
                        clicked(id) => { root.message-selected(id); }
                    }

                    if root.total-count == 0 && root.query != "" && !root.searching: Text {
                        text: "No results";
                        color: Colors.text-muted;
                        font-size: Typography.sm;
                        horizontal-alignment: center;
                    }
                }
            }
        }

        // Arrow / Escape navigation across the combined list.
        FocusScope {
            width: 0; height: 0;
            key-pressed(event) => {
                if event.text == Key.UpArrow {
                    if root.selected-index > 0 { root.selected-index -= 1; }
                    return accept;
                }
                if event.text == Key.DownArrow {
                    if root.selected-index < root.total-count - 1 { root.selected-index += 1; }
                    return accept;
                }
                if event.text == Key.Return {
                    root.activate-selected();
                    return accept;
                }
                if event.text == Key.Escape {
                    root.open = false;
                    return accept;
                }
                reject
            }
        }
    }
}
```

> NOTE: confirm `Icons.search` exists in `ui/theme/icons.slint` (the sidebar uses `Icons.search` at `sidebar.slint:557`, so it does). Confirm `Spacing.xxs` / `Spacing.neo-blur-lg` / `Colors.glow-cyan` exist — all are used in `quick_switcher.slint`, so they do.

- [ ] **Step 2: Verify the project still runs**

Run: `cargo run -p chatter-ui`
Expected: app launches as before (component compiles but is not yet mounted/visible). Close the window.

- [ ] **Step 3: Commit**

```bash
git add ui/components/unified_search.slint
git commit -m "feat(ui): UnifiedSearch slint component (bar + dropdown palette)"
```

---

## Task 4: Wire the bar — properties, mount, Rust glue, shortcut

This is the integration task: add window state, mount the bar, add the `unified_search.rs` glue, and rewrite the FocusScope. After this task the feature works end-to-end (old surfaces removed in Task 5).

**Files:**
- Modify: `ui/app.slint` (properties ~88-100 & ~501-506; import ~41; mount at chat column ~1196; FocusScope ~882-894)
- Create: `crates/chatter-ui/src/bin_ui/features/unified_search.rs`
- Modify: `crates/chatter-ui/src/bin_ui/features/mod.rs`

- [ ] **Step 1: Add window properties + callbacks (app.slint)**

Add the import near line 41:

```slint
import { UnifiedSearch } from "components/unified_search.slint";
```

Add properties in the root component (alongside the existing quick-switcher block ~line 88):

```slint
    // ========== Unified search (top bar) ==========
    in property <[QuickSwitcherItem]> unified-search-conversations: [];
    in property <[SearchResult]> unified-search-messages: [];
    in property <bool> unified-search-searching: false;
    in-out property <string> unified-search-query: "";
    in-out property <bool> unified-search-open: false;
    in-out property <int> unified-search-selected-index: 0;
    // Loaded from SettingsRepository at startup: "k" | "f" | "both".
    in-out property <string> search-shortcut-pref: "both";
    callback unified-search-opened();
    callback unified-search-changed(string);
    callback unified-search-conversation-selected(string, string);
    callback unified-search-message-selected(string);
```

- [ ] **Step 2: Mount the bar at the top of the chat column (app.slint ~1196)**

Inside the chat-column `VerticalLayout` (currently starts at ~1196 with `ActiveCallView` as its first child), insert the bar as the FIRST child, before `ActiveCallView`:

```slint
                    UnifiedSearch {
                        visible: !root.active-call-fullscreen;
                        conversations: root.unified-search-conversations;
                        messages: root.unified-search-messages;
                        searching: root.unified-search-searching;
                        query <=> root.unified-search-query;
                        open <=> root.unified-search-open;
                        selected-index <=> root.unified-search-selected-index;
                        opened => { root.unified-search-opened(); }
                        query-changed(q) => { root.unified-search-changed(q); }
                        conversation-selected(id, t) => {
                            root.unified-search-conversation-selected(id, t);
                            root.unified-search-open = false;
                        }
                        message-selected(id) => {
                            root.unified-search-message-selected(id);
                            root.unified-search-open = false;
                        }
                    }
```

- [ ] **Step 3: Rewrite the FocusScope shortcut block (app.slint ~882-894)**

Replace the Ctrl+K and Ctrl+F branches with a single pref-driven open:

```slint
            // Ctrl+K / Ctrl+F: open unified search (which combo depends on pref)
            if (event.modifiers.control
                && ((event.text == "k" && root.search-shortcut-pref != "f")
                 || (event.text == "f" && root.search-shortcut-pref != "k"))) {
                root.unified-search-query = "";
                root.unified-search-selected-index = 0;
                root.unified-search-open = true;
                root.unified-search-opened();
                return accept;
            }
```

(Delete the old `event.text == "k"` quick-switcher branch and the old `event.text == "f"` search branch.)

- [ ] **Step 4: Create the Rust wiring module**

Create `crates/chatter-ui/src/bin_ui/features/unified_search.rs`. It merges conversation collection (from the old `quick_switcher.rs`) with message search (from `header.rs`):

```rust
//! Unified search wiring — top-bar palette merging conversation navigation
//! ("Jump to") with message-content search ("Messages").
//!
//! Subsumes the former `quick_switcher.rs` and the search half of `header.rs`.

use std::sync::Arc;

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use chatter_core::client::ChatterClient;
use chatter_core::services::quick_switcher::QuickSwitcherEntry;
use chatter_core::services::search::filter_conversations;

use crate::{MainWindow, QuickSwitcherItem, SearchResult};

/// Wire all unified-search callbacks.
pub fn wire(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    wire_opened(window);
    wire_changed(window, ui_ctx);
    wire_conversation_selected(window);
    wire_message_selected(window, ui_ctx);
}

/// On open: populate conversations (empty query → recent), clear messages.
fn wire_opened(window: &MainWindow) {
    let weak = window.as_weak();
    window.on_unified_search_opened(move || {
        if let Some(w) = weak.upgrade() {
            let convs = filter_to_items("", &w);
            set_conversations(&w, convs);
            w.set_unified_search_messages(ModelRc::new(VecModel::default()));
        }
    });
}

/// On query change: filter conversations (sync, core) + search messages (bg).
fn wire_changed(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak = window.as_weak();
    let ui_ctx = ui_ctx.clone();
    window.on_unified_search_changed(move |query: SharedString| {
        let query_str = query.to_string();

        // Conversations: synchronous, core-ranked.
        if let Some(w) = weak.upgrade() {
            let convs = filter_to_items(&query_str, &w);
            set_conversations(&w, convs);
        }

        // Messages: empty query clears; otherwise search in background.
        if query_str.trim().is_empty() {
            if let Some(w) = weak.upgrade() {
                w.set_unified_search_messages(ModelRc::new(VecModel::default()));
                w.set_unified_search_searching(false);
            }
            return;
        }

        if let Some(w) = weak.upgrade() {
            w.set_unified_search_searching(true);
        }
        let weak_bg = weak.clone();
        let ui_ctx = ui_ctx.clone();
        std::thread::spawn(move || {
            let results = search_messages(&ui_ctx, &query_str);
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_bg.upgrade() {
                    w.set_unified_search_messages(ModelRc::new(VecModel::from(results)));
                    w.set_unified_search_searching(false);
                }
            });
        });
    });
}

/// On conversation pick: navigate (channel/dm/private group).
fn wire_conversation_selected(window: &MainWindow) {
    let weak = window.as_weak();
    window.on_unified_search_conversation_selected(move |id: SharedString, conv_type: SharedString| {
        if let Some(w) = weak.upgrade() {
            match conv_type.as_str() {
                "channel" => w.invoke_channel_selected(id),
                "dm" | "anon_dm" => w.invoke_dm_selected(id),
                "private_group" => w.invoke_private_group_selected(id),
                _ => {}
            }
        }
    });
}

/// On message pick: navigate to the message (port of header.rs search_result_clicked).
fn wire_message_selected(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak = window.as_weak();
    let ui_ctx = ui_ctx.clone();
    window.on_unified_search_message_selected(move |message_id: SharedString| {
        let message_id_str = message_id.to_string();
        let weak = weak.clone();
        let ui_ctx = ui_ctx.clone();
        if let Some(w) = weak.upgrade() {
            w.set_loading_messages(true);
        }
        tokio::spawn(async move {
            let msg_id = match message_id_str.parse::<chatter_core::domain::ids::MessageId>() {
                Ok(id) => id,
                Err(e) => { tracing::error!("Invalid message ID: {}", e); return; }
            };
            let message = match ui_ctx.get_message_local(&msg_id) {
                Some(m) => m,
                None => { tracing::error!("Message {} not found locally", msg_id); return; }
            };
            let conv_id_str = message.conversation_id.to_string();
            let conv_type = message.conversation_type;
            let weak_dispatch = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_dispatch.upgrade() {
                    let id_slint: SharedString = conv_id_str.into();
                    use chatter_core::domain::message::ConversationType as CT;
                    match conv_type {
                        CT::Channel => w.invoke_channel_selected(id_slint),
                        CT::ServerDm | CT::MailboxDm | CT::AnonDm => w.invoke_dm_selected(id_slint),
                        CT::AnonGroup => w.invoke_private_group_selected(id_slint),
                        CT::SavedMessages => w.set_loading_messages(false),
                    }
                }
            });
            // Scroll-to-message once the loader populates the model.
            let weak_scroll = weak.clone();
            let target_id = message_id_str.clone();
            tokio::spawn(async move {
                for _ in 0..20 {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    let target_id = target_id.clone();
                    let weak_inner = weak_scroll.clone();
                    let found = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                    let found_set = found.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak_inner.upgrade() {
                            let messages = w.get_messages();
                            for i in 0..messages.row_count() {
                                if let Some(m) = messages.row_data(i) {
                                    if m.message_id == target_id {
                                        w.set_scroll_to_message_index(i as i32);
                                        found_set.store(true, std::sync::atomic::Ordering::SeqCst);
                                        break;
                                    }
                                }
                            }
                        }
                    });
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    if found.load(std::sync::atomic::Ordering::SeqCst) { break; }
                }
            });
        });
    });
}

// ---- helpers ----

/// Collect conversations from sidebar models, filter+arrange via core.
fn filter_to_items(query: &str, window: &MainWindow) -> Vec<QuickSwitcherItem> {
    let mut entries: Vec<QuickSwitcherEntry> = Vec::new();

    let channels = window.get_channels();
    for i in 0..channels.row_count() {
        if let Some(ch) = channels.row_data(i) {
            entries.push(QuickSwitcherEntry {
                id: ch.channel_id.to_string(),
                name: ch.name.to_string(),
                conv_type: "channel".into(),
                presence_status: String::new(),
                unread_count: ch.unread_count,
            });
        }
    }
    let dms = window.get_dm_conversations();
    for i in 0..dms.row_count() {
        if let Some(dm) = dms.row_data(i) {
            entries.push(QuickSwitcherEntry {
                id: dm.channel_id.to_string(),
                name: dm.display_name.to_string(),
                conv_type: "dm".into(),
                presence_status: dm.presence_status.to_string(),
                unread_count: dm.unread_count,
            });
        }
    }
    let groups = window.get_private_groups();
    for i in 0..groups.row_count() {
        if let Some(g) = groups.row_data(i) {
            entries.push(QuickSwitcherEntry {
                id: g.group_id.to_string(),
                name: g.name.to_string(),
                conv_type: "private_group".into(),
                presence_status: String::new(),
                unread_count: g.unread_count,
            });
        }
    }

    filter_conversations(query, entries)
        .into_iter()
        .map(|e| QuickSwitcherItem {
            id: e.id.into(),
            name: e.name.into(),
            conv_type: e.conv_type.into(),
            presence_status: e.presence_status.into(),
            unread_count: e.unread_count,
        })
        .collect()
}

fn set_conversations(window: &MainWindow, items: Vec<QuickSwitcherItem>) {
    window.set_unified_search_conversations(ModelRc::new(VecModel::from(items)));
}

/// Port of header.rs message-search shaping. Searches all conversations
/// (unified palette has no per-channel scope toggle).
fn search_messages(ui_ctx: &Arc<ChatterClient>, query: &str) -> Vec<SearchResult> {
    let results = match ui_ctx.search_messages_local(query, None, 50) {
        Ok(r) => r,
        Err(e) => { tracing::error!("Search failed: {}", e); return Vec::new(); }
    };
    let mut out: Vec<SearchResult> = Vec::with_capacity(results.len());
    for result in &results {
        let msg = match ui_ctx.get_message_local(&result.message_id) {
            Some(m) => m,
            None => continue,
        };
        use chatter_core::domain::message::ConversationType as CT;
        let channel_name = match msg.conversation_type {
            CT::Channel | CT::ServerDm | CT::MailboxDm => ui_ctx
                .get_channel_local(&result.conversation_id)
                .map(|c| c.name)
                .unwrap_or_else(|| "Unknown".to_string()),
            CT::AnonDm => result
                .conversation_id
                .to_string()
                .parse::<chatter_core::domain::anon_dm::AnonDmId>()
                .ok()
                .and_then(|id| ui_ctx.get_anon_dm_conversation(&id))
                .and_then(|c| c.peer_display_name)
                .unwrap_or_else(|| "Private Chat".to_string()),
            CT::AnonGroup => result
                .conversation_id
                .to_string()
                .parse::<chatter_core::domain::anon_group::AnonGroupId>()
                .ok()
                .and_then(|id| ui_ctx.get_anon_group(&id))
                .map(|g| g.name)
                .unwrap_or_else(|| "Private Group".to_string()),
            CT::SavedMessages => "Saved Messages".to_string(),
        };
        let sender_name = msg
            .sender_id
            .as_ref()
            .map(|id| ui_ctx.display_name(id))
            .unwrap_or_else(|| "Unknown".to_string());
        let timestamp = msg
            .created_at
            .with_timezone(&chrono::Local)
            .format("%b %d, %H:%M")
            .to_string();
        let content_preview = msg
            .as_text()
            .map(|t| ui_ctx.truncate_message_preview(&t))
            .unwrap_or_else(|| "[Attachment]".to_string());
        out.push(SearchResult {
            message_id: result.message_id.to_string().into(),
            channel_id: result.conversation_id.to_string().into(),
            channel_name: channel_name.into(),
            sender_name: sender_name.into(),
            content_preview: content_preview.into(),
            timestamp: timestamp.into(),
            highlight_start: 0,
            highlight_end: 0,
        });
    }
    out
}
```

- [ ] **Step 5: Register the module (mod.rs)**

In `crates/chatter-ui/src/bin_ui/features/mod.rs`:
- Add `pub mod unified_search;` (after `pub mod thread;`).
- Add `unified_search::wire(window, ui_ctx);` in the `wire`-all function (after `quick_switcher::wire(...)` for now; that line is removed in Task 5).

- [ ] **Step 6: Verify end-to-end**

Run: `cargo run -p chatter-ui`
Expected, observe:
- A centered search bar at the top of the chat column.
- Pressing Ctrl+K (and Ctrl+F) opens the dropdown; empty query shows "Recent conversations" under "Jump to".
- Typing a conversation name narrows "Jump to"; typing a word present in messages populates "Messages".
- Arrow Up/Down moves the highlight across both sections; Enter opens the highlighted item; Escape closes.
- Clicking a conversation navigates; clicking a message opens its conversation and scrolls to it.

(The old sidebar search field and Ctrl+K modal may still be present — they are removed in Task 5.)

- [ ] **Step 7: Commit**

```bash
git add ui/app.slint crates/chatter-ui/src/bin_ui/features/unified_search.rs crates/chatter-ui/src/bin_ui/features/mod.rs
git commit -m "feat(ui): mount unified search bar + wire palette end-to-end"
```

---

## Task 5: Remove the old surfaces (clean break)

**Files:**
- Delete: `crates/chatter-ui/src/bin_ui/features/quick_switcher.rs`
- Delete: `ui/modals/quick_switcher.slint`, `ui/modals/search_modal.slint`
- Modify: `crates/chatter-ui/src/bin_ui/features/mod.rs`, `features/header.rs`, `ui/app.slint`, `ui/sidebar/sidebar.slint`

- [ ] **Step 1: Drop the old Rust wiring**

- Delete `crates/chatter-ui/src/bin_ui/features/quick_switcher.rs`.
- In `features/mod.rs`: remove `pub mod quick_switcher;` and the `quick_switcher::wire(window, ui_ctx);` call.
- In `features/header.rs`: remove `wire_search_clicked`, `wire_search_callbacks` (functions and their calls in `wire`). Keep `wire_pinned_navigation`, `wire_chat_settings_clicked`, `wire_invite_member`, `wire_members_panel`, `wire_load_more`, `wire_scroll_to_bottom`. Remove now-unused imports (`messages_to_display_batch` stays only if still used elsewhere in the file — it is not after removal, so drop it; keep `ActiveConversation`).

- [ ] **Step 2: Remove the Slint modals + properties**

- Delete `ui/modals/quick_switcher.slint` and `ui/modals/search_modal.slint`.
- In `ui/app.slint`:
  - Remove imports of `QuickSwitcher`/`QuickSwitcherItem` (line ~41) and `SearchModal`/`SearchResult` (line ~36). BUT `QuickSwitcherItem` and `SearchResult` are still used by `unified_search.slint` and the export list. **Re-home these two structs:** move the `QuickSwitcherItem` struct definition and the `SearchResult` struct definition into `ui/components/unified_search.slint` (export them there) and update `unified_search.slint` to define-not-import them; update the `app.slint` import to `import { UnifiedSearch, QuickSwitcherItem, SearchResult } from "components/unified_search.slint";`.
  - Remove the `quick-switcher-*` properties/callbacks (~88-94) and the message-`search-*` properties/callbacks (`search-query`, `search-results`, `search-searching`, `search-result-count`, `search-all-channels` ~501-506; `search-clicked` ~665; `search-messages`/`search-result-clicked`/`search-toggle-scope` ~777-779).
  - Remove the `QuickSwitcher { ... }` mount (~1840) and the `SearchModal { ... }` mount (~1817).
  - Keep `SearchResult`/`QuickSwitcherItem` in the top-level `export { ... }` list (now sourced from `unified_search.slint`).
  - Update the keyboard-shortcuts help dialog (~2025): change `{ key: "Ctrl + F", action: "Search messages" }` to `{ key: "Ctrl + K / Ctrl + F", action: "Search" }`.

- [ ] **Step 3: Remove the sidebar search field**

In `ui/sidebar/sidebar.slint`:
- Remove the expanded search bar block (~140-181) and the collapsed search icon block (~182-215).
- Remove the `search-clicked()` callback declaration (~76) and any internal references.

- [ ] **Step 4: Verify nothing references removed symbols**

Run: `cargo run -p chatter-ui`
Expected: compiles and runs. If the compiler flags a leftover `on_search_clicked` / `set_search_results` / `on_quick_switcher_*` reference, delete that line (it belongs to a removed surface).

Then verify by hand:
- No "Search…" field at the sidebar top.
- The top search bar is the only search entry point; Ctrl+K and Ctrl+F both open it.
- Conversation nav + message search + scroll-to-message all still work (re-run the Task 4 Step 6 checks).

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "refactor(ui): remove QuickSwitcher, SearchModal, sidebar search field (replaced by unified search)"
```

---

## Task 6: Settings — expose the shortcut choice

**Files:**
- Modify: `ui/settings/settings_screen.slint`
- Modify: `crates/chatter-ui/src/bin_ui/features/modals/settings.rs`
- Modify: settings-hydration site (where other UI prefs are pushed to the window at startup — locate via `set_search_shortcut`-adjacent prefs; e.g. `features/auth/initial_data.rs` or wherever `set_compact_mode`/theme is applied).

- [ ] **Step 1: Add the window callback + add a selector in settings**

In `ui/app.slint` add a callback (near other settings callbacks):

```slint
    callback settings-search-shortcut-changed(string); // "k" | "f" | "both"
```

In `ui/settings/settings_screen.slint`, add a row (follow the existing toggle/row pattern in that file — e.g. how read-receipts is rendered) offering three choices ("Ctrl+K", "Ctrl+F", "Both"). Each selection invokes `root.settings-search-shortcut-changed("k"|"f"|"both")` and reflects the current `root.search-shortcut-pref`. Bind the screen's incoming pref property to `search-shortcut-pref` from `app.slint`.

> NOTE: match the existing settings-screen widget style. If it uses a segmented control or radio rows elsewhere, reuse that component; otherwise three `Button`s with a `selected` highlight bound to `search-shortcut-pref` is sufficient.

- [ ] **Step 2: Persist on change (Rust)**

In `crates/chatter-ui/src/bin_ui/features/modals/settings.rs`, inside `wire_settings_callbacks`, add:

```rust
    let ui_ctx_shortcut = ui_ctx.clone();
    let weak_shortcut = window.as_weak();
    window.on_settings_search_shortcut_changed(move |value: SharedString| {
        let shortcut = chatter_core::services::search::SearchShortcut::from_str(value.as_str());
        if let Err(e) = ui_ctx_shortcut.set_search_shortcut(shortcut) {
            tracing::error!("Failed to save search shortcut: {}", e);
            return;
        }
        if let Some(w) = weak_shortcut.upgrade() {
            w.set_search_shortcut_pref(shortcut.as_str().into());
        }
    });
```

- [ ] **Step 3: Hydrate the pref at startup (Rust)**

At the settings-hydration site (where theme/compact-mode are applied to the window after login), add:

```rust
    window.set_search_shortcut_pref(ui_ctx.get_search_shortcut().as_str().into());
```

> NOTE: locate this site by searching for an existing `window.set_*` that pushes a `SettingsRepository`-backed pref to the window at startup. Place the line alongside it so it runs once the client is ready.

- [ ] **Step 4: Verify**

Run: `cargo run -p chatter-ui`
Expected:
- Settings shows a "Search shortcut" selector reflecting the saved value (default "Both").
- Set it to "Ctrl+K": now Ctrl+F no longer opens search, Ctrl+K does.
- Set it to "Ctrl+F": now Ctrl+K no longer opens search, Ctrl+F does.
- Restart the app: the choice persists.

- [ ] **Step 5: Commit**

```bash
git add ui/app.slint ui/settings/settings_screen.slint crates/chatter-ui/src/bin_ui/features/modals/settings.rs
git commit -m "feat(ui): settings selector for search shortcut (Ctrl+K / Ctrl+F / Both)"
```

---

## Self-Review (filled by plan author)

**Spec coverage (Change A sections):**
- §1 Placement → Task 4 Step 2. ✓
- §2 Resting→open (dropdown under bar) → Task 3 component + Task 4 mount. ✓
- §3 Palette contents (Jump to + Messages, arrow nav, empty=recent) → Task 3 + Task 4. ✓
- §4 Core service (filter/arrange in core) → Task 1 (`filter_conversations`); message search already core. ✓
- §5 Removals (sidebar field, quick_switcher.slint, search_modal.slint, dead glue) → Task 5. ✓
- §6 Shortcut user-selectable + persisted + help dialog updated → Task 2 (persist), Task 4 Step 3 (FocusScope reads pref), Task 5 Step 2 (help text), Task 6 (settings UI). ✓
- Change B (Join-in-Create tab) → out of scope here (separate plan), per user sequencing. ✓

**Open confirmations flagged inline (NOTE blocks):** `settings_repo` accessor path in `client/mod.rs`; `StorageError` import path; theme/compact-mode constants exist in Slint; the startup hydration site for prefs; settings-screen widget style. These are verification-on-touch, not gaps.

**Type consistency:** `QuickSwitcherItem`, `SearchResult`, `QuickSwitcherEntry`, `SearchShortcut`, callback names (`unified-search-*`) used consistently across Tasks 3/4/5/6. ✓

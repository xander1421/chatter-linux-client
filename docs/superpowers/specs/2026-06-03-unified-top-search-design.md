# Unified Top Search (Slack-style) — Design

**Date:** 2026-06-03
**Client:** Linux/desktop (Rust + Slint), `linux-client/`
**Status:** Approved design, pending implementation plan

## Problem

Search is fragmented across three surfaces:

1. Sidebar-top "Search..." field → `search-clicked()`.
2. `SearchModal` (`ui/modals/search_modal.slint`) — message content search, current channel or all.
3. `QuickSwitcher` (`ui/modals/quick_switcher.slint`, Ctrl+K) — jump to conversations/people.

Two of these (SearchModal, QuickSwitcher) overlap conceptually and are opened by
different shortcuts (Ctrl+F vs Ctrl+K) with different UIs. Users expect a single,
discoverable, top-of-window search like Slack.

## Goal

One unified, Slack-style search: a persistent top-center search bar in the chat
column, opening a single palette that merges conversation/people navigation
("Jump to") with message-content search ("Messages"). One user-selectable shortcut.

## Current architecture (relevant)

```
VerticalLayout (main, app.slint:1039)
 ├ ConnectionBanner                       (full width)
 └ HorizontalLayout (1050)
    ├ Sidebar                             (top "Search..." field; Channels-header magnifier + plus)
    └ content HorizontalLayout (1188)
       └ chat column VerticalLayout (1196)
          ├ ActiveCallView                ("Anonymous Call 6:19" call dock)
          └ ChatLayout
```

- Shortcuts: `FocusScope` (app.slint:871) — Ctrl+K opens `quick-switcher` modal,
  Ctrl+F opens `search` modal.
- Rust glue:
  - `features/quick_switcher.rs` — collects items from sidebar models, ranks via
    `chatter_core::services::quick_switcher::arrange_entries`.
  - `features/header.rs` — `on_search_messages` → `ui_ctx.search_messages_local(...)`
    (chatter-core).
- Message search + conversation ranking already live in `chatter-core`. Only the
  merge/entry-point is missing.

## Design

### 1. Placement

New `SearchHeader` Slint component inserted at the **top of the chat-column
`VerticalLayout`** (app.slint ~1196), **above** `ActiveCallView`.

- Spans chat-pane width; the search box itself is centered with a max-width cap
  (~640px) so it does not stretch edge-to-edge on wide windows.
- Sits below `ConnectionBanner`, visually aligned with the sidebar's top region.
- Hidden when `active-call-fullscreen` (the fullscreen call takes over the column).

### 2. Resting → open behavior

- Resting: rounded pill, magnifier icon + "Search..." placeholder.
- Click **or** the configured shortcut → opens the **unified palette** as a
  **dropdown anchored directly under the bar** (Slack-style), not a centered modal.
- The text input lives in the bar; typing drives live results in the dropdown.
- Escape / click-outside closes the dropdown and clears focus.

### 3. Unified palette contents

One overlay, two sections, single arrow-key navigation spanning both; Enter opens
the highlighted item:

- **Jump to** — conversations + people (replaces QuickSwitcher).
- **Messages** — message content matches (replaces SearchModal).
- Empty query → "recent conversations" (QuickSwitcher's current empty-state).

Item rows reuse the visual rendering from the existing `QuickSwitcherItem` /
`SearchResultItem` components, lifted into the new `unified_search.slint`.

### 4. Core service

Add a `chatter-core` entry point (working name
`services::search::unified_search(query, ctx) -> UnifiedSearchResult { conversations, messages }`)
that:

- Reuses `quick_switcher::arrange_entries` for the conversation/people side.
- Reuses the existing local message search (`search_messages_local`) for the
  message side.
- Performs ranking/merge/limits in **core**, per the project rule that business
  logic and data shaping live in `chatter-core`, never in `chatter-ui`.

The UI exposes a single `unified-search(query)` callback and renders the two
returned lists. The split quick-switcher/search-messages glue is replaced.

### 5. Removals (clean break — no fallbacks/shims)

- Sidebar top "Search..." field and its `search-clicked()` path.
- Sidebar Channels-header **magnifier** icon. The Channels header keeps `+`
  (create channel) and gains a **separate search button beside it** that opens the
  unified palette.
- `ui/modals/search_modal.slint` and `ui/modals/quick_switcher.slint` modals,
  plus their now-dead Slint properties/callbacks and Rust wiring. Replaced by the
  new `ui/components/unified_search.slint` and a single
  `features/unified_search.rs` wiring module (subsumes the old
  `features/quick_switcher.rs` and the search-messages part of `features/header.rs`).

### 6. Shortcut (user-selectable)

- New settings preference **Search shortcut**: `Ctrl+K` / `Ctrl+F` / `Both`
  (default **Both**).
- `FocusScope` reads the preference; the matching combo focuses the top search bar
  and opens the palette.
- Ctrl+K and Ctrl+F no longer open two different surfaces — both (or whichever is
  configured) open the one unified search.
- Update the keyboard-shortcuts help dialog (app.slint ~2025) accordingly.

## Out of scope

- Server-side / global search beyond what `search_messages_local` already does.
- Search filters (by sender, date, has-attachment) — future enhancement.
- Android client (separate UI; this spec is desktop-only).

## Success criteria

1. A persistent centered search bar appears at the top of the chat column.
2. Clicking it or pressing the configured shortcut opens a dropdown palette
   showing "Jump to" (conversations/people) and "Messages" (content) sections.
3. Arrow keys move across both sections; Enter opens the highlighted result.
4. Sidebar top search field and Channels-header magnifier are gone; a search
   button sits beside the Channels `+`.
5. The old SearchModal and QuickSwitcher modals are fully removed (no dead code).
6. Settings exposes the shortcut choice; changing it changes which combo opens
   search; default is Both.
7. `cargo run -p chatter-ui` launches and all flows work (verification per repo
   rule: run, don't just build/check).

# Android accessibility bridge

## Why the bridge is needed

The Android backend renders Slint into a SurfaceView. Android sees the native
view hierarchy, not Slint's item tree. Desktop AccessKit receives that tree
through the winit adapter; enabling Slint's `accessibility` feature alone does
not supply an Android `AccessibilityNodeProvider`.

The bridge is a patch to the vendored
`vendor/i-slint-backend-android-activity` (Slint 1.18.1). Its per-change
rationale is in `vendor/i-slint-backend-android-activity/PATCHES.md`; the code is
`accessibility.rs`, `javahelper.rs` and `java/SlintAndroidJavaHelper.java`
(`SlintNodeProvider`, `SlintInputView`) in that directory.

## What it does

`SlintInputView`, the full-size native view over the SurfaceView, returns a
`SlintNodeProvider` that serves virtual Android nodes while an accessibility
service is enabled. No tree walk happens during normal rendering or when
accessibility is off.

- **Snapshots.** A provider query calls JNI `accessibility::query`, which asks
  Slint's event thread for a snapshot and waits up to 500 ms; on timeout it
  returns the last finished tree. One snapshot runs at a time; snapshots over
  200 ms are logged. Java reuses a snapshot for 100 ms from when it is ready, so
  Android's per-node queries share one.
- **Node contents.** Virtual ID, parent ID, role, label, description,
  physical-pixel rectangle, visibility, enabled/checked state, focusability and
  available actions. IDs are stable while an item exists (keyed by item-tree
  address and item index, checked against a stored weak reference).
  Items clipped out of view are skipped; zero-size nodes are reported not
  visible; a Text node whose label equals its parent's is dropped; a labelled
  node inside a clickable labelled ancestor is not focusable, so a chat row is
  one TalkBack stop. Text inputs carry their value as node text and their label
  or placeholder as the hint.
- **Host node.** The host node explicitly reports the input view's parent and
  screen bounds, visibility and enabled state.
  `AccessibilityNodeInfo.obtain(host)` alone leaves an invisible 0x0 host, and
  Android then skips every virtual child. The input view stays non-focusable on
  the GameActivity path; IME ownership and insets handling are unchanged.
- **Modals.** Snapshots serialize only the last visible
  `accessible-id: "modal-overlay"` subtree when one is open; click dispatch
  re-checks modal membership so a stale cached ID cannot activate content
  behind an overlay.
- **Actions.** `ACTION_CLICK` runs the item's default accessibility action,
  `ACTION_LONG_CLICK` runs Slint's Expand action, and
  `ACTION_ACCESSIBILITY_FOCUS` / clear are tracked by the provider. A movable
  Flickable between an accessible node and its accessible parent is reported as
  a scrollable `ScrollView` node with `ACTION_SCROLL_FORWARD/BACKWARD`; scrolling
  moves the viewport 80% of a page and sends `TYPE_VIEW_SCROLLED`.

The sidebar `ChatRow` (`ui/sidebar/chat_list.slint`) is `list-item` with its
conversation label, binds `accessible-action-default` to row activation and
`accessible-action-expand` to its menu.

## Not implemented

The bridge sends no content-change events: there is no `PropertyTracker`
invalidation, so Android learns of changed labels, structure, visibility or
geometry only on its next query (after the 100 ms cache). Keyboard focus, text
editing actions, range actions and richer role/state mapping are not bridged.

## Device verification

With TalkBack or another accessibility service active, open a chat list and
run `uiautomator dump`. The native host `View` must have nonempty window-sized
bounds and `visible="true"`; a 0x0 invisible host means Android will not visit
the rows. Chat rows should have nonempty `content-desc`, screen bounds and
`clickable="true"`. Move TalkBack focus across rows, double-tap one and confirm
it opens that conversation; double-tap-and-hold should open its menu. Scroll a
long list with TalkBack and confirm rows past the first screen become reachable.
Change the visible list and confirm a later query announces the new rows. Check
that touch and IME behavior still work with TalkBack off and on.

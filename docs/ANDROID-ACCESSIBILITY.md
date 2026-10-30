# Android accessibility bridge

## Why the bridge is needed

The Android backend renders Slint into a SurfaceView. Android sees the native
view hierarchy, not Slint's item tree. Desktop AccessKit receives that tree
through the winit adapter; enabling Slint's `accessibility` feature alone does
not supply an Android `AccessibilityNodeProvider`.

Slint 1.17.1 exposes `ItemRc::is_accessible`, `accessible_role`,
`accessible_string_property`, `supported_accessibility_actions`, geometry,
visibility, and `accessible_action`. `accessible_descendents` walks through
non-accessible items to the next accessible children. `WindowInner` supplies
the current component and can instantiate repeaters before a walk. The winit
adapter uses these APIs, tracks components and item indices for node identity,
and maps `Action::Click` to `AccessibilityAction::Default`.

## Current first slice

`SlintInputView`, the existing full-size native view over the SurfaceView,
provides virtual Android nodes while `AccessibilityManager` is enabled. A
provider query calls a JNI method; the JNI method asks Slint's event thread
for a fresh snapshot and waits up to 500 ms. No tree walk occurs in normal
rendering or when accessibility is off. Java keeps a snapshot for at most
100 ms so Android's series of per-node queries does not rebuild it each time.
The provider's host node reports the input view's laid-out bounds in its parent
and on screen, plus its visibility and enabled state. `AccessibilityNodeInfo.obtain(host)`
does not fill those fields: the first device dump reported an invisible native
`View` at `Rect(0,0-0,0)`, so Android skipped the virtual children. The GameActivity
path keeps the input view non-focusable; this accessibility fix does not change
the SurfaceView's IME ownership or the input view's insets callbacks.

The snapshot includes a virtual ID, parent ID, role, label, description,
physical-pixel rectangle, visibility, enabled/checked state, and default
action availability. IDs refer to weak Slint item references and remain stable
while an item exists. Java translates the snapshot into
`AccessibilityNodeInfo` children, screen bounds, spoken text or content
description, accessibility focus, and `ACTION_CLICK`. A click returns to Slint's event thread and
invokes the item's default accessibility action. The shared `ChatRow` exposes
its conversation name and last message and binds its default action to the
existing row activation callback.

The native view's location is added to Slint's physical-pixel item bounds for
`boundsInScreen`; nested nodes get parent-relative `boundsInParent`. The
bridge does not put application data into logs.

## Next work

Slint core has `WindowAdapterInternal::register_item_tree`,
`unregister_item_tree`, and `handle_focus_change`, but no direct
accessibility-property-changed callback in 1.17.1. A complete bridge should
use `PropertyTracker` dirty handlers, like winit's AccessKit adapter, to
invalidate Java's node cache and send Android content-change events when
accessible properties, structure, visibility, or geometry change. It should
also invalidate on focus, register/unregister, popup changes, and viewport
changes; preserve IDs across component churn without unbounded weak-slot
growth; and add keyboard focus, scroll, text
editing, range actions, and richer role/state mapping. Those features are
outside this first slice.

## Device verification

With TalkBack or another accessibility service active, open a chat list and
run `uiautomator dump`. Chat rows should have nonempty `content-desc`, screen
bounds, and `clickable="true"`. Move TalkBack focus across rows, double-tap
one, and confirm it opens that conversation. Then change the visible list and
confirm a subsequent query announces the new row labels. Check that touch
and IME behavior still work with TalkBack off and on.
The native host `View` should also have nonempty window-sized bounds and
`visible="true"`; a 0x0 invisible host means Android will not visit the rows.

// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

//! Android's synchronous virtual-view query, bridged to Slint's event thread.

use crate::CURRENT_WINDOW;
use i_slint_core::accessibility::{
    AccessibilityAction, AccessibleStringProperty as Property, SupportedAccessibilityAction,
    accessible_descendents,
};
use i_slint_core::item_tree::ItemRc;
use i_slint_core::items::{AccessibleRole, Flickable};
use i_slint_core::window::WindowInner;
use jni::objects::{JClass, JString};
use jni::sys::jint;
use jni::Env;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

// The last finished tree and how many snapshots have finished. Android's
// query waits at most 500 ms; on a large screen (an emoji tab, ~700 nodes,
// O0 generated code on the S9) a snapshot took longer and Android was
// handed "[]" — an empty tree for TalkBack until the screen got smaller.
// A query that times out now returns the last finished tree, and the late
// snapshot replaces it for the next query. One snapshot runs at a time.
static LATEST: Mutex<(u64, String)> = Mutex::new((0, String::new()));
static FINISHED: Condvar = Condvar::new();
static IN_FLIGHT: AtomicBool = AtomicBool::new(false);

pub(crate) fn snapshot() -> String {
    CURRENT_WINDOW.with_borrow(|window| {
        let Some(adapter) = window.upgrade() else { return "[]".into() };
        let inner = WindowInner::from_pub(&adapter.window);
        inner.ensure_tree_instantiated();
        let Some(component) = inner.try_component() else { return "[]".into() };
        let mut items = adapter.accessibility_items.borrow_mut();
        let mut ids = adapter.accessibility_ids.borrow_mut();
        let root = ItemRc::new_root(component);
        let mut modal = None;
        find_topmost_modal(&root, &mut modal);
        let mut nodes = Vec::new();
        let mut scrollers = std::collections::HashMap::new();
        let mut walk = Walk { scale: adapter.window.scale_factor(), items: &mut items, ids: &mut ids, nodes: &mut nodes, scrollers: &mut scrollers };
        visit(modal.unwrap_or(root), -1, None, None, false, &mut walk);
        serde_json::to_string(&nodes).unwrap_or_else(|_| "[]".into())
    })
}

// Accessible IDs are metadata, not spoken labels. The last visible marker in
// accessible tree order is the frontmost overlay in the app's overlay stack.
fn find_topmost_modal(item: &ItemRc, modal: &mut Option<ItemRc>) {
    // An overlay is on screen; nothing under a clipped-away item is.
    if !item.is_visible() {
        return;
    }
    if item.accessible_string_property(Property::Id).as_deref() == Some("modal-overlay") {
        *modal = Some(item.clone());
    }
    for child in accessible_descendents(item) {
        find_topmost_modal(&child, modal);
    }
}

fn contains_accessible_item(root: &ItemRc, target: &ItemRc) -> bool {
    root.downgrade() == target.downgrade()
        || accessible_descendents(root).any(|child| contains_accessible_item(&child, target))
}

struct Walk<'a> {
    scale: f32,
    items: &'a mut Vec<i_slint_core::item_tree::ItemWeak>,
    ids: &'a mut std::collections::HashMap<(usize, u32), i32>,
    nodes: &'a mut Vec<Value>,
    // Flickables already emitted in this snapshot, by item key.
    scrollers: &'a mut std::collections::HashMap<(usize, u32), i32>,
}

fn item_key(item: &ItemRc) -> (usize, u32) {
    (vtable::VRef::as_ptr(vtable::VRc::borrow(item.item_tree())).as_ptr() as usize, item.index())
}

impl Walk<'_> {
    // `items` only grows (ids must stay stable for clicks). Finding an item's
    // id by scanning it made every new node cost the whole list: after a few
    // emoji-grid rebuilds the S9 snapshot overran `query`'s 500 ms and
    // Android got an empty tree. Look ids up by item-tree address and index;
    // an address a freed tree left behind is trusted only while the stored
    // weak still names this item.
    fn id_of(&mut self, item: &ItemRc) -> i32 {
        let weak = item.downgrade();
        let key = item_key(item);
        match self.ids.get(&key) {
            Some(&id) if self.items[id as usize] == weak => id,
            _ => {
                let id = self.items.len() as i32;
                self.items.push(weak);
                self.ids.insert(key, id);
                id
            }
        }
    }

    fn bounds(&self, item: &ItemRc) -> (i32, i32, i32, i32) {
        let geometry = item.geometry();
        let origin = item.map_to_window(geometry.origin);
        let scale = self.scale;
        ((origin.x * scale).round() as i32, (origin.y * scale).round() as i32,
         ((origin.x + geometry.size.width) * scale).round() as i32,
         ((origin.y + geometry.size.height) * scale).round() as i32)
    }

    /// The node that stands for the nearest Flickable between `item` and its
    /// accessible parent, emitted once per snapshot. Slint does not expose
    /// Flickables to accessibility, and with rows off screen left out of the
    /// tree (see `visit`) TalkBack had nothing to scroll: the rest of a list,
    /// the settings tabs past "Linked devices", older messages, were out of
    /// reach. Android scrolls a container that offers SCROLL_FORWARD/BACKWARD.
    fn scroller(&mut self, item: &ItemRc, parent_item: Option<&ItemRc>, parent: i32) -> i32 {
        let stop = parent_item.map(|p| p.downgrade());
        let mut cursor = item.parent_item(i_slint_core::item_tree::ParentItemTraversalMode::FindAllParents);
        let flick = loop {
            let Some(current) = cursor else { return parent };
            if stop.as_ref() == Some(&current.downgrade()) { return parent }
            if current.downcast::<Flickable>().is_some() { break current }
            cursor = current.parent_item(i_slint_core::item_tree::ParentItemTraversalMode::FindAllParents);
        };
        let key = item_key(&flick);
        if let Some(&id) = self.scrollers.get(&key) {
            return id;
        }
        let (backward, forward) = scroll_room(&flick);
        if !backward && !forward {
            self.scrollers.insert(key, parent);
            return parent;
        }
        let id = self.id_of(&flick);
        let (left, top, right, bottom) = self.bounds(&flick);
        self.nodes.push(json!({
            "id": id, "parent": parent, "role": "View",
            "left": left, "top": top, "right": right, "bottom": bottom,
            "clickable": false, "focusable": false, "visible": true,
            "scrollable": true, "scroll_backward": backward, "scroll_forward": forward,
        }));
        self.scrollers.insert(key, id);
        id
    }
}

/// Whether the Flickable can move back (up/left) and forward (down/right),
/// on its vertical axis if that scrolls, else its horizontal one.
fn scroll_room(flick: &ItemRc) -> (bool, bool) {
    let Some(f) = flick.downcast::<Flickable>() else { return (false, false) };
    let f = f.as_pin_ref();
    let size = flick.geometry().size;
    let (offset, extent, view) = if f.content_height().get() > size.height + 1.0 {
        (f.content_y().get(), f.content_height().get(), size.height)
    } else {
        (f.content_x().get(), f.content_width().get(), size.width)
    };
    (offset < -1.0, offset + extent > view + 1.0)
}

fn visit(
    item: ItemRc,
    parent: i32,
    parent_item: Option<&ItemRc>,
    parent_label: Option<&str>,
    // Inside a clickable node with its own label: that label already speaks
    // for this text, as a native row reads its children as one stop.
    in_labelled_clickable: bool,
    walk: &mut Walk<'_>,
) {
    // Items clipped out of view (the rest of a 500-cell emoji tab, scrolled
    // off messages) cannot be read or reached on screen, and every node costs
    // property evaluation: with them a snapshot took ~1.2 s on the S9 and
    // stalled the UI thread for each TalkBack query. The root always stays.
    if parent >= 0 && !item.is_visible() {
        return;
    }
    // A button's visible text repeats the button's own label ("Saved
    // messages", "Edit profile", "GIFs"): TalkBack stopped on both. A native
    // Button is one node; drop the repeat.
    let label = item.accessible_string_property(Property::Label).map(|s| s.to_string());
    if item.accessible_role() == AccessibleRole::Text && label.is_some() && label.as_deref() == parent_label {
        return;
    }
    let parent = if parent >= 0 { walk.scroller(&item, parent_item, parent) } else { parent };
    let id = walk.id_of(&item);
    let (left, top, right, bottom) = walk.bounds(&item);
    let property = |name| item.accessible_string_property(name).map(|s| s.to_string());
    let clickable = item.supported_accessibility_actions().contains(SupportedAccessibilityAction::Default);
    walk.nodes.push(json!({
        "id": id,
        "parent": parent,
        "role": match item.accessible_role() {
            AccessibleRole::Button => "Button",
            AccessibleRole::Text => "Text",
            AccessibleRole::TextInput => "TextInput",
            AccessibleRole::List => "List",
            AccessibleRole::ListItem => "ListItem",
            AccessibleRole::Checkbox => "Checkbox",
            AccessibleRole::Switch => "Switch",
            _ => "View",
        },
        "label": label.clone(),
        "description": property(Property::Description),
        "value": property(Property::Value),
        "placeholder": property(Property::PlaceholderText),
        "left": left, "top": top, "right": right, "bottom": bottom,
        "clickable": clickable,
        "long_clickable": item.supported_accessibility_actions().contains(SupportedAccessibilityAction::Expand),
        // TalkBack stopped on the row ("Ops 265, last message …") and again
        // on each text inside it ("Ops 265", "You: …", "01:37").
        // Interactive roles stay stops inside a row even without a default
        // action (the Calls row's redial buttons went unreachable).
        "focusable": clickable
            || matches!(item.accessible_role(), AccessibleRole::Button | AccessibleRole::Checkbox
                | AccessibleRole::Switch | AccessibleRole::TextInput)
            || (label.as_deref().is_some_and(|l| !l.is_empty()) && !in_labelled_clickable),
        "enabled": property(Property::Enabled).as_deref() != Some("false"),
        "checkable": property(Property::Checkable).as_deref() == Some("true"),
        "checked": property(Property::Checked).as_deref() == Some("true"),
        // A banner collapsed to zero height is still `is_visible()`.
        "visible": item.is_visible() && right > left && bottom > top,
    }));
    for child in accessible_descendents(&item) {
        let labelled_clickable = in_labelled_clickable
            || (clickable && label.as_deref().is_some_and(|l| !l.is_empty()));
        visit(child, id, Some(&item), label.as_deref(), labelled_clickable, walk);
    }
}

pub(crate) fn query<'local>(
    env: &mut Env<'local>,
    _class: JClass<'local>,
) -> Result<JString<'local>, jni::errors::Error> {
    let seen = LATEST.lock().map(|latest| latest.0).unwrap_or(0);
    if !IN_FLIGHT.swap(true, Ordering::AcqRel) {
        let queued = i_slint_core::api::invoke_from_event_loop(|| {
            let started = Instant::now();
            let tree = snapshot();
            let took = started.elapsed();
            if took > Duration::from_millis(200) {
                crate::a11y_log(&format!("accessibility snapshot took {} ms", took.as_millis()));
            }
            if let Ok(mut latest) = LATEST.lock() {
                *latest = (latest.0 + 1, tree);
            }
            IN_FLIGHT.store(false, Ordering::Release);
            FINISHED.notify_all();
        });
        if queued.is_err() {
            IN_FLIGHT.store(false, Ordering::Release);
        }
    }
    let tree = match LATEST.lock() {
        Ok(latest) => match FINISHED.wait_timeout_while(latest, Duration::from_millis(500), |l| l.0 == seen) {
            Ok((latest, _)) if !latest.1.is_empty() => latest.1.clone(),
            _ => "[]".to_owned(),
        },
        Err(_) => "[]".to_owned(),
    };
    JString::new(env, tree)
}

pub(crate) fn click<'local>(
    _env: &mut Env<'local>,
    _class: JClass<'local>,
    id: jint,
) -> Result<(), jni::errors::Error> {
    perform(id, AccessibilityAction::Default, SupportedAccessibilityAction::Default);
    Ok(())
}

/// Android's long click (TalkBack: double tap and hold) runs the item's
/// Expand action: chat rows open their menu with it, as a long press does.
pub(crate) fn long_click<'local>(
    _env: &mut Env<'local>,
    _class: JClass<'local>,
    id: jint,
) -> Result<(), jni::errors::Error> {
    perform(id, AccessibilityAction::Expand, SupportedAccessibilityAction::Expand);
    Ok(())
}

fn perform(id: jint, action: AccessibilityAction, required: SupportedAccessibilityAction) {
    if id < 0 { return }
    let _ = i_slint_core::api::invoke_from_event_loop(move || {
        CURRENT_WINDOW.with_borrow(|window| {
            let Some(adapter) = window.upgrade() else { return };
            let item = adapter.accessibility_items.borrow().get(id as usize).and_then(|item| item.upgrade());
            if let Some(item) = item {
                let inner = WindowInner::from_pub(&adapter.window);
                inner.ensure_tree_instantiated();
                let Some(component) = inner.try_component() else { return };
                let root = ItemRc::new_root(component);
                let mut modal = None;
                find_topmost_modal(&root, &mut modal);
                if !item.is_visible()
                    || modal.as_ref().is_some_and(|modal| !contains_accessible_item(modal, &item))
                {
                    return;
                }
                if item.supported_accessibility_actions().contains(required) {
                    item.accessible_action(&action);
                }
            }
        });
    });
}

/// Android's SCROLL_FORWARD/BACKWARD on a node `scroller` emitted: move the
/// Flickable most of a page along the axis `scroll_room` reported.
pub(crate) fn scroll<'local>(
    _env: &mut Env<'local>,
    _class: JClass<'local>,
    id: jint,
    forward: jint,
) -> Result<(), jni::errors::Error> {
    if id < 0 { return Ok(()) }
    let _ = i_slint_core::api::invoke_from_event_loop(move || {
        CURRENT_WINDOW.with_borrow(|window| {
            let Some(adapter) = window.upgrade() else { return };
            let Some(item) = adapter.accessibility_items.borrow().get(id as usize).and_then(|item| item.upgrade())
            else { return };
            if !item.is_visible() { return }
            let Some(flick) = item.downcast::<Flickable>() else { return };
            let flick = flick.as_pin_ref();
            let size = item.geometry().size;
            let vertical = flick.content_height().get() > size.height + 1.0;
            let (offset, extent, view) = if vertical {
                (Flickable::FIELD_OFFSETS.content_y().apply_pin(flick), flick.content_height().get(), size.height)
            } else {
                (Flickable::FIELD_OFFSETS.content_x().apply_pin(flick), flick.content_width().get(), size.width)
            };
            let step = view * 0.8 * if forward != 0 { -1.0 } else { 1.0 };
            let target = (offset.get().get() + step).clamp((view - extent).min(0.0), 0.0);
            offset.set(i_slint_core::lengths::LogicalLength::new(target));
        });
    });
    Ok(())
}

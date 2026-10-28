// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

//! Android's synchronous virtual-view query, bridged to Slint's event thread.

use crate::CURRENT_WINDOW;
use i_slint_core::accessibility::{
    AccessibilityAction, AccessibleStringProperty as Property, SupportedAccessibilityAction,
    accessible_descendents,
};
use i_slint_core::item_tree::ItemRc;
use i_slint_core::items::AccessibleRole;
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
        visit(modal.unwrap_or(root), -1, adapter.window.scale_factor(), &mut items, &mut ids, &mut nodes);
        serde_json::to_string(&nodes).unwrap_or_else(|_| "[]".into())
    })
}

// Accessible IDs are metadata, not spoken labels. The last visible marker in
// accessible tree order is the frontmost overlay in the app's overlay stack.
fn find_topmost_modal(item: &ItemRc, modal: &mut Option<ItemRc>) {
    if item.is_visible()
        && item.accessible_string_property(Property::Id).as_deref() == Some("modal-overlay")
    {
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

fn visit(
    item: ItemRc,
    parent: i32,
    scale: f32,
    items: &mut Vec<i_slint_core::item_tree::ItemWeak>,
    ids: &mut std::collections::HashMap<(usize, u32), i32>,
    nodes: &mut Vec<Value>,
) {
    // `items` only grows (ids must stay stable for clicks). Finding an item's
    // id by scanning it made every new node cost the whole list: after a few
    // emoji-grid rebuilds the S9 snapshot overran `query`'s 500 ms and
    // Android got an empty tree. Look ids up by item-tree address and index;
    // an address a freed tree left behind is trusted only while the stored
    // weak still names this item.
    let weak = item.downgrade();
    let key = (vtable::VRef::as_ptr(vtable::VRc::borrow(item.item_tree())).as_ptr() as usize, item.index());
    let id = match ids.get(&key) {
        Some(&id) if items[id as usize] == weak => id,
        _ => {
            let id = items.len() as i32;
            items.push(weak);
            ids.insert(key, id);
            id
        }
    };
    let geometry = item.geometry();
    let origin = item.map_to_window(geometry.origin);
    let left = (origin.x * scale).round() as i32;
    let top = (origin.y * scale).round() as i32;
    let right = ((origin.x + geometry.size.width) * scale).round() as i32;
    let bottom = ((origin.y + geometry.size.height) * scale).round() as i32;
    let property = |name| item.accessible_string_property(name).map(|s| s.to_string());
    let clickable = item.supported_accessibility_actions().contains(SupportedAccessibilityAction::Default);
    nodes.push(json!({
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
        "label": property(Property::Label),
        "description": property(Property::Description),
        "value": property(Property::Value),
        "placeholder": property(Property::PlaceholderText),
        "left": left, "top": top, "right": right, "bottom": bottom,
        "clickable": clickable,
        "enabled": property(Property::Enabled).as_deref() != Some("false"),
        "checkable": property(Property::Checkable).as_deref() == Some("true"),
        "checked": property(Property::Checked).as_deref() == Some("true"),
        // A banner collapsed to zero height is still `is_visible()`.
        "visible": item.is_visible() && right > left && bottom > top,
    }));
    for child in accessible_descendents(&item) {
        visit(child, id, scale, items, ids, nodes);
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
    if id < 0 { return Ok(()) }
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
                if item.supported_accessibility_actions().contains(SupportedAccessibilityAction::Default) {
                    item.accessible_action(&AccessibilityAction::Default);
                }
            }
        });
    });
    Ok(())
}

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
use std::sync::mpsc;
use std::time::Duration;

pub(crate) fn snapshot() -> String {
    CURRENT_WINDOW.with_borrow(|window| {
        let Some(adapter) = window.upgrade() else { return "[]".into() };
        let inner = WindowInner::from_pub(&adapter.window);
        inner.ensure_tree_instantiated();
        let Some(component) = inner.try_component() else { return "[]".into() };
        let mut items = adapter.accessibility_items.borrow_mut();
        let mut nodes = Vec::new();
        visit(ItemRc::new_root(component), -1, adapter.window.scale_factor(), &mut items, &mut nodes);
        serde_json::to_string(&nodes).unwrap_or_else(|_| "[]".into())
    })
}

fn visit(
    item: ItemRc,
    parent: i32,
    scale: f32,
    items: &mut Vec<i_slint_core::item_tree::ItemWeak>,
    nodes: &mut Vec<Value>,
) {
    let weak = item.downgrade();
    let id = if let Some(id) = items.iter().position(|entry| *entry == weak) {
        id as i32
    } else {
        let id = items.len() as i32;
        items.push(weak);
        id
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
        "left": left, "top": top, "right": right, "bottom": bottom,
        "clickable": clickable,
        "enabled": property(Property::Enabled).as_deref() != Some("false"),
        "checkable": property(Property::Checkable).as_deref() == Some("true"),
        "checked": property(Property::Checked).as_deref() == Some("true"),
        "visible": item.is_visible(),
    }));
    for child in accessible_descendents(&item) {
        visit(child, id, scale, items, nodes);
    }
}

pub(crate) fn query<'local>(
    env: &mut Env<'local>,
    _class: JClass<'local>,
) -> Result<JString<'local>, jni::errors::Error> {
    let (sender, receiver) = mpsc::sync_channel(1);
    let result = i_slint_core::api::invoke_from_event_loop(move || {
        let _ = sender.send(snapshot());
    });
    let tree = if result.is_ok() {
        receiver.recv_timeout(Duration::from_millis(500)).unwrap_or_else(|_| "[]".into())
    } else {
        "[]".into()
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
                if item.supported_accessibility_actions().contains(SupportedAccessibilityAction::Default) {
                    item.accessible_action(&AccessibilityAction::Default);
                }
            }
        });
    });
    Ok(())
}

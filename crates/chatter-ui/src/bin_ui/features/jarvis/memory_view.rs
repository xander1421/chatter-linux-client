//! Jarvis memory browser — facts, entities, and the radial connection map.
//!
//! Layout is computed here (presentation math, not business logic): the
//! selected entity sits at the center of a virtual 400x300 space, its
//! connections on a ring around it.

use std::sync::Arc;

use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use chatter_core::client::ChatterClient;

use crate::{
    JarvisGlobal, MainWindow, MemoryEntityData, MemoryFactData, MemoryGraphEdgeData,
    MemoryGraphNodeData,
};

const VW: f32 = 400.0;
const VH: f32 = 300.0;

fn kind_str(t: chatter_core::domain::jarvis::EntityType) -> &'static str {
    use chatter_core::domain::jarvis::EntityType::*;
    match t {
        Person => "person",
        Project => "project",
        Topic => "topic",
        Organization => "organization",
        Technology => "technology",
    }
}

pub fn wire(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    {
        let ui_ctx = ui_ctx.clone();
        let weak = window.as_weak();
        window
            .global::<crate::JarvisPane>()
            .on_jarvis_memory_refresh(move || {
                let ui_ctx = ui_ctx.clone();
                let weak = weak.clone();
                tokio::spawn(async move { refresh(ui_ctx, weak, None).await });
            });
    }
    {
        let ui_ctx = ui_ctx.clone();
        let weak = window.as_weak();
        window
            .global::<crate::JarvisPane>()
            .on_jarvis_memory_select_entity(move |entity_id: SharedString| {
                let ui_ctx = ui_ctx.clone();
                let weak = weak.clone();
                tokio::spawn(
                    async move { refresh(ui_ctx, weak, Some(entity_id.to_string())).await },
                );
            });
    }
    {
        let ui_ctx = ui_ctx.clone();
        let weak = window.as_weak();
        window.global::<JarvisGlobal>().on_clear_history(move || {
            let ui_ctx = ui_ctx.clone();
            let weak = weak.clone();
            tokio::spawn(async move {
                if let Err(error) = ui_ctx.jarvis_clear_history() {
                    tracing::error!(%error, "failed to clear Jarvis history");
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_toast_message("Couldn't clear Jarvis history. Try again.".into());
                            w.set_toast_variant("error".into());
                            w.set_toast_visible(true);
                        }
                    });
                    return;
                }
                let current = matches!(ui_ctx.active_conversation(),
                    chatter_core::state::ActiveConversation::Channel(id)
                        if chatter_core::client::is_jarvis_channel(&id));
                if current {
                    let weak_chat = weak.clone();
                    let ui_ctx_chat = ui_ctx.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak_chat.upgrade() {
                            if matches!(ui_ctx_chat.active_conversation(),
                                chatter_core::state::ActiveConversation::Channel(id)
                                    if chatter_core::client::is_jarvis_channel(&id)) {
                                w.global::<crate::ChatPane>()
                                    .set_messages(ModelRc::new(crate::bin_ui::message_model::MessageModel::default()));
                            }
                        }
                    });
                }
                refresh(ui_ctx, weak, None).await
            });
        });
    }
}

async fn refresh(
    ui_ctx: Arc<ChatterClient>,
    weak: slint::Weak<MainWindow>,
    selected: Option<String>,
) {
    // Facts + entities come from SQLite via the Jarvis façade (fast, sync).
    let facts = ui_ctx.jarvis_memory_facts();
    let entities = ui_ctx.jarvis_top_entities(12);

    let selected_id = selected.or_else(|| entities.first().map(|e| e.id.to_string()));
    let connections = selected_id
        .as_deref()
        .map(|id| ui_ctx.jarvis_entity_connections(id))
        .unwrap_or_default();
    let center_name = selected_id
        .as_deref()
        .and_then(|id| entities.iter().find(|e| e.id.to_string() == id))
        .map(|e| (e.name.clone(), kind_str(e.entity_type).to_string()));

    let fact_rows: Vec<MemoryFactData> = facts
        .iter()
        .map(|f| MemoryFactData {
            predicate: f.predicate.clone().into(),
            object: f.object.clone().into(),
            confidence_label: format!("{:.0}%", f.decayed_confidence() * 100.0).into(),
            since_label: match (f.valid_from, f.valid_until) {
                (Some(from), None) => format!("since {}", from.format("%Y-%m-%d")).into(),
                (Some(from), Some(until)) => {
                    format!("{} → {}", from.format("%Y-%m-%d"), until.format("%Y-%m-%d")).into()
                }
                _ => "".into(),
            },
            is_current: f.valid_until.is_none(),
        })
        .collect();

    let entity_rows: Vec<MemoryEntityData> = entities
        .iter()
        .map(|e| MemoryEntityData {
            entity_id: e.id.to_string().into(),
            name: e.name.clone().into(),
            kind: kind_str(e.entity_type).into(),
            mention_label: if e.mention_count == 1 {
                "1 mention".into()
            } else {
                format!("{} mentions", e.mention_count).into()
            },
            is_selected: selected_id.as_deref() == Some(&e.id.to_string()),
        })
        .collect();

    // Radial map in the 400x300 virtual space.
    let (cx, cy) = (VW / 2.0, VH / 2.0);
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    if let Some((name, kind)) = center_name {
        nodes.push(MemoryGraphNodeData {
            name: name.into(),
            kind: kind.into(),
            x: cx,
            y: cy,
            is_center: true,
        });
        let ring: Vec<_> = connections.iter().take(10).collect();
        let n = ring.len().max(1) as f32;
        let radius = 110.0_f32;
        for (i, (entity, _depth)) in ring.iter().enumerate() {
            let angle = (i as f32) / n * std::f32::consts::TAU - std::f32::consts::FRAC_PI_2;
            let x = cx + radius * angle.cos();
            let y = cy + (radius * 0.75) * angle.sin();
            edges.push(MemoryGraphEdgeData {
                x1: cx,
                y1: cy,
                x2: x,
                y2: y,
            });
            nodes.push(MemoryGraphNodeData {
                name: entity.name.clone().into(),
                kind: kind_str(entity.entity_type).into(),
                x,
                y,
                is_center: false,
            });
        }
    }

    let stats = ui_ctx.jarvis_memory_stats_label();

    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            w.global::<crate::JarvisPane>()
                .set_jarvis_memory_facts(ModelRc::new(VecModel::from(fact_rows)));
            w.global::<crate::JarvisPane>()
                .set_jarvis_memory_entities(ModelRc::new(VecModel::from(entity_rows)));
            w.global::<crate::JarvisPane>()
                .set_jarvis_memory_graph_nodes(ModelRc::new(VecModel::from(nodes)));
            w.global::<crate::JarvisPane>()
                .set_jarvis_memory_graph_edges(ModelRc::new(VecModel::from(edges)));
            w.global::<crate::JarvisPane>()
                .set_jarvis_memory_stats(stats.into());
        }
    });
}

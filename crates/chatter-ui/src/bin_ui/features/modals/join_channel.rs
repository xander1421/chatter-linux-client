//! Join channel modal callbacks.
//!
//! Uses ChannelService for all channel operations - no direct network calls.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use slint::{ComponentHandle, ModelRc, SharedString, VecModel};
use tokio::time::{sleep, Duration};

use chatter_core::client::ChatterClient;
use chatter_core::domain::channel::ChannelType;

use crate::bin_ui::converters::avatar_bytes_to_image;
use crate::bin_ui::features::sidebar::load_channel_into_ui;
use crate::{MainWindow, MembershipGlobal, SearchResultChannel};

pub fn wire_join_channel(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let search_counter = Arc::new(AtomicU64::new(0));
    let ui_ctx = ui_ctx.clone();

    // Search channels callback (with debounce)
    let weak_window = window.as_weak();
    let ui_ctx_search = ui_ctx.clone();
    let search_counter_clone = search_counter.clone();

    window.on_search_channels(move |query: SharedString| {
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx_search.clone();
        let query = query.to_string();
        let counter = search_counter_clone.clone();

        // Increment counter for this search
        let search_id = counter.fetch_add(1, Ordering::SeqCst) + 1;

        // A pasted join link is not a search: select it so Join uses it.
        // Core decides what a link is. This callback runs on the UI thread.
        let is_link = ChatterClient::is_channel_join_link(&query);
        if let Some(w) = weak.upgrade() {
            w.global::<MembershipGlobal>().set_join_input_is_link(is_link);
            if is_link {
                w.set_join_channel_results(ModelRc::new(VecModel::from(Vec::<SearchResultChannel>::new())));
                w.set_join_channel_searching(false);
                w.set_join_channel_error("".into());
                w.set_join_channel_selected_id(query.as_str().into());
                return;
            }
        }

        tokio::spawn(async move {
            // Debounce: wait 300ms before searching
            sleep(Duration::from_millis(300)).await;

            // Check if a newer search was triggered
            if counter.load(Ordering::SeqCst) != search_id {
                return;
            }

            // Empty query - clear results
            if query.trim().is_empty() {
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        w.set_join_channel_results(ModelRc::new(VecModel::from(
                            Vec::<SearchResultChannel>::new(),
                        )));
                        w.set_join_channel_searching(false);
                        w.set_join_channel_error("".into());
                    }
                });
                return;
            }

            // Set searching state
            let weak_loading = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_loading.upgrade() {
                    w.set_join_channel_searching(true);
                    w.set_join_channel_error("".into());
                }
            });

            // Check again after setting loading state
            if counter.load(Ordering::SeqCst) != search_id {
                return;
            }

            // Perform search using ChannelService
            match ui_ctx
                .search_channels_filtered(
                    &query,
                    20,
                    0,
                    vec![ChannelType::Public], // Only public channels
                )
                .await
            {
                Ok(channels) => {
                    // Check if still the latest search
                    if counter.load(Ordering::SeqCst) != search_id {
                        return;
                    }

                    // Ship raw data (Send) to the UI thread — slint::Image is !Send,
                    // so build SearchResultChannel inside the event-loop closure.
                    let raw: Vec<(String, String, String, Option<Vec<u8>>)> = channels
                        .into_iter()
                        .map(|ch| (
                            ch.id.to_string(),
                            ch.name,
                            ch.description.unwrap_or_default(),
                            ch.icon,
                        ))
                        .collect();

                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            let results: Vec<SearchResultChannel> = raw
                                .into_iter()
                                .map(|(id, name, description, icon_bytes)| {
                                    let icon_img = icon_bytes
                                        .as_deref()
                                        .and_then(avatar_bytes_to_image);
                                    SearchResultChannel {
                                        id: id.into(),
                                        name: name.into(),
                                        description: description.into(),
                                        member_count: 0,
                                        has_icon: icon_img.is_some(),
                                        icon: icon_img.unwrap_or_default(),
                                    }
                                })
                                .collect();
                            w.set_join_channel_results(ModelRc::new(VecModel::from(results)));
                            w.set_join_channel_searching(false);
                        }
                    });
                }
                Err(e) => {
                    if counter.load(Ordering::SeqCst) != search_id {
                        return;
                    }

                    let error_msg = format!("Search failed: {}", e);
                    tracing::error!("{}", error_msg);

                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_join_channel_searching(false);
                            w.set_join_channel_error(error_msg.into());
                        }
                    });
                }
            }
        });
    });

    // Join channel callback
    let weak_window = window.as_weak();
    let ui_ctx = ui_ctx.clone();
    let chat_state = ui_ctx.chat_state().clone();

    window.on_join_channel(move |channel_id_str: SharedString| {
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx.clone();
        let chat_state = chat_state.clone();
        let channel_id_str = channel_id_str.to_string();
        tokio::spawn(async move {
            // Set joining state
            let weak_loading = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_loading.upgrade() {
                    w.set_join_channel_joining(true);
                    w.set_join_channel_error("".into());
                }
            });

            // A channel id or a join link; core tells them apart and sets up
            // group encryption either way.
            match ui_ctx.join_channel_from_input(&channel_id_str).await {
                Ok(channel) => {
                    let channel_id = channel.id;
                    let channel_id_str: SharedString = channel_id.to_string().into();

                    // Set the joined channel as active
                    chat_state.set_active_channel(channel_id);

                    // The joined channel's row comes from core, like every row.
                    crate::bin_ui::features::sidebar::rebuild_chat_list(&weak, &ui_ctx);
                    let weak_modal = weak.clone();
                    let channel_id_for_ui = channel_id_str.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak_modal.upgrade() {
                            // Close modal and clear form
                            w.set_join_channel_joining(false);
                            w.set_join_channel_search_query("".into());
                            w.set_join_channel_selected_id("".into());
                            w.global::<MembershipGlobal>().set_join_input_is_link(false);
                            w.set_join_channel_results(ModelRc::new(VecModel::from(
                                Vec::<SearchResultChannel>::new(),
                            )));
                            crate::bin_ui::features::back::close(&w, "create-channel");

                            // Set loading state for channel switch
                            w.set_loading_messages(true);
                            w.invoke_channel_selected(channel_id_for_ui);
                        }
                    });

                    // Load the joined channel properly (loads messages, sets header)
                    load_channel_into_ui(
                        weak,
                        channel_id,
                        ui_ctx,
                        Default::default(),
                        None,
                    ).await;


                    tracing::info!("Joined channel: {}", channel_id);
                }
                Err(e) => {
                    use chatter_core::error::NetworkError;
                    use chatter_core::services::channel::ChannelError;
                    let error_msg = if matches!(
                        e,
                        ChannelError::ChannelNotFound(_) | ChannelError::Network(NetworkError::NotFound(_))
                    ) {
                        "Channel not found or is private".to_string()
                    } else {
                        format!("Failed to join: {}", e)
                    };
                    tracing::error!("Failed to join channel: {}", e);

                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_join_channel_joining(false);
                            w.set_join_channel_error(error_msg.into());
                        }
                    });
                }
            }
        });
    });
}

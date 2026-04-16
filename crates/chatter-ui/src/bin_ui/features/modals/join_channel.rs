//! Join channel modal callbacks.
//!
//! Uses ChannelService for all channel operations - no direct network calls.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use tokio::time::{sleep, Duration};

use chatter_core::client::ChatterClient;
use chatter_core::domain::channel::ChannelType;
use chatter_core::domain::ids::ChannelId;

use crate::bin_ui::converters::channel_to_slint;
use crate::bin_ui::features::sidebar::load_channel_into_ui;
use crate::{ChannelListItemData, MainWindow, SearchResultChannel};

pub fn wire_join_channel(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let search_counter = Arc::new(AtomicU64::new(0));
    let ui_ctx = ui_ctx.clone();

    // Search channels callback (with debounce)
    let weak_window = window.as_weak();
    let channel_service = ui_ctx.channels().clone();
    let search_counter_clone = search_counter.clone();

    window.on_search_channels(move |query: SharedString| {
        let weak = weak_window.clone();
        let channel_service = channel_service.clone();
        let query = query.to_string();
        let counter = search_counter_clone.clone();

        // Increment counter for this search
        let search_id = counter.fetch_add(1, Ordering::SeqCst) + 1;

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
            match channel_service
                .search_channels(
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

                    let results: Vec<SearchResultChannel> = channels
                        .into_iter()
                        .map(|ch| SearchResultChannel {
                            id: ch.id.to_string().into(),
                            name: ch.name.into(),
                            description: ch.description.unwrap_or_default().into(),
                            member_count: 0, // Would need additional API call
                        })
                        .collect();

                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
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

            // Parse channel ID
            let channel_id: ChannelId = match channel_id_str.parse() {
                Ok(id) => id,
                Err(e) => {
                    let error_msg = format!("Invalid channel ID: {}", e);
                    tracing::error!("{}", error_msg);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_join_channel_joining(false);
                            w.set_join_channel_error(error_msg.into());
                        }
                    });
                    return;
                }
            };

            // Join channel via ChatterClient (handles mailbox + SKDM automatically)
            match ui_ctx.join_channel(&channel_id, None).await {
                Ok(channel) => {
                    let channel_id = channel.id;
                    let channel_id_str: SharedString = channel_id.to_string().into();

                    // Set the joined channel as active
                    chat_state.set_active_channel(channel_id);

                    // Move domain Channel to UI thread; convert there (slint::Image is !Send).
                    let channel_for_ui = channel.clone();
                    let attachments_for_ui = ui_ctx.attachments().clone();
                    let weak_modal = weak.clone();
                    let channel_id_for_ui = channel_id_str.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak_modal.upgrade() {
                            let slint_channel = channel_to_slint(&channel_for_ui, &attachments_for_ui);
                            // Close modal and clear form
                            w.set_join_channel_joining(false);
                            w.set_join_channel_search_query("".into());
                            w.set_join_channel_selected_id("".into());
                            w.set_join_channel_results(ModelRc::new(VecModel::from(
                                Vec::<SearchResultChannel>::new(),
                            )));
                            w.set_active_modal("".into());

                            // Add joined channel to sidebar list
                            let current_channels = w.get_channels();
                            let mut channels: Vec<ChannelListItemData> = (0..current_channels.row_count())
                                .filter_map(|i| current_channels.row_data(i))
                                .collect();
                            channels.push(slint_channel);
                            w.set_channels(ModelRc::new(VecModel::from(channels)));

                            // Set loading state for channel switch
                            w.set_loading_messages(true);
                            w.set_selected_channel_id(channel_id_for_ui);
                            w.set_selected_dm_id("".into());
                        }
                    });

                    // Load the joined channel properly (loads messages, sets header)
                    load_channel_into_ui(
                        weak,
                        channel_id,
                        ui_ctx,
                    ).await;


                    tracing::info!("Joined channel: {}", channel_id);
                }
                Err(e) => {
                    let error_msg = if e.to_string().contains("not found") {
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

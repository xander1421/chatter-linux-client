//! Audio and screen device selection.
//!
//! Handles device enumeration and selection for calls.

use std::sync::Arc;

use slint::{ComponentHandle, ModelRc, VecModel};

use crate::AudioDeviceData;
use crate::MainWindow;
#[cfg(not(target_os = "android"))]
use crate::ScreenSourceData;
#[cfg(not(target_os = "android"))]
use chatter_core::calls::{ScreenCapture, ScreenSourceType};
use chatter_core::client::ChatterClient;

// Display the actual snapshot identity, never infer a device from ordering/default flags.
fn selected_audio_name<'a>(devices: impl IntoIterator<Item = (&'a str, &'a str)>, selected: &str) -> String {
    devices.into_iter().find(|(id, _)| *id == selected)
        .map(|(_, name)| name.to_owned()).unwrap_or_else(|| "Device unavailable".into())
}

/// Wire device picker callbacks for audio device and screen source selection.
pub fn wire_device_picker(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak = window.as_weak();

    let refresh_generation = std::rc::Rc::new(std::cell::Cell::new(0u64));
    window.global::<crate::CallView>().on_refresh_audio_controls({
        let weak = weak.clone();
        let ui_ctx = ui_ctx.clone();
        move || {
            let Some(w) = weak.upgrade() else { return };
            let view = w.global::<crate::CallView>();
            let generation = refresh_generation.get().wrapping_add(1);
            refresh_generation.set(generation);
            #[cfg(target_os = "android")]
            {
                // Models and selected IDs are published from AudioManager acknowledgements.
                use slint::Model;
                let selected = view.get_selected_audio_output_id();
                let devices: Vec<_> = view.get_audio_output_devices().iter().collect();
                let name = selected_audio_name(devices.iter().map(|d| (d.id.as_str(), d.name.as_str())), selected.as_str());
                view.set_audio_input_name(name.clone().into());
                view.set_audio_output_name(name.into());
            }
            #[cfg(not(target_os = "android"))]
            {
                let weak = weak.clone();
                let ui_ctx = ui_ctx.clone();
                let refresh_generation = refresh_generation.clone();
                let call_id = view.get_active_call_data().call_id;
                let session = ui_ctx.auth_snapshot().session_revision;
                slint::spawn_local(async move {
                    for output in [false, true] {
                        let result = ui_ctx.call_audio_device_snapshot(output).await;
                        let Some(w) = weak.upgrade() else { return };
                        let view = w.global::<crate::CallView>();
                        if refresh_generation.get() != generation
                            || !view.get_audio_controls_visible()
                            || view.get_active_call_data().call_id != call_id
                            || ui_ctx.auth_snapshot().session_revision != session { return; }
                        let name = match result {
                            Ok((devices, selected)) => selected_audio_name(devices.iter().map(|d| (d.id.as_str(), d.name.as_str())), &selected),
                            Err(_) => "Device unavailable".into(),
                        };
                        if output { view.set_audio_output_name(name.into()); }
                        else { view.set_audio_input_name(name.into()); }
                    }
                }).unwrap();
            }
        }
    });

    {
        // Refresh an open picker on hotplug, including actual selection after fallback.
        thread_local! { static REFRESH: slint::Timer = slint::Timer::default(); }
        let weak = weak.clone();
        REFRESH.with(|timer| timer.start(slint::TimerMode::Repeated, std::time::Duration::from_secs(2), move || {
            let Some(w) = weak.upgrade() else { return };
            let view = w.global::<crate::CallView>();
            if view.get_audio_controls_visible() { view.invoke_refresh_audio_controls(); }
            #[cfg(not(target_os = "android"))]
            if view.get_device_picker_visible() {
                match view.get_device_picker_type().as_str() {
                    "audio" => view.invoke_show_audio_device_picker(),
                    "audio_output" => view.invoke_show_audio_output_picker(),
                    _ => {},
                }
            }
        }));
    }

    for output in [false, true] {
        let weak = weak.clone();
        let ui_ctx = ui_ctx.clone();
        let show = move || {
            #[cfg(target_os = "android")]
            { crate::platform::android::audio_route::show_picker(); }
            #[cfg(not(target_os = "android"))]
            {
                let weak = weak.clone();
                let ui_ctx = ui_ctx.clone();
                // Open immediately; a late refresh must never reopen a dismissed sheet.
                if let Some(w) = weak.upgrade() {
                    w.global::<crate::CallView>().set_device_picker_type(if output { "audio_output" } else { "audio" }.into());
                    w.global::<crate::CallView>().set_device_picker_visible(true);
                }
                slint::spawn_local(async move {
                    let result = ui_ctx.call_audio_device_snapshot(output).await;
                    let Some(w) = weak.upgrade() else { return };
                    let view = w.global::<crate::CallView>();
                    match result {
                        Ok((devices, selected)) => {
                            let rows: Vec<AudioDeviceData> = devices.into_iter().map(|d| AudioDeviceData {
                                id: d.id.into(), name: d.name.into(), is_default: d.is_default,
                            }).collect();
                            let model = ModelRc::new(VecModel::from(rows));
                            if output {
                                view.set_audio_output_devices(model);
                                view.set_selected_audio_output_id(selected.into());
                            } else {
                                view.set_audio_devices(model);
                                view.set_selected_audio_device_id(selected.into());
                            }
                        }
                        Err(error) => {
                            tracing::warn!(%error, "Could not list call audio devices");
                            picker_error(&w, "Could not list audio devices. Try again.");
                        }
                    }
                }).unwrap();
            }
        };
        if output { window.global::<crate::CallView>().on_show_audio_output_picker(show); }
        else { window.global::<crate::CallView>().on_show_audio_device_picker(show); }
    }

    // Show screen source picker (desktop only)
    window
        .global::<crate::CallView>()
        .on_show_screen_source_picker({
            let weak = weak.clone();
            let ui_ctx = ui_ctx.clone();
            move || {
                #[cfg(target_os = "android")]
                {
                    let _ = &ui_ctx;
                    if let Some(w) = weak.upgrade() {
                        w.global::<crate::CallView>().invoke_call_toggle_screen_share();
                    }
                    return;
                }
                #[cfg(not(target_os = "android"))]
                {
                    let weak = weak.clone();
                    let ui_ctx = ui_ctx.clone();
                    slint::spawn_local(async move {
                        // Gate on the room being connected. During an outgoing
                        // "Calling…" call there is no room yet, so opening the
                        // picker would only end in a `NotInCall` failure after the
                        // user selected a source (and the OS portal dialog would
                        // never appear). Don't surface the picker until connected.
                        if !ui_ctx.call_is_connected().await {
                            tracing::info!("Screen share unavailable — call not connected yet");
                            if let Some(w) = weak.upgrade() {
                                picker_error(&w, "Screen sharing is available once the call connects.");
                            }
                            return;
                        }
                        // Desktop capturer discovery may invoke an OS portal.
                        std::thread::spawn(move || {
                            let result = ScreenCapture::list_sources();
                            let _ = slint::invoke_from_event_loop(move || {
                                let Some(w) = weak.upgrade() else { return };
                                match result {
                                    Ok(sources) => {
                                        let source_count = sources.len();
                                        let ui_sources: Vec<ScreenSourceData> = sources
                                            .into_iter()
                                            .enumerate()
                                            .map(|(idx, s)| ScreenSourceData {
                                                id: s.id.to_string().into(),
                                                name: s.name.into(),
                                                width: 0,
                                                height: 0,
                                                is_monitor: s.source_type == ScreenSourceType::Monitor,
                                                is_primary: idx == 0 && s.source_type == ScreenSourceType::Monitor,
                                            })
                                            .collect();
                                        let (monitors, windows): (Vec<_>, Vec<_>) = ui_sources
                                            .into_iter().partition(|source| source.is_monitor);
                                        w.global::<crate::CallView>()
                                            .set_screen_monitors(ModelRc::new(VecModel::from(monitors)));
                                        w.global::<crate::CallView>()
                                            .set_screen_windows(ModelRc::new(VecModel::from(windows)));
                                        w.global::<crate::CallView>()
                                            .set_device_picker_type("screen".into());
                                        w.global::<crate::CallView>()
                                            .set_device_picker_visible(true);
                                        tracing::info!("Showing screen source picker with {} sources", source_count);
                                    }
                                    Err(e) => {
                                        tracing::error!("Failed to enumerate screen sources: {}", e);
                                        picker_error(&w, "Could not list screens. Try again.");
                                    }
                                }
                            });
                        });
                    })
                    .unwrap();
                }
            }
        });

    // Handle audio input device selection
    window
        .global::<crate::CallView>()
        .on_audio_device_selected({
            let ui_ctx = ui_ctx.clone();
            let weak = weak.clone();
            move |device_id| {
                #[cfg(target_os = "android")]
                { crate::platform::android::audio_route::select(&device_id); return; }
                let device_id_str = device_id.to_string();
                tracing::info!("Audio input device selected: {}", device_id_str);

                let ui_ctx = ui_ctx.clone();
                let weak = weak.clone();
                slint::spawn_local(async move {
                    let result = ui_ctx.call_publish_microphone(Some(&device_id_str)).await;
                    #[cfg(not(target_os = "android"))]
                    let snapshot = ui_ctx.call_audio_device_snapshot(false).await;
                    if let Some(w) = weak.upgrade() {
                        #[cfg(not(target_os = "android"))]
                        if let Ok((_, selected)) = snapshot {
                            w.global::<crate::CallView>().set_selected_audio_device_id(selected.into());
                        }
                        if let Err(error) = result {
                            tracing::warn!(%error, "Microphone switch failed");
                            picker_error(&w, "Could not switch microphones. Try again.");
                        }
                    }
                })
                .unwrap();
            }
        });

    // Handle audio output device selection
    window
        .global::<crate::CallView>()
        .on_audio_output_selected({
            let ui_ctx = ui_ctx.clone();
            let weak = weak.clone();
            move |device_id| {
                #[cfg(target_os = "android")]
                { crate::platform::android::audio_route::select(&device_id); return; }
                let device_id_str = device_id.to_string();
                let ui_ctx = ui_ctx.clone();
                let weak = weak.clone();
                slint::spawn_local(async move {
                    let result = ui_ctx.call_set_audio_output(&device_id_str).await;
                    #[cfg(not(target_os = "android"))]
                    let snapshot = ui_ctx.call_audio_device_snapshot(true).await;
                    let Some(w) = weak.upgrade() else { return };
                    #[cfg(not(target_os = "android"))]
                    if let Ok((_, selected)) = snapshot {
                        w.global::<crate::CallView>().set_selected_audio_output_id(selected.into());
                    }
                    // Failed lookup/build leaves the prior stream and tick intact.
                    if let Err(error) = result {
                        tracing::warn!(%error, "Output switch failed");
                        picker_error(&w, "Could not switch speakers. Try again.");
                    }
                })
                .unwrap();
            }
        });

    // Handle screen source selection
    window
        .global::<crate::CallView>()
        .on_screen_source_selected({
            let ui_ctx = ui_ctx.clone();
            let weak = weak.clone();
            move |source_id| {
                #[cfg(target_os = "android")]
                {
                    let _ = (source_id, &ui_ctx, &weak);
                    tracing::warn!("Screen source selection not available on Android");
                    return;
                }
                #[cfg(not(target_os = "android"))]
                {
                    let source_id_str = source_id.to_string();
                    tracing::info!("Screen source selected: {}", source_id_str);

                    let source_id_u64: u64 = match source_id_str.parse() {
                        Ok(id) => id,
                        Err(e) => {
                            tracing::error!("Invalid screen source ID '{}': {}", source_id_str, e);
                            if let Some(w) = weak.upgrade() {
                                picker_error(&w, "That screen is no longer available.");
                            }
                            return;
                        }
                    };

                    let ui_ctx = ui_ctx.clone();
                    let weak = weak.clone();
                    let source_id_display = source_id_str.clone();
                    slint::spawn_local(async move {
                        let sources_result = tokio::task::spawn_blocking(ScreenCapture::list_sources).await;
                        match sources_result {
                            Err(error) => {
                                tracing::error!("Failed to enumerate screen sources: {}", error);
                                if let Some(w) = weak.upgrade() {
                                    picker_error(&w, "Could not list screens. Try again.");
                                }
                            }
                            Ok(result) => match result {
                            Ok(sources) => {
                                if let Some(source) =
                                    sources.into_iter().find(|s| s.id == source_id_u64)
                                {
                                    if let Err(e) = ui_ctx.call_publish_screen_share(source).await {
                                        tracing::error!("Failed to start screen share: {}", e);
                                        if let Some(w) = weak.upgrade() {
                                            picker_error(&w, "Could not share that screen. Try again.");
                                        }
                                    } else {
                                        tracing::info!(
                                            "Screen share started with source: {}",
                                            source_id_display
                                        );
                                        if let Some(w) = weak.upgrade() {
                                            w.global::<crate::CallView>()
                                                .set_call_is_screen_sharing(true);
                                            w.global::<crate::CallView>()
                                                .set_selected_screen_source_id(
                                                    source_id_display.into(),
                                                );
                                            if let Some(id) = ui_ctx.session_current_user_id() {
                                                super::active::set_local_participant_screen_sharing(
                                                    &w,
                                                    &id.to_string(),
                                                    true,
                                                );
                                            }
                                        }
                                    }
                                } else {
                                    tracing::error!(
                                        "Selected screen source not found: {}",
                                        source_id_display
                                    );
                                    if let Some(w) = weak.upgrade() {
                                        picker_error(&w, "That screen is no longer available.");
                                    }
                                }
                            }
                            Err(e) => {
                                tracing::error!("Failed to enumerate screen sources: {}", e);
                                if let Some(w) = weak.upgrade() {
                                    picker_error(&w, "Could not list screens. Try again.");
                                }
                            }
                            }
                        }
                    })
                    .unwrap();
                }
            }
        });
}

fn picker_error(window: &MainWindow, message: &str) {
    window.set_toast_message(message.into());
    window.set_toast_variant("error".into());
    window.set_toast_visible(true);
}

#[cfg(test)]
mod audio_name_tests {
    use super::selected_audio_name;

    #[test]
    fn selected_audio_name_tracks_acknowledged_identity_and_unavailable() {
        let rows = [("system-default", "System default"), ("usb", "USB headset"), ("builtin", "Built-in speaker")];
        assert_eq!(selected_audio_name(rows, "usb"), "USB headset");
        assert_eq!(selected_audio_name(rows, "builtin"), "Built-in speaker");
        assert_eq!(selected_audio_name(rows, "system-default"), "System default");
        assert_eq!(selected_audio_name(rows, "removed"), "Device unavailable");
        assert_eq!(selected_audio_name(rows, ""), "Device unavailable");
    }
}

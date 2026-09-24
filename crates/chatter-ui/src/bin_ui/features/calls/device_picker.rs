//! Audio and screen device selection.
//!
//! Handles device enumeration and selection for calls.

use std::sync::Arc;

use slint::{ComponentHandle, ModelRc, VecModel};

use chatter_core::client::ChatterClient;
use chatter_core::calls::AudioCapture;
#[cfg(not(target_os = "android"))]
use chatter_core::calls::{ScreenCapture, ScreenSourceType};
use crate::AudioDeviceData;
#[cfg(not(target_os = "android"))]
use crate::ScreenSourceData;
use crate::MainWindow;

/// Wire device picker callbacks for audio device and screen source selection.
pub fn wire_device_picker(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak = window.as_weak();

    // Show audio input device picker (microphones)
    window.on_show_audio_device_picker({
        let weak = weak.clone();
        move || {
            if let Some(w) = weak.upgrade() {
                // Enumerate audio input devices
                match AudioCapture::list_devices() {
                    Ok(devices) => {
                        let ui_devices: Vec<AudioDeviceData> = devices
                            .into_iter()
                            .map(|d| AudioDeviceData {
                                id: d.id.into(),
                                name: d.name.into(),
                                is_default: d.is_default,
                            })
                            .collect();

                        w.set_audio_devices(ModelRc::new(VecModel::from(ui_devices)));
                        w.set_device_picker_type("audio".into());
                        w.set_device_picker_visible(true);
                        tracing::info!("Showing audio input device picker");
                    }
                    Err(e) => {
                        tracing::error!("Failed to enumerate audio input devices: {}", e);
                    }
                }
            }
        }
    });

    // Show audio output device picker (speakers/headphones)
    window.on_show_audio_output_picker({
        let weak = weak.clone();
        move || {
            if let Some(w) = weak.upgrade() {
                // Enumerate audio output devices
                match AudioCapture::list_output_devices() {
                    Ok(devices) => {
                        let ui_devices: Vec<AudioDeviceData> = devices
                            .into_iter()
                            .map(|d| AudioDeviceData {
                                id: d.id.into(),
                                name: d.name.into(),
                                is_default: d.is_default,
                            })
                            .collect();

                        w.set_audio_output_devices(ModelRc::new(VecModel::from(ui_devices)));
                        w.set_device_picker_type("audio_output".into());
                        w.set_device_picker_visible(true);
                        tracing::info!("Showing audio output device picker");
                    }
                    Err(e) => {
                        tracing::error!("Failed to enumerate audio output devices: {}", e);
                    }
                }
            }
        }
    });

    // Show screen source picker (desktop only)
    window.on_show_screen_source_picker({
        let weak = weak.clone();
        let ui_ctx = ui_ctx.clone();
        move || {
            #[cfg(target_os = "android")]
            { tracing::warn!("Screen source picker not available on Android"); return; }
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
                        return;
                    }
                    let Some(w) = weak.upgrade() else { return };
                    // Enumerate screen sources using LiveKit's desktop_capturer
                    match ScreenCapture::list_sources() {
                        Ok(sources) => {
                            let source_count = sources.len();
                            let ui_sources: Vec<ScreenSourceData> = sources
                                .into_iter()
                                .enumerate()
                                .map(|(idx, s)| ScreenSourceData {
                                    id: s.id.to_string().into(),
                                    name: s.name.into(),
                                    width: 0, // Not available from libwebrtc
                                    height: 0,
                                    is_monitor: s.source_type == ScreenSourceType::Monitor,
                                    is_primary: idx == 0 && s.source_type == ScreenSourceType::Monitor,
                                })
                                .collect();

                            w.set_screen_sources(ModelRc::new(VecModel::from(ui_sources)));
                            w.set_device_picker_type("screen".into());
                            w.set_device_picker_visible(true);
                            tracing::info!("Showing screen source picker with {} sources", source_count);
                        }
                        Err(e) => {
                            tracing::error!("Failed to enumerate screen sources: {}", e);
                        }
                    }
                })
                .unwrap();
            }
        }
    });

    // Handle audio input device selection
    window.on_audio_device_selected({
        let ui_ctx = ui_ctx.clone();
        let weak = weak.clone();
        move |device_id| {
            let device_id_str = device_id.to_string();
            tracing::info!("Audio input device selected: {}", device_id_str);

            let ui_ctx = ui_ctx.clone();
            let weak = weak.clone();
            slint::spawn_local(async move {
                ui_ctx.call_stop_microphone();
                if let Err(e) = ui_ctx.call_publish_microphone(Some(&device_id_str)).await {
                    tracing::error!("Failed to switch microphone to {}: {}", device_id_str, e);
                } else {
                    tracing::info!("Switched to microphone: {}", device_id_str);
                    if let Some(w) = weak.upgrade() {
                        w.set_selected_audio_device_id(device_id_str.into());
                    }
                }
            })
            .unwrap();
        }
    });

    // Handle audio output device selection
    window.on_audio_output_selected({
        let ui_ctx = ui_ctx.clone();
        let weak = weak.clone();
        move |device_id| {
            let device_id_str = device_id.to_string();
            let ui_ctx = ui_ctx.clone();
            let weak = weak.clone();
            slint::spawn_local(async move {
                let result = ui_ctx.call_set_audio_output(&device_id_str).await;
                let Some(w) = weak.upgrade() else { return };
                match result {
                    Ok(()) => w.set_selected_audio_output_id(device_id_str.into()),
                    Err(e) => {
                        // Slint set the id optimistically; don't leave a tick
                        // on a device the audio isn't actually going to.
                        tracing::error!("Failed to switch audio output to {}: {}", device_id_str, e);
                        w.set_selected_audio_output_id("".into());
                    }
                }
            })
            .unwrap();
        }
    });

    // Handle screen source selection
    window.on_screen_source_selected({
        let ui_ctx = ui_ctx.clone();
        let weak = weak.clone();
        move |source_id| {
            #[cfg(target_os = "android")]
            { let _ = source_id; let _ = &ui_ctx; tracing::warn!("Screen source selection not available on Android"); return; }
            #[cfg(not(target_os = "android"))]
            {
                let source_id_str = source_id.to_string();
                tracing::info!("Screen source selected: {}", source_id_str);

                let source_id_u64: u64 = match source_id_str.parse() {
                    Ok(id) => id,
                    Err(e) => {
                        tracing::error!("Invalid screen source ID '{}': {}", source_id_str, e);
                        return;
                    }
                };

                let ui_ctx = ui_ctx.clone();
                let weak = weak.clone();
                let source_id_display = source_id_str.clone();
                slint::spawn_local(async move {
                    match ScreenCapture::list_sources() {
                        Ok(sources) => {
                            if let Some(source) = sources.into_iter().find(|s| s.id == source_id_u64) {
                                if let Err(e) = ui_ctx.call_publish_screen_share(source).await {
                                    tracing::error!("Failed to start screen share: {}", e);
                                } else {
                                    tracing::info!("Screen share started with source: {}", source_id_display);
                                    if let Some(w) = weak.upgrade() {
                                        w.set_call_is_screen_sharing(true);
                                        w.set_selected_screen_source_id(source_id_display.into());
                                    }
                                }
                            } else {
                                tracing::error!("Selected screen source not found: {}", source_id_display);
                            }
                        }
                        Err(e) => {
                            tracing::error!("Failed to enumerate screen sources: {}", e);
                        }
                    }
                })
                .unwrap();
            }
        }
    });
}


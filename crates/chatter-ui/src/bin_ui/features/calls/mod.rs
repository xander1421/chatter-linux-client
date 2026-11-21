//! Call UI callback wiring.
//!
//! Wires call-related Slint callbacks to CallService.
//!
//! This module is organized into:
//! - `timers` - Call duration and video frame polling timers
//! - `key_exchange` - Ephemeral key exchange for E2EE calls
//! - `history` - The Calls screen (past calls, redial)
//! - `room_events` - LiveKit room event handling
//! - `outgoing` - Outgoing call initiation
//! - `incoming` - Incoming call accept/decline + orchestrator event glue
//! - `active` - Active call controls (mute, video, screen share, end)
//! - `device_picker` - Audio and screen device selection
//! - `event_handlers` - Domain event handling for call keys

use std::sync::Arc;

use crate::MainWindow;
use slint::ComponentHandle;
use chatter_core::client::ChatterClient;

mod active;
mod device_picker;
mod event_handlers;
pub mod history;
mod incoming;
pub mod key_exchange;
mod outgoing;
mod room_events;
pub mod timers;

type ConfirmedCall = Box<dyn FnOnce(bool)>;

thread_local! {
    static SHIELD_CONFIRM: std::cell::RefCell<Option<ConfirmedCall>> = const {
        std::cell::RefCell::new(None)
    };
}

fn with_shield_confirmation(
    window: &MainWindow,
    client: &ChatterClient,
    answering: bool,
    call: impl FnOnce(bool) + 'static,
) {
    if client.call_needs_shield_confirmation() {
        SHIELD_CONFIRM.with(|slot| *slot.borrow_mut() = Some(Box::new(call)));
        let view = window.global::<crate::CallView>();
        view.set_shield_confirmation_answering(answering);
        view.set_shield_confirmation_open(true);
    } else {
        call(false);
    }
}

/// Wire all call-related callbacks.
///
/// Note: call_service is passed separately because it's initialized post-boot
/// via AppContext.init_call_service() and stored in a OnceLock.
pub fn wire(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    #[cfg(target_os = "android")]
    {
        let weak = window.as_weak();
        window.global::<crate::CallView>().on_leave_keyguard_call(move || {
            if let Some(w) = weak.upgrade() {
                let id = w.global::<crate::CallView>().get_keyguard_call_id();
                crate::platform::android::activity::update_call_window(
                    jni::jni_str!("requestCallWindowUnlock"), id.as_str(),
                );
            }
        });
        window.global::<crate::CallView>().on_keyguard_call_ended(|id| {
            crate::platform::android::activity::clear_incoming_call_window(id.as_str());
        });
    }
    use slint::{Model, ModelRc, VecModel};
    let weak = window.as_weak();
    window.global::<crate::CallView>().on_shield_confirmation_answer(move |confirmed| {
        if let Some(window) = weak.upgrade() {
            window.global::<crate::CallView>().set_shield_confirmation_open(false);
        }
        let call = SHIELD_CONFIRM.with(|slot| slot.borrow_mut().take());
        if confirmed {
            if let Some(call) = call {
                call(true);
            }
        }
    });
    window.global::<crate::CallView>().on_select_call_tile_indices(|participants, pinned_id| {
        let tiles: Vec<crate::CallParticipantData> = participants.iter().collect();
        let indices = chatter_core::calls::tiles::select_visible_tiles(
            &tiles,
            pinned_id.as_str(),
            |tile| tile.user_id.as_str(),
            |tile| tile.is_screen_sharing,
            |tile| tile.is_speaking,
        );
        ModelRc::new(VecModel::from(indices.into_iter().map(|index| index as i32).collect::<Vec<_>>()))
    });
    outgoing::wire_start_call(window, ui_ctx);
    incoming::wire_incoming_call(window, ui_ctx);
    incoming::wire_orchestrator_events(window, ui_ctx);
    active::wire_active_call(window, ui_ctx);
    room_events::wire_room_events(window, ui_ctx);
    device_picker::wire_device_picker(window, ui_ctx);
    active::wire_call_hangup_handler(window, ui_ctx);
    event_handlers::wire_call_key_answer_handler(window, ui_ctx);
    history::wire(window, ui_ctx);
}

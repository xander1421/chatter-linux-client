//! Android Back key.
//!
//! `ui/main.slint`'s top-level `back-scope` FocusScope reports every Back press
//! through the `back-pressed` callback; the decision of what Back *means* is
//! made here, in Rust, so it can be tested headlessly.
//!
//! Order matters and is the whole point of this module — innermost layer first:
//!
//!   1. Message multi-select exits. Selection mode is the shallowest, most
//!      recently entered state, so it pops first.
//!   2. The unified search palette closes. It is an overlay of its own rather
//!      than an `active-modal` value, so it needs its own branch.
//!   3. An open modal closes. A Back press must never leave or background the
//!      app while a modal is up — the Settings screen's only other exit is a
//!      glyph that renders as a tofu box, so "Back does nothing" strands the
//!      user there.
//!   4. An open thread panel closes, returning to its conversation. On a
//!      phone the thread covers the chat pane, so it is a layer above it.
//!   5. The mobile conversation pane returns to the conversation list. On a
//!      phone the two are separate screens, so Back out of a chat means "up to
//!      the list" — going straight to backgrounding the app there is the single
//!      most jarring thing Back can do in a messenger. Only in the compact
//!      layout: on a two-pane tablet the list is already beside the chat, so
//!      there is nothing to go up to.
//!   6. Otherwise the task is backgrounded, which is Android's normal
//!      back-out-of-app. `moveTaskToBack(true)`, NOT `finish()`: finishing
//!      destroys the Activity and with it the whole in-memory session, which is
//!      wrong for a messaging app the user expects to come back to.
//!
//! On desktop steps 5 and 6 are no-ops: the layout is never compact there
//! (both panes are on screen at once), and — popping in-app layers is the only meaningful
//! part, and nothing should try to "background" a desktop window.

use slint::ComponentHandle;

use crate::MainWindow;

/// Wire the Back key. No core access: this is pure UI-shell navigation state.
pub fn wire(window: &MainWindow) {
    let weak = window.as_weak();
    window.on_back_pressed(move || {
        if let Some(w) = weak.upgrade() {
            handle_back(&w);
        }
    });
}

/// Apply one Back press. Returns `true` if it was consumed by in-app state
/// (a layer was popped), `false` if it fell through to backgrounding the app.
///
/// Split out from the callback so tests can drive it directly.
pub fn handle_back(window: &MainWindow) -> bool {
    if window.get_selection_mode() {
        // Both halves are required, and this is the same pair the selection
        // bar's own close button fires (ui/app.slint:2579-2580). The flag drives
        // the bar's visibility; the selected SET lives in
        // `features::chat::selection`, so without `selection-cleared` the set
        // would stay populated and the bubbles would stay visually marked.
        window.set_selection_mode(false);
        window.invoke_selection_cleared();
        return true;
    }

    // The message context menu and the reaction picker are overlays opened
    // from a bubble. Back fell through them to the conversation pane, which
    // left the menu floating over the conversation list.
    if window.get_reaction_picker_visible() {
        window.set_reaction_picker_visible(false);
        return true;
    }
    if window.get_context_menu_visible() {
        window.set_context_menu_visible(false);
        return true;
    }

    // The composer's emoji and GIF pickers are overlays too, and since the
    // emoji picker now stays open across insertions (§112u) Back is the
    // natural way to dismiss it.
    if window.get_composer_emoji_picker_visible() {
        window.set_composer_emoji_picker_visible(false);
        return true;
    }
    if window.get_composer_gif_picker_visible() {
        window.set_composer_gif_picker_visible(false);
        return true;
    }

    // The unified search palette is its own overlay, not an `active-modal`
    // value, so Back fell straight through it and backgrounded the app with
    // the palette still on screen (§112u).
    if window.get_unified_search_open() {
        window.set_unified_search_open(false);
        return true;
    }

    if window.get_active_modal().as_str() != "none" {
        window.set_active_modal("none".into());
        return true;
    }

    // The thread panel is a full screen on a phone (it hides the chat pane,
    // app.slint:1480), so Back there means "up to the conversation" — not
    // "out of the conversation to the list", which is what the next branch
    // did: opening a thread and pressing Back landed on the chat list, two
    // levels up (§112s). Fire the same callback the panel's own close button
    // does, so the thread state is cleared in core as well.
    if window.get_thread_panel_visible() {
        window.invoke_close_thread_clicked();
        return true;
    }

    // `mobile-show-chat` is set by every conversation tap, on every layout,
    // so gate on the layout rather than the flag: a two-pane tablet popping it
    // changes nothing on screen and ate a Back press. Left set there, it is
    // what keeps the chat open if the window later narrows to compact.
    if window.get_compact_layout() && window.get_mobile_show_chat() {
        window.set_mobile_show_chat(false);
        return true;
    }

    background_app();
    false
}

/// Send the task to the background, leaving the Activity (and the process, and
/// every open stream) alive.
#[cfg(target_os = "android")]
fn background_app() {
    use crate::platform::android::activity;

    let result = (|| -> Result<(), String> {
        let mut env = activity::jni_env()?;
        // The Activity registered itself in `onCreate`; reuse that GlobalRef
        // rather than ndk-context's Application, which has no Activity methods.
        let act = activity::activity()?;
        let call = env.call_method(act, "moveAppToBack", "()V", &[]);
        if call.is_err() {
            activity::clear_exception(&mut env);
        }
        call.map_err(|e| e.to_string())?;
        Ok(())
    })();

    if let Err(e) = result {
        tracing::warn!(error = %e, "back: moveTaskToBack failed; app stays foreground");
    }
}

/// Desktop: Back has no OS meaning. Closing a modal (handled above) is all it
/// does; never quit the window out from under the user.
#[cfg(not(target_os = "android"))]
fn background_app() {}

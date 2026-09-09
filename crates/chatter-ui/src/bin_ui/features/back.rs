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
//!   2. An open modal closes. A Back press must never leave or background the
//!      app while a modal is up — the Settings screen's only other exit is a
//!      glyph that renders as a tofu box, so "Back does nothing" strands the
//!      user there.
//!   3. The mobile conversation pane returns to the conversation list. On a
//!      phone the two are separate screens, so Back out of a chat means "up to
//!      the list" — going straight to backgrounding the app there is the single
//!      most jarring thing Back can do in a messenger.
//!   4. Otherwise the task is backgrounded, which is Android's normal
//!      back-out-of-app. `moveTaskToBack(true)`, NOT `finish()`: finishing
//!      destroys the Activity and with it the whole in-memory session, which is
//!      wrong for a messaging app the user expects to come back to.
//!
//! On desktop steps 3 and 4 are no-ops: `mobile-show-chat` is never set there
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

    if window.get_active_modal().as_str() != "none" {
        window.set_active_modal("none".into());
        return true;
    }

    if window.get_mobile_show_chat() {
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

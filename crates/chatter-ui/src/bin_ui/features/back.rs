//! Android Back key.
//!
//! `ui/app.slint`'s top-level `back-scope` FocusScope reports every Back press
//! through the `back-pressed` callback; the decision of what Back *means* is
//! made here, in Rust, so it can be tested headlessly.
//!
//! Order matters and is the whole point of this module — innermost layer first:
//!
//!   0. A ringing call's full-screen ring is hidden (Signal): it is drawn over
//!      everything, so Back is its. The call keeps ringing and its
//!      notification keeps Accept / Decline; nothing underneath is popped.
//!   1. Message multi-select exits. Selection mode is the shallowest, most
//!      recently entered state, so it pops first.
//!   Calls overflow and the other local pickers dismiss after selection,
//!   before the modal stack or owning tab can navigate.
//!   2. The unified search palette closes. It is an overlay of its own rather
//!      than a screen on the modal stack, so it needs its own branch.
//!   3. The top of the modal stack pops, revealing whatever it was pushed
//!      over — Settings › Sessions › Back is Settings again, not the chat
//!      list.
//!   4. A tab other than Chats returns to Chats (Android's bottom-nav
//!      convention: Chats is the start destination).
//!   5. An open thread panel closes, returning to its conversation. On a
//!      phone the thread covers the chat pane, so it is a layer above it.
//!   6. The mobile conversation pane returns to the conversation list. On a
//!      phone the two are separate screens, so Back out of a chat means "up to
//!      the list" — going straight to backgrounding the app there is the single
//!      most jarring thing Back can do in a messenger. Only in the compact
//!      layout: on a two-pane tablet the list is already beside the chat, so
//!      there is nothing to go up to.
//!   7. Otherwise the task is backgrounded, which is Android's normal
//!      back-out-of-app. `moveTaskToBack(true)`, NOT `finish()`: finishing
//!      destroys the Activity and with it the whole in-memory session, which is
//!      wrong for a messaging app the user expects to come back to.
//!
//! On desktop steps 6 and 7 are no-ops: the layout is never compact there
//! (both panes are on screen at once), and — popping in-app layers is the only meaningful
//! part, and nothing should try to "background" a desktop window.

//!
//! # Navigation model
//!
//! Two separate pieces of state, both on the window (`ui/app.slint`):
//!
//! - `active-tab` — the top-level destination: `chats`, `calls`, `contacts`,
//!   `profile` (the account menu). The bottom bar highlights it; on a two-pane
//!   layout the sidebar's Calls / Contacts entries and its avatar / gear
//!   select it. Settings is not a tab: the account menu pushes it.
//! - `modal-stack` — screens and dialogs pushed over the tab, top last.
//!   `active-modal` is derived from it (the top, or `"none"`), so each modal's
//!   `if active-modal == ...` keeps working, and closing the top reveals the
//!   one below instead of dropping the user on the chat list.
//!
//! The stack is only ever changed through [`push`], [`close`], [`pop`] and
//! [`reset_to_chats`] (Slint reaches them via `nav-push` / `nav-close` /
//! `nav-pop`), so the rules live here.

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use crate::MainWindow;

/// Wire the Back key and the Slint-side stack operations. No core access:
/// this is pure UI-shell navigation state.
pub fn wire(window: &MainWindow) {
    let weak = window.as_weak();
    window.on_back_pressed(move || {
        if let Some(w) = weak.upgrade() {
            handle_back(&w);
        }
    });
    let weak = window.as_weak();
    window.on_nav_push(move |screen| {
        if let Some(w) = weak.upgrade() {
            push(&w, &screen);
        }
    });
    let weak = window.as_weak();
    window.on_nav_close(move |screen| {
        if let Some(w) = weak.upgrade() {
            close(&w, &screen);
        }
    });
    let weak = window.as_weak();
    window.on_nav_pop(move || {
        if let Some(w) = weak.upgrade() {
            pop(&w);
        }
    });
}

fn stack(w: &MainWindow) -> Vec<SharedString> {
    w.get_modal_stack().iter().collect()
}

fn set_stack(w: &MainWindow, stack: Vec<SharedString>) {
    // Settings stays mounted under what it opens, so Slint needs "anywhere on
    // the stack", which it cannot compute from an array itself.
    w.set_settings_open(stack.iter().any(|x| x.as_str() == "settings"));
    w.set_modal_stack(ModelRc::new(VecModel::from(stack)));
}

/// Push `screen` over whatever is showing. A screen already on the stack is
/// moved to the top rather than stacked twice, so a Back never reveals a
/// second copy of the same page.
pub fn push(w: &MainWindow, screen: &str) {
    let mut s = stack(w);
    s.retain(|x| x.as_str() != screen);
    s.push(screen.into());
    set_stack(w, s);
}

/// Close `screen` and everything pushed on top of it — what it opened goes
/// with it. A no-op if it is not open, so an async completion that arrives
/// after the user already left cannot close something else.
pub fn close(w: &MainWindow, screen: &str) {
    let mut s = stack(w);
    if let Some(i) = s.iter().rposition(|x| x.as_str() == screen) {
        s.truncate(i);
        set_stack(w, s);
    }
}

/// Close the top of the stack. `false` if the stack was empty.
pub fn pop(w: &MainWindow) -> bool {
    let mut s = stack(w);
    if s.pop().is_none() {
        return false;
    }
    set_stack(w, s);
    true
}

/// Drop every pushed screen and return to the Chats tab: the landing for
/// "open this conversation" from anywhere, answering a call, and logout.
pub fn reset_to_chats(w: &MainWindow) {
    set_stack(w, Vec::new());
    w.set_active_tab("chats".into());
}

/// Apply one Back press. Returns `true` if it was consumed by in-app state
/// (a layer was popped), `false` if it fell through to backgrounding the app.
///
/// Split out from the callback so tests can drive it directly.
pub fn handle_back(window: &MainWindow) -> bool {
    // The ring covers everything, so Back belonged to it — and fell through
    // it, popping the chat underneath while the call rang on (FX3).
    if window.get_incoming_call_visible() && !window.get_incoming_call_minimized() {
        window.set_incoming_call_minimized(true);
        return true;
    }

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

    // Calls overflow is owned by the window and bound to the screen. Consume
    // it before popping a modal or returning the Calls tab to Chats.
    if window.get_calls_more_menu_visible() {
        window.set_calls_more_menu_visible(false);
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
        window.invoke_release_text_focus();
        return true;
    }
    if window.get_composer_gif_picker_visible() {
        window.set_composer_gif_picker_visible(false);
        window.invoke_release_text_focus();
        return true;
    }

    // The unified search palette is its own overlay, not a stacked screen,
    // so Back fell straight through it and backgrounded the app with the
    // palette still on screen (§112u).
    if window.get_unified_search_open() {
        window.set_unified_search_open(false);
        return true;
    }

    if pop(window) {
        return true;
    }

    if window.get_active_tab().as_str() != "chats" {
        window.set_active_tab("chats".into());
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

/// Desktop: Back has no OS meaning. Popping in-app state (handled above) is all it
/// does; never quit the window out from under the user.
#[cfg(not(target_os = "android"))]
fn background_app() {}

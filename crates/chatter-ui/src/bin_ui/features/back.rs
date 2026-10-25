//! Android Back key.
//!
//! `MainActivity` reports Android Back through the `back-pressed` callback
//! independently of Slint focus. The `back-scope` FocusScope reports keyboard
//! Back/Escape on other platforms. The navigation decision is made here.
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
    window
        .global::<crate::ModalHostState>()
        .on_nav_push(move |screen| {
            if let Some(w) = weak.upgrade() {
                push(&w, &screen);
            }
        });
    let weak = window.as_weak();
    window
        .global::<crate::ModalHostState>()
        .on_nav_close(move |screen| {
            if let Some(w) = weak.upgrade() {
                close(&w, &screen);
            }
        });
    let weak = window.as_weak();
    window
        .global::<crate::ModalHostState>()
        .on_nav_pop(move || {
            if let Some(w) = weak.upgrade() {
                pop(&w);
            }
        });
}

fn stack(w: &MainWindow) -> Vec<SharedString> {
    w.global::<crate::ModalHostState>()
        .get_modal_stack()
        .iter()
        .collect()
}

fn set_stack(w: &MainWindow, stack: Vec<SharedString>) {
    if w.global::<crate::ModalHostState>()
        .get_modal_stack()
        .iter()
        .any(|s| s == "user-profile")
        && !stack.iter().any(|s| s == "user-profile")
    {
        crate::bin_ui::features::modals::profile::clear_shared_content(w);
    }
    if w.global::<crate::ModalHostState>()
        .get_modal_stack()
        .iter()
        .any(|s| s == "edit-profile")
        && !stack.iter().any(|s| s == "edit-profile")
    {
        crate::bin_ui::features::modals::profile::retire_avatar_edit();
        w.global::<crate::ModalHostState>()
            .set_edit_profile_loading(false);
    }
    // Settings stays mounted under what it opens, so Slint needs "anywhere on
    // the stack", which it cannot compute from an array itself.
    w.global::<crate::ModalHostState>()
        .set_settings_open(stack.iter().any(|x| x.as_str() == "settings"));
    w.global::<crate::ModalHostState>()
        .set_modal_stack(ModelRc::new(VecModel::from(stack)));
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
    w.global::<crate::SidebarPane>()
        .set_active_tab("chats".into());
}

/// Apply one Back press. Returns `true` if it was consumed by in-app state
/// (a layer was popped), `false` if it fell through to backgrounding the app.
///
/// Split out from the callback so tests can drive it directly.
pub fn handle_back(window: &MainWindow) -> bool {
    // The ring covers everything, so Back belonged to it — and fell through
    // it, popping the chat underneath while the call rang on (FX3).
    if window
        .global::<crate::CallView>()
        .get_incoming_call_visible()
        && !window
            .global::<crate::CallView>()
            .get_incoming_call_minimized()
    {
        window
            .global::<crate::CallView>()
            .set_incoming_call_minimized(true);
        return true;
    }

    if window
        .global::<crate::ChatPane>()
        .get_delete_confirm_count()
        > 0
    {
        window
            .global::<crate::ChatPane>()
            .set_delete_confirm_count(0);
        return true;
    }

    if window.global::<crate::ChatPane>().get_selection_mode() {
        // Both halves are required, and this is the same pair the selection
        // bar's own close button fires (ui/app.slint:2579-2580). The flag drives
        // the bar's visibility; the selected SET lives in
        // `features::chat::selection`, so without `selection-cleared` the set
        // would stay populated and the bubbles would stay visually marked.
        window.global::<crate::ChatPane>().set_selection_mode(false);
        window
            .global::<crate::ChatPane>()
            .invoke_selection_cleared();
        return true;
    }

    // Direct media overlays are not on the modal stack. Dismiss the topmost
    // one with its normal cleanup callback before navigating the chat.
    if window.global::<crate::ChatPane>().get_pdf_preview_visible() {
        window
            .global::<crate::ChatPane>()
            .set_pdf_preview_visible(false);
        window
            .global::<crate::ChatPane>()
            .invoke_close_pdf_preview();
        return true;
    }
    if window
        .global::<crate::ChatPane>()
        .get_code_preview_visible()
    {
        window
            .global::<crate::ChatPane>()
            .set_code_preview_visible(false);
        window
            .global::<crate::ChatPane>()
            .invoke_close_code_preview();
        return true;
    }
    if window
        .global::<crate::ChatPane>()
        .get_video_player_visible()
    {
        window
            .global::<crate::ChatPane>()
            .set_video_player_visible(false);
        window
            .global::<crate::ChatPane>()
            .invoke_close_video_player();
        return true;
    }
    if window
        .global::<crate::ChatPane>()
        .get_image_viewer_visible()
    {
        window
            .global::<crate::ChatPane>()
            .set_image_viewer_visible(false);
        window
            .global::<crate::ChatPane>()
            .invoke_close_image_viewer();
        return true;
    }

    // Calls row selection: leave it before the overflow or the tab. The
    // marks live in `features::calls::history`, which this callback clears.
    if window
        .global::<crate::CallView>()
        .get_call_history_selecting()
    {
        window
            .global::<crate::CallView>()
            .invoke_call_history_end_selection();
        return true;
    }

    // Calls overflow is owned by the window and bound to the screen. Consume
    // it before popping a modal or returning the Calls tab to Chats.
    if window
        .global::<crate::CallView>()
        .get_calls_more_menu_visible()
    {
        window
            .global::<crate::CallView>()
            .set_calls_more_menu_visible(false);
        return true;
    }

    // The message context menu and the reaction picker are overlays opened
    // from a bubble. Back fell through them to the conversation pane, which
    // left the menu floating over the conversation list.
    if window
        .global::<crate::ChatPane>()
        .get_reaction_picker_visible()
    {
        window
            .global::<crate::ChatPane>()
            .set_reaction_picker_visible(false);
        return true;
    }
    if window
        .global::<crate::ChatPane>()
        .get_context_menu_visible()
    {
        window
            .global::<crate::ChatPane>()
            .set_context_menu_visible(false);
        return true;
    }

    // The composer's emoji and GIF pickers are overlays too, and since the
    // emoji picker now stays open across insertions (§112u) Back is the
    // natural way to dismiss it.
    if window
        .global::<crate::ChatPane>()
        .get_composer_emoji_picker_visible()
    {
        window
            .global::<crate::ChatPane>()
            .set_composer_emoji_picker_visible(false);
        window.invoke_release_text_focus();
        return true;
    }
    if window
        .global::<crate::ChatPane>()
        .get_composer_gif_picker_visible()
    {
        window
            .global::<crate::ChatPane>()
            .set_composer_gif_picker_visible(false);
        window.invoke_release_text_focus();
        return true;
    }

    // The unified search palette is its own overlay, not a stacked screen,
    // so Back fell straight through it and backgrounded the app with the
    // palette still on screen (§112u).
    if window
        .global::<crate::SearchPane>()
        .get_unified_search_open()
    {
        window
            .global::<crate::SearchPane>()
            .set_unified_search_open(false);
        return true;
    }

    // Recovery codes remain in the encrypted local database until Continue.
    // Keep their screen present while the user is deciding where to save them.
    if window.get_active_modal().as_str() == "recovery-codes" {
        return true;
    }

    if pop(window) {
        return true;
    }

    // A mesh conversation is pushed from the Mesh destination. Pop the
    // conversation before leaving that destination.
    if window.get_compact_layout()
        && window.global::<crate::SidebarPane>().get_mobile_show_chat()
        && window.global::<crate::SidebarPane>().get_active_tab().as_str() == "mesh"
    {
        if window.global::<crate::ChatPane>().get_thread_panel_visible() {
            window.global::<crate::ChatPane>().invoke_close_thread_clicked();
            return true;
        }
        window.global::<crate::SidebarPane>().set_mobile_show_chat(false);
        return true;
    }

    if window
        .global::<crate::SidebarPane>()
        .get_active_tab()
        .as_str()
        != "chats"
    {
        window
            .global::<crate::SidebarPane>()
            .set_active_tab("chats".into());
        return true;
    }

    // The thread panel is a full screen on a phone (it hides the chat pane,
    // app.slint:1480), so Back there means "up to the conversation" — not
    // "out of the conversation to the list", which is what the next branch
    // did: opening a thread and pressing Back landed on the chat list, two
    // levels up (§112s). Fire the same callback the panel's own close button
    // does, so the thread state is cleared in core as well.
    if window
        .global::<crate::ChatPane>()
        .get_thread_panel_visible()
    {
        window
            .global::<crate::ChatPane>()
            .invoke_close_thread_clicked();
        return true;
    }

    // `mobile-show-chat` is set by every conversation tap, on every layout,
    // so gate on the layout rather than the flag: a two-pane tablet popping it
    // changes nothing on screen and ate a Back press. Left set there, it is
    // what keeps the chat open if the window later narrows to compact.
    if window.get_compact_layout() && window.global::<crate::SidebarPane>().get_mobile_show_chat() {
        window
            .global::<crate::SidebarPane>()
            .set_mobile_show_chat(false);
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

    let result = activity::with_jni_env(|env| {
        // The Activity registered itself in `onCreate`; reuse that GlobalRef
        // rather than ndk-context's Application, which has no Activity methods.
        let act = activity::activity()?;
        let call = env.call_method(
            act,
            jni::jni_str!("moveAppToBack"),
            jni::jni_sig!("()V"),
            &[],
        );
        if call.is_err() {
            activity::clear_exception(env);
        }
        call.map_err(|e| e.to_string())?;
        Ok(())
    });

    if let Err(e) = result {
        tracing::warn!(error = %e, "back: moveTaskToBack failed; app stays foreground");
    }
}

/// Desktop: Back has no OS meaning. Popping in-app state (handled above) is all it
/// does; never quit the window out from under the user.
#[cfg(not(target_os = "android"))]
fn background_app() {}

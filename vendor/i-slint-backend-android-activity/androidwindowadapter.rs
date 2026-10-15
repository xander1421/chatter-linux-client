// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

// cSpell:ignore Eisu Endcall Hankaku Headsethook Henkan Muhenkan Numpad Pictsymbols Sysrq teriary Thumbl Thumbr Zenkaku

use super::*;
use crate::javahelper::{JavaHelper, print_jni_error};
use android_activity::input::{
    ButtonState, InputEvent, KeyAction, Keycode, MotionAction, MotionEvent,
};
use android_activity::{InputStatus, MainEvent, PollEvent};
use i_slint_core::SharedString;
use i_slint_core::api::{
    LogicalPosition, LogicalSize, PhysicalPosition, PhysicalSize, PlatformError, Window,
};
use i_slint_core::input::{InternalKeyEvent, KeyEvent, KeyEventResult, KeyEventType, TouchPhase};
use i_slint_core::lengths::PhysicalEdges;
use i_slint_core::platform::{
    Key, PointerEventButton, WindowAdapter, WindowEvent, WindowProperties,
};
use i_slint_core::timers::{Timer, TimerMode};
use i_slint_core::window::{InputMethodRequest, WindowInner};
use i_slint_renderer_skia::{SkiaRenderer, SkiaSharedContext};
use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

struct LongPressDetection {
    _timer: Timer,
    position: LogicalPosition,
}

pub struct AndroidWindowAdapter {
    app: AndroidApp,
    pub(crate) window: Window,
    pub(crate) renderer: i_slint_renderer_skia::SkiaRenderer,
    requested_graphics_api: RefCell<Option<i_slint_core::graphics::RequestedGraphicsAPI>>,
    pub(crate) event_queue: EventQueue,
    pub(crate) pending_redraw: Cell<bool>,
    /// Chatter patch: when a touch last moved; feeds the refresh-rate vote
    /// (`AndroidPlatform::with_refresh_rate_vote`).
    pub(crate) last_motion: Cell<Option<std::time::Instant>>,
    pub(super) java_helper: JavaHelper,
    pub(crate) fullscreen: Cell<bool>,
    /// The offset at which the Slint view is drawn in the native window (account for status bar)
    pub offset: Cell<PhysicalPosition>,

    /// Whether the cursor handle should be shown.
    /// They are shown when taping, but hidden whenever keys are pressed
    pub(crate) show_cursor_handles: Cell<bool>,

    long_press: RefCell<Option<LongPressDetection>>,
    /// A completed text-selection gesture must not re-enable the IME on release.
    long_press_selection: Cell<bool>,
    last_pressed_state: Cell<ButtonState>,

    /// Chatter patch: states already pushed to GameTextInput during the current
    /// committed-text epoch, to dedupe echoes.
    ///
    /// GameTextInput's set_text_input_state calls restartInput; without this,
    /// Slint echoes the IME's own keystroke back, the restart fires, Slint sees
    /// it as a change and echoes again (slint-ui/slint #9240).
    ///
    /// Slint 1.16 echoed the SAME state, so one remembered state sufficed.
    /// Slint 1.17 alternates between TWO splits of the same visible text
    /// (committed "s" + preedit "a" at offset 1, then committed "" + preedit
    /// "sa" at offset 0), so consecutive states always differ and a single-slot
    /// compare never fires — measured at ~200 restarts per keystroke on a
    /// Galaxy S9. Remember every state pushed while the COMMITTED text is
    /// unchanged: that bounds the set to one keystroke's worth and breaks
    /// cycles of any length. Cleared when the committed text changes, so a
    /// legitimately repeated state after a real edit is still pushed.
    last_ime_epoch: RefCell<Option<String>>,
    ime_states_seen: RefCell<Vec<(String, usize, usize, Option<(usize, usize)>)>>,
    /// Last `InputType` handed to the IME via `set_ime_editor_info`, so the
    /// repeated Enable requests do not re-push it (and restart the IME) on
    /// every keystroke.
    ime_input_type: Cell<Option<(i_slint_core::items::InputType, bool)>>,
    /// Whether the last state we pushed carried a compose region, i.e. whether
    /// the input method may still be composing. Used to settle composition
    /// before a pointer gesture moves the native selection.
    had_compose: Cell<bool>,
    /// Merged (committed + preedit) text of the last state pushed. Used to tell
    /// a real edit from the cursor-only churn the seen-set is meant to absorb.
    last_pushed_text: RefCell<Option<String>>,
    /// Cursor offset of the last state pushed, for the same reason.
    last_pushed_cursor: Cell<usize>,
    /// Full text of the last state the INPUT METHOD reported to us. Used to
    /// avoid echoing a stale Slint state back over a newer IME state.
    last_ime_text: RefCell<Option<String>>,
    /// Selection of the last state the INPUT METHOD reported. Paired with
    /// `last_ime_text` so a caret-only move is not mistaken for an echo (§112o).
    last_ime_selection: Cell<(usize, usize)>,
    /// State pushed by `finish_composing`, waiting for GameTextInput to echo it
    /// back. `set_text_input_state` calls `restartInput`, and the IME answers
    /// with the state it now holds — including the OLD cursor, which lands
    /// after Slint has already moved its caret for the tap and drags it back
    /// (§112o). Swallow exactly that one echo.
    pending_self_echo: RefCell<Option<(String, usize)>>,
    /// Caret that `finish_composing` handed Slint. Slint notifies its own state
    /// change a frame LATER — after the tap has already moved the caret and the
    /// IME has been told about it — so that late push carries the pre-tap
    /// cursor and would drag the caret back (§112o). Armed here, disarmed by
    /// the first push that matches it.
    stale_commit_cursor: Cell<Option<usize>>,
    /// The TextInput the input method was last enabled for. Every field above
    /// describes THIS item's editing session, not the input method's in
    /// general, so it is all forgotten when the client changes.
    ///
    /// Held weakly on purpose: Slint sends no FocusOut — hence no Disable —
    /// for a focused TextInput destroyed with its component (a modal behind
    /// an `if`, the popped conversation pane). A weak that no longer upgrades
    /// is the only sign that happened. Without it the dead field's text and
    /// composition stayed in GameTextInput and in the fields above, and the
    /// next tap's `finish_composing` replayed them into whatever field came
    /// next — the uncleareable leak in docs/OPEN-ITEMS.md §112m.
    ime_client: RefCell<Option<i_slint_core::item_tree::ItemWeak>>,
    /// Set while an input-method TextEvent is being dispatched into Slint.
    /// Updates Slint raises meanwhile are parked in `deferred_ime_update` and
    /// only the last one is pushed, once the event is fully applied.
    ///
    /// Core applies an IME commit in two steps: `insert()` puts the caret at
    /// the END of the inserted text and notifies (text.rs `insert` ->
    /// `set_cursor_position` -> `update_ime`), and only then is the event's
    /// own cursor applied and notified. Pushed as they came, the first moved
    /// GameTextInput's caret to the end, and the second — the correct one —
    /// matched what the IME had last reported and was dropped as an echo. So
    /// a tap mid-text reverted to the end and typing appended there (§112o:
    /// "push sel=(1,1)", "inbound sel=(1,1)", then "sel=(7,7)").
    ime_dispatching: Cell<bool>,
    deferred_ime_update: RefCell<Option<i_slint_core::window::InputMethodProperties>>,
    /// Explicit modifier transitions, separate from an IME's meta-only command.
    /// Restore held Shift for pointer selection without leaving virtual Ctrl stuck.
    native_held_modifiers: Cell<[bool; 8]>,
    /// Chatter patch: the app's producer gate (see `TextInputBridge`). With
    /// one bound, every outbound state goes through it and inbound states
    /// arrive through `handle_text_input_state`; android-activity's
    /// `TextEvent` is then only a wake-up.
    text_bridge: RefCell<Option<Rc<dyn TextInputBridge>>>,
    /// The field the IME serves, as the gate numbers it: a new number for
    /// each new `ime_client`. An inbound state tagged with another owner was
    /// produced for a field the IME no longer serves.
    ime_owner: Cell<u64>,
    /// How many accepted sends have consumed this backend's field so far
    /// (`consume_text_input`). An inbound state tagged with an older
    /// generation was produced before that consumption was applied on the
    /// UI thread and is dropped: it can only show text the app has taken.
    ime_generation: Cell<u64>,
    /// Reset acknowledgement and failed-publication recovery bookkeeping.
    /// A failed push leaves the current generation owed to the gate, bypassing
    /// echo guards until a retry lands. While a reset is pending, cached text
    /// must not be replayed by `finish_composing`.
    publication: crate::text_input_publication::Publication,
}

/// Chatter patch: a Slint text state for the IME's copy of the field, as
/// handed to the app's gate. Offsets are UTF-16, as Android's
/// `InputConnection` counts them. `owner` and `generation` are the values the
/// state was computed under; the gate drops a push whose generation is older
/// than a reset it has applied since.
pub struct TextInputPush {
    pub text: String,
    pub selection: (usize, usize),
    pub compose: Option<(usize, usize)>,
    pub owner: u64,
    pub generation: u64,
}

/// Chatter patch: a state the IME produced, as the gate tagged it on the UI
/// thread: the owner and generation applied when the IME made it. UTF-16
/// offsets.
pub struct InboundTextInput {
    pub text: String,
    pub selection: (usize, usize),
    pub compose: Option<(usize, usize)>,
    pub owner: u64,
    pub generation: u64,
}

/// Chatter patch: the app side of the producer gate. Both calls are made on
/// the Slint thread and must not block: the gate serializes them on the UI
/// thread with the IME's own operations.
pub trait TextInputBridge {
    /// Slint's state for the IME's field. `false` when the request did not
    /// reach the gate.
    fn push(&self, push: &TextInputPush) -> bool;
    /// An accepted send took the field's text: the IME's copy is consumed as
    /// of `generation`. The gate empties it before the IME's next operation
    /// runs (or from a posted fallback), once. `false` when the request did
    /// not reach the gate: the generation is then not advanced.
    fn consume(&self, owner: u64, generation: u64) -> bool;
}

impl WindowAdapter for AndroidWindowAdapter {
    fn window(&self) -> &Window {
        &self.window
    }
    fn size(&self) -> PhysicalSize {
        self.app.native_window().map_or_else(Default::default, |w| PhysicalSize {
            width: w.width() as u32,
            height: w.height() as u32,
        })
    }
    fn renderer(&self) -> &dyn i_slint_core::platform::Renderer {
        &self.renderer
    }

    fn request_redraw(&self) {
        self.pending_redraw.set(true);
    }

    fn update_window_properties(&self, properties: WindowProperties<'_>) {
        let f = properties.is_fullscreen();
        if self.fullscreen.replace(f) != f {
            self.resize().unwrap();
        }
    }

    fn internal(
        &self,
        _: i_slint_core::InternalToken,
    ) -> Option<&dyn i_slint_core::window::WindowAdapterInternal> {
        Some(self)
    }
}

impl i_slint_core::window::WindowAdapterInternal for AndroidWindowAdapter {
    #[cfg(feature = "native-activity")]
    fn input_method_request(&self, request: InputMethodRequest) {
        match request {
            InputMethodRequest::Enable(props) => {
                self.java_helper
                    .set_imm_data(
                        &props,
                        self.window.scale_factor(),
                        self.show_cursor_handles.get(),
                    )
                    .unwrap_or_else(|e| print_jni_error(&self.app, e));
                self.java_helper
                    .show_or_hide_soft_input(true)
                    .unwrap_or_else(|e| print_jni_error(&self.app, e));

                if let Some(focus_item) =
                    WindowInner::from_pub(&self.window).focus_item.borrow().upgrade()
                {
                    if let Some(text_input) =
                        focus_item.downcast::<i_slint_core::items::TextInput>()
                    {
                        let color = text_input.as_pin_ref().selection_background_color();
                        self.java_helper
                            .set_handle_color(color.with_alpha(1.))
                            .unwrap_or_else(|e| print_jni_error(&self.app, e));
                    }
                }
            }
            InputMethodRequest::Update(props) => {
                self.java_helper
                    .set_imm_data(
                        &props,
                        self.window.scale_factor(),
                        self.show_cursor_handles.get(),
                    )
                    .unwrap_or_else(|e| print_jni_error(&self.app, e));
            }
            InputMethodRequest::Disable => {
                self.dismiss_selection_menu();
                self.java_helper
                    .show_or_hide_soft_input(false)
                    .unwrap_or_else(|e| print_jni_error(&self.app, e));
            }
            _ => (),
        };
    }

    #[cfg(not(feature = "native-activity"))]
    fn input_method_request(&self, request: InputMethodRequest) {
        use android_activity::input::{TextInputState, TextSpan};

        let (props, show) = match request {
            InputMethodRequest::Enable(props) => {
                // NO composition reset here. Slint 1.17 fires Enable
                // repeatedly (§112c), and set_text_input_state calls
                // restartInput, so resetting on Enable — even conditionally on
                // "a composition is outstanding" — lands an empty state between
                // keystrokes and wipes them. Measured twice: the keyboard
                // opened, the field focused, and not one character ever
                // arrived. Disable now detaches ownership without an empty
                // push; the first Enable for a new session publishes that
                // field's actual state. Destroyed-field cleanup is separate.
                //
                // Tell the IME what kind of field this is. Without it every
                // field — passwords and PINs included — opened as plain text,
                // so the keyboard offered suggestions for, and learned, secrets.
                // Multi-line is a property of the focused TextInput, not of the
                // request; without it the keyboard's Enter was a DONE key and a
                // chat composer could not take a newline.
                let multi_line = WindowInner::from_pub(&self.window)
                    .focus_item
                    .borrow()
                    .upgrade()
                    .and_then(|f| {
                        f.downcast::<i_slint_core::items::TextInput>()
                            .map(|t| !t.as_pin_ref().single_line())
                    })
                    .unwrap_or(false);
                if self.ime_input_type.get() != Some((props.input_type, multi_line)) {
                    use android_activity::input::{ImeOptions, InputType, TextInputAction};
                    use i_slint_core::items::InputType as SlintInputType;
                    let mut input_type = match props.input_type {
                        SlintInputType::Password => {
                            InputType::TYPE_CLASS_TEXT
                                | InputType::TYPE_TEXT_VARIATION_PASSWORD
                                | InputType::TYPE_TEXT_FLAG_NO_SUGGESTIONS
                        }
                        SlintInputType::Number => InputType::TYPE_CLASS_NUMBER,
                        SlintInputType::Decimal => {
                            InputType::TYPE_CLASS_NUMBER | InputType::TYPE_NUMBER_FLAG_DECIMAL
                        }
                        _ => InputType::TYPE_CLASS_TEXT,
                    };
                    // GameActivity's own default imeOptions, 0x02000006: NO_FULLSCREEN
                    // plus IME_ACTION_DONE in the action bits. Without the action
                    // bits the keyboard's ✓ key turned into a newline key.
                    let mut options = ImeOptions::from_bits_retain(0x0200_0006);
                    if props.input_type == SlintInputType::Password {
                        options |= ImeOptions::IME_FLAG_NO_PERSONALIZED_LEARNING;
                    }
                    let mut action = TextInputAction::Done;
                    if multi_line && props.input_type == SlintInputType::Text {
                        input_type |= InputType::TYPE_TEXT_FLAG_MULTI_LINE;
                        options = ImeOptions::IME_FLAG_NO_FULLSCREEN
                            | ImeOptions::IME_FLAG_NO_ENTER_ACTION;
                        action = TextInputAction::None;
                    }
                    self.app.set_ime_editor_info(input_type, action, options);
                    self.ime_input_type.set(Some((props.input_type, multi_line)));
                }
                // A different field, or the first one: nothing recorded about
                // the previous client's session applies to this one (§112m).
                // Keyed on item identity, so the repeated Enables Slint sends
                // for the SAME field while typing never reach this — that is
                // what separates it from the per-Enable reset that wiped
                // keystrokes. The push below then hands GameTextInput this
                // field's own state, unfiltered by the old field's guards.
                let focus = WindowInner::from_pub(&self.window).focus_item.borrow().clone();
                if self.ime_client.borrow().as_ref() != Some(&focus) {
                    self.forget_ime_session();
                    *self.ime_client.borrow_mut() = Some(focus);
                    // A new owner for the gate: states the IME produced for
                    // the previous field carry the previous number.
                    self.ime_owner.set(self.ime_owner.get() + 1);
                }
                // A parked Update belongs to the field that had focus.
                self.deferred_ime_update.take();
                // Slint calls Enable again when the selecting finger lifts.
                // GameTextInput's show_soft_input also calls restartInput, even
                // for an already visible keyboard; resetting the echo guards
                // then repeats the selected-state restart too. Neither belongs
                // to long-press selection (which must keep IME/insets stable).
                // Chatter patch: the show itself comes AFTER the push below.
                // `TextInputBridge::push` returns with the state in the gate's
                // slot and the gate applies it when the IME attaches
                // (`onCreateInputConnection`) or before its first operation,
                // so the connection the IME gets is this field's, not an empty
                // Editable the first owner state would later overwrite.
                // `show_soft_input` travels the native work pipe and a push a
                // Handler message: with show first their order was undefined.
                let show = !self.long_press_selection.get();
                if show {
                    *self.last_ime_epoch.borrow_mut() = None;
                    self.ime_states_seen.borrow_mut().clear();
                }
                (props, show)
            }
            InputMethodRequest::Update(props) => {
                if self.ime_client.borrow().is_none() {
                    return;
                }
                if self.ime_dispatching.get() {
                    *self.deferred_ime_update.borrow_mut() = Some(props);
                    return;
                }
                (props, false)
            }
            InputMethodRequest::Disable => {
                // Native focus loss (SAF picker / pause) is not a text edit.
                // Publishing an empty GameTextInput state here queues a full
                // replacement which can arrive after resume and erase the draft.
                // Detach the client before hiding; the next Enable resets the
                // session and publishes the new owner's actual text/selection.
                *self.ime_client.borrow_mut() = None;
                self.deferred_ime_update.take();
                self.forget_ime_session();
                self.app.hide_soft_input(true);
                return;
            }
            _ => return,
        };
        // RC26: game-activity bypasses set_imm_data, so its long-press ActionMode
        // never received selection geometry or the collapse/clear that finishes it.
        // Keep presentation in sync before the IME echo guards return; only the
        // long-press detector may open the menu, never an IME anchor/cursor report.
        self.java_helper
            .update_selection_menu(&props, self.window.scale_factor(), self.show_cursor_handles.get())
            .unwrap_or_else(|e| print_jni_error(&self.app, e));
        'push: {
        let mut text = props.text.to_string();
        if !props.preedit_text.is_empty() {
            text.insert_str(props.preedit_offset, props.preedit_text.as_str());
        }
        // Chatter patch: convert the cursor into MERGED-text coordinates.
        //
        // Slint documents `InputMethodProperties::text` as excluding the
        // pre-edit, and `cursor_position` as "the position of the cursor in
        // bytes within the `text`" — i.e. committed-text coordinates. We hand
        // GameTextInput the MERGED string (committed with the pre-edit spliced
        // in), so the offset has to be shifted past the pre-edit, which
        // upstream never does.
        //
        // Committed "a" + pre-edit "s" at offset 1 merges to "as", where the
        // caret belongs at 2 — upstream pushes 1, putting the caret BEFORE the
        // composing region. The IME then cannot extend the composition past two
        // characters and backspace deletes nothing, which is exactly the
        // reported "typing goes up 2 letters and clearing does nothing"
        // (OPEN-ITEMS §112n).
        let preedit_len = props.preedit_text.len();
        let to_merged = |pos: usize| {
            if preedit_len > 0 && pos >= props.preedit_offset { pos + preedit_len } else { pos }
        };
        let sel_start = to_merged(props.anchor_position.unwrap_or(props.cursor_position));
        let sel_end = to_merged(props.cursor_position);
        let compose = (!props.preedit_text.is_empty())
            .then_some((props.preedit_offset, props.preedit_offset + props.preedit_text.len()));

        // Chatter patch: a state that never reached the gate is owed: the
        // guards below describe a state the IME does not hold, so none of
        // them applies until a push lands (`publish_pending`).
        if !self.publication.publish_pending() {
        // Chatter patch: skip pushing an unchanged state back to GameTextInput.
        // Echoing the IME's own change triggers restartInput, which Slint then
        // sees as a change and echoes again — a feedback loop that restarts the
        // input connection thousands of times and drops keystrokes (#9240).
        // A new committed text means a real edit landed: start a fresh epoch.
        let committed = props.text.to_string();
        if self.last_ime_epoch.borrow().as_deref() != Some(committed.as_str()) {
            *self.last_ime_epoch.borrow_mut() = Some(committed);
            self.ime_states_seen.borrow_mut().clear();
        }

        // ...but NEVER dedupe a change to the visible text. The seen-set exists
        // to absorb the 1.17 oscillation, which churns cursor/compose while the
        // merged text stays identical. A real edit changes the merged text — and
        // BACKSPACE walks it back to a state already in the set, so deduping on
        // the full tuple silently swallowed every delete: typing "as" and
        // pressing backspace twice left "as" untouched on a Galaxy S9. That was
        // a regression introduced by this very patch (OPEN-ITEMS §112n).
        if self.last_pushed_text.borrow().as_deref() != Some(text.as_str()) {
            self.ime_states_seen.borrow_mut().clear();
        }

        // Chatter patch: reject the degenerate cursor-0 echo.
        //
        // Slint 1.17 reports each keystroke twice with two different splits of
        // the SAME visible text: once correctly (committed "a", preedit "s",
        // cursor 2) and once degenerate — committed empty, the whole string as
        // preedit at offset 0, cursor 0. The degenerate one arrives last, so
        // GameTextInput ends up with its cursor at 0 and the entire text inside
        // the compose region. Two user-visible consequences, both reported:
        //
        //   * the text renders as one highlighted block, and
        //   * BACKSPACE DOES NOTHING — logcat shows
        //     `gti.InputConnection: processKeyEvent(key=67) text=as`, i.e. the
        //     IME received KEYCODE_DEL and deleted nothing, because deleting
        //     backwards from offset 0 is a no-op.
        //
        // Skipping it is safe: the merged text is identical, so no character
        // can be lost — only the cursor regression is dropped (§112n).
        //
        // HONESTLY: this does NOT fix the reported symptoms. Instrumenting the
        // real sequence showed the degenerate split is SLINT'S OWN internal
        // state, not just what we forward — after a backspace Slint reports
        // `committed="" pre="as" off=0 cur=0`, so it renders the whole string
        // as one highlighted compose region and its editing logic never removes
        // a character. Suppressing the push keeps a bogus cursor out of the IME,
        // which is worth doing on its own, but the backend cannot repair what
        // Slint believes it holds. §112n stays open.
        if self.last_pushed_text.borrow().as_deref() == Some(text.as_str())
            && compose.is_some()
            && sel_end == 0
            && self.last_pushed_cursor.get() > 0
        {
            break 'push;
        }

        // Chatter patch: never echo the IME's own text back at it.
        //
        // Pressing SPACE makes the input method commit the composing word AND
        // insert the space, so GameTextInput's text becomes "as ". Slint then
        // reports committed="as" (without the space yet) and we pushed that
        // back — overwriting "as " with "as" and destroying the space. The user
        // saw this as space working sometimes and not others; instrumented, it
        // was every space in the run.
        //
        // The existing tuple dedupe cannot catch it, because the tuple differs
        // (compose region None vs Some) even though the TEXT is identical. If
        // the text matches what the IME last told us, the IME is already in
        // that state or ahead of it, and there is nothing to send (§112n).
        // ...but a caret move is NOT an echo. Tapping into existing text leaves
        // the merged string identical while the cursor changes, and returning
        // here left GameTextInput's cursor where it was: every later keystroke
        // landed at the old offset, so tapping mid-text and typing appended at
        // the end instead (§112o). Only suppress when the selection matches
        // too — and keep suppressing unconditionally while a composition is
        // live, which is where the space clobber above lives.
        if self.last_ime_text.borrow().as_deref() == Some(text.as_str())
            && (compose.is_some() || self.last_ime_selection.get() == (sel_start, sel_end))
        {
            break 'push;
        }

        // Slint's delayed notification of the caret `finish_composing` set,
        // arriving after the tap already moved it. Never push a caret the IME
        // has already moved past (§112o).
        if compose.is_none()
            && self.stale_commit_cursor.get() == Some(sel_end)
            && sel_start == sel_end
            && self.last_ime_text.borrow().as_deref() == Some(text.as_str())
            && self.last_ime_selection.get() != (sel_end, sel_end)
        {
            self.stale_commit_cursor.set(None);
            break 'push;
        }
        let key = (text.clone(), sel_start, sel_end, compose);
        if self.ime_states_seen.borrow().contains(&key) {
            break 'push;
        }
        self.ime_states_seen.borrow_mut().push(key);
        }
        self.had_compose.set(compose.is_some());
        *self.last_pushed_text.borrow_mut() = Some(text.clone());
        self.last_pushed_cursor.set(sel_end);

        self.set_game_text_input_state(TextInputState {
            text,
            selection: TextSpan { start: sel_start, end: sel_end },
            compose_region: compose.map(|(start, end)| TextSpan { start, end }),
        });
        }
        if show {
            self.app.show_soft_input(true);
        }
    }

    /// Modals and the conversation pane re-take focus when they close
    /// (`ui/app.slint`, the `changed active-modal` / `changed
    /// mobile-show-chat` handlers), which is exactly the moment a focused
    /// field may have been destroyed without a FocusOut (§112m).
    fn handle_focus_change(
        &self,
        old: Option<i_slint_core::item_tree::ItemRc>,
        new: Option<i_slint_core::item_tree::ItemRc>,
    ) {
        if old != new {
            self.long_press_selection.set(false);
            self.dismiss_selection_menu();
        }
        self.release_vanished_ime_client(true);
    }

    fn safe_area_inset(&self) -> PhysicalEdges {
        if self.fullscreen.get() {
            Default::default()
        } else {
            self.java_helper.get_safe_area().unwrap_or_else(|e| print_jni_error(&self.app, e))
        }
    }
}

impl AndroidWindowAdapter {
    /// A user may select the same range twice without changing the text.
    /// Historical IME echo guards must not suppress that new gesture.
    pub(crate) fn begin_selection_change(&self) {
        self.ime_states_seen.borrow_mut().clear();
        self.stale_commit_cursor.set(None);
    }

    fn dismiss_selection_menu(&self) {
        self.java_helper
            .hide_action_menu()
            .unwrap_or_else(|e| print_jni_error(&self.app, e));
    }

    /// Drop everything recorded about the current client's editing session.
    fn forget_ime_session(&self) {
        self.dismiss_selection_menu();
        // Chatter patch: a reset still unacknowledged belongs to the old
        // owner; its acknowledgement will not match the new one
        // (`handle_text_input_ack`), so the flag would otherwise keep
        // `finish_composing` inert for the next field.
        self.publication.forget();
        self.had_compose.set(false);
        *self.last_ime_text.borrow_mut() = None;
        self.last_ime_selection.set((0, 0));
        *self.pending_self_echo.borrow_mut() = None;
        self.stale_commit_cursor.set(None);
        *self.last_pushed_text.borrow_mut() = None;
        self.last_pushed_cursor.set(0);
        *self.last_ime_epoch.borrow_mut() = None;
        self.ime_states_seen.borrow_mut().clear();
    }

    /// Whether the TextInput the input method belongs to was destroyed
    /// without losing focus first (see `ime_client`).
    fn ime_client_vanished(&self) -> bool {
        self.ime_client.borrow().as_ref().is_some_and(|c| c.upgrade().is_none())
    }

    /// Forget a destroyed field's session, so nothing of it can be replayed
    /// into the next one (§112m). With `disable`, also do what Disable does
    /// for a live field, which Slint never sends for a destroyed one: empty
    /// GameTextInput — unconditionally, as no live field owns that text — and
    /// let `set_text_input_focused(false)` route through the Disable arm to
    /// hide the keyboard and clear the window's text-input-focused flag,
    /// which is otherwise left true.
    ///
    /// `disable` only from a completed focus change. From a touch or an
    /// inbound event the same batch may go on to focus a new field, and the
    /// IME's answer to an empty push could then land on it and wipe it.
    fn release_vanished_ime_client(&self, disable: bool) {
        use android_activity::input::{TextInputState, TextSpan};
        if !self.ime_client_vanished() {
            return;
        }
        *self.ime_client.borrow_mut() = None;
        self.forget_ime_session();
        if !disable {
            return;
        }
        self.set_game_text_input_state(TextInputState {
            text: String::new(),
            selection: TextSpan { start: 0, end: 0 },
            compose_region: None,
        });
        WindowInner::from_pub(&self.window).set_text_input_focused(false);
    }

    pub fn new(app: AndroidApp) -> Rc<Self> {
        let java_helper = JavaHelper::new(&app).unwrap_or_else(|e| print_jni_error(&app, e));
        Rc::<Self>::new_cyclic(|w| Self {
            app,
            window: Window::new(w.clone()),
            #[cfg(not(any(feature = "unstable-wgpu-28", feature = "unstable-wgpu-29")))]
            renderer: SkiaRenderer::default(&SkiaSharedContext::default()),
            #[cfg(all(feature = "unstable-wgpu-28", not(feature = "unstable-wgpu-29")))]
            renderer: SkiaRenderer::default_wgpu_28(&SkiaSharedContext::default()),
            #[cfg(feature = "unstable-wgpu-29")]
            renderer: SkiaRenderer::default_wgpu_29(&SkiaSharedContext::default()),
            requested_graphics_api: RefCell::new(None),
            event_queue: Default::default(),
            pending_redraw: Default::default(),
            last_motion: Default::default(),
            java_helper,
            fullscreen: Cell::new(false),
            offset: Default::default(),
            show_cursor_handles: Cell::new(false),
            long_press: RefCell::default(),
            long_press_selection: Cell::new(false),
            last_pressed_state: Cell::new(ButtonState(0)),
            last_ime_selection: Cell::new((0, 0)),
            pending_self_echo: RefCell::default(),
            stale_commit_cursor: Cell::new(None),
            last_ime_epoch: RefCell::default(),
            ime_states_seen: RefCell::default(),
            ime_input_type: Cell::new(None),
            had_compose: Cell::new(false),
            last_pushed_text: RefCell::default(),
            last_pushed_cursor: Cell::new(0),
            last_ime_text: RefCell::default(),
            ime_client: RefCell::default(),
            ime_dispatching: Cell::new(false),
            deferred_ime_update: RefCell::default(),
            native_held_modifiers: Cell::new([false; 8]),
            text_bridge: RefCell::default(),
            ime_owner: Cell::new(0),
            ime_generation: Cell::new(0),
            publication: Default::default(),
        })
    }

    pub fn process_event(&self, event: &PollEvent<'_>) -> Result<ControlFlow<()>, PlatformError> {
        let queue = std::mem::take(&mut *self.event_queue.lock().unwrap());
        for e in queue {
            match e {
                Event::Quit => return Ok(ControlFlow::Break(())),
                Event::Other(o) => o(),
            }
        }
        #[cfg_attr(slint_nightly_test, allow(non_exhaustive_omitted_patterns))]
        match event {
            PollEvent::Main(MainEvent::InputAvailable) => self.process_inputs()?,
            PollEvent::Main(MainEvent::InitWindow { .. }) => {
                if let Some(w) = self.app.native_window() {
                    let size = PhysicalSize { width: w.width() as u32, height: w.height() as u32 };

                    let scale_factor =
                        self.app.config().density().map(|dpi| dpi as f32 / 160.0).unwrap_or(1.0);

                    if (scale_factor - self.window.scale_factor()).abs() > f32::EPSILON {
                        self.window
                            .try_dispatch_event(WindowEvent::ScaleFactorChanged { scale_factor })?;
                    }

                    self.renderer.set_window_handle(
                        Arc::new(w),
                        Arc::new(DummyDisplayHandle),
                        size,
                        self.requested_graphics_api.borrow().clone(),
                    )?;
                    self.resize()?;
                    self.java_helper.window_focus_gained()
                        .unwrap_or_else(|e| print_jni_error(&self.app, e));

                    // Fixes a problem for old Android versions: the soft input always prompt out on startup.
                    #[cfg(feature = "native-activity")]
                    self.java_helper
                        .show_or_hide_soft_input(false)
                        .unwrap_or_else(|e| print_jni_error(&self.app, e));
                }
            }
            PollEvent::Main(
                MainEvent::WindowResized { .. } | MainEvent::ContentRectChanged { .. },
            ) => {
                self.resize()?;
                self.java_helper.reconcile_window_insets()
                    .unwrap_or_else(|e| print_jni_error(&self.app, e));
            }
            PollEvent::Main(MainEvent::RedrawNeeded { .. }) => {
                self.pending_redraw.set(false);
                self.do_render()?;
            }
            PollEvent::Main(MainEvent::GainedFocus) => {
                self.resize()?;
                self.java_helper.window_focus_gained()
                    .unwrap_or_else(|e| print_jni_error(&self.app, e));
                self.window.try_dispatch_event(WindowEvent::WindowActiveChanged(true))?;
            }
            PollEvent::Main(MainEvent::LostFocus) => {
                self.native_held_modifiers.set([false; 8]);
                self.long_press.take();
                self.long_press_selection.set(false);
                self.dismiss_selection_menu();
                self.window.try_dispatch_event(WindowEvent::WindowActiveChanged(false))?;
            }
            PollEvent::Main(MainEvent::ConfigChanged { .. }) => {
                let scale_factor =
                    self.app.config().density().map(|dpi| dpi as f32 / 160.0).unwrap_or(1.0);

                if (scale_factor - self.window.scale_factor()).abs() > f32::EPSILON {
                    self.window
                        .try_dispatch_event(WindowEvent::ScaleFactorChanged { scale_factor })?;
                    self.window.try_dispatch_event(WindowEvent::Resized {
                        size: self.size().to_logical(scale_factor),
                    })?;
                    WindowInner::from_pub(&self.window).set_window_item_safe_area(
                        self.internal(i_slint_core::InternalToken)
                            .map(|internal| internal.safe_area_inset().to_logical(scale_factor))
                            .unwrap_or_default(),
                    );
                }
            }
            PollEvent::Main(MainEvent::Destroy) => {
                return Ok(ControlFlow::Break(()));
            }
            _ => (),
        }
        Ok(ControlFlow::Continue(()))
    }

    /// Chatter patch: fold a live composition into the committed text.
    ///
    /// Slint splices the pre-edit into the text at `preedit_offset`, and it
    /// keeps that offset pinned to the caret. So moving the caret while the
    /// input method is composing RE-ORDERS the merged string we hand
    /// GameTextInput: on a Galaxy S9, "abc def" (with "def" composing) tapped
    /// just after the "a" became "adefbc" — characters moved, the space lost.
    /// Android's own contract is that a cursor move finishes composing
    /// (`finishComposingText`), so do that before the touch reaches Slint.
    /// See docs/OPEN-ITEMS.md §112o.
    fn finish_composing(&self) {
        use android_activity::input::{TextInputState, TextSpan};
        // Chatter patch: between an accepted send and the gate's
        // acknowledgement there is no composition of this field to finish;
        // what the cache held was consumed.
        if self.publication.reset_pending() {
            return;
        }
        // Use the last state the INPUT METHOD reported, not `last_pushed_text`:
        // every dedupe guard in the outbound path can return before updating
        // that, so it goes stale exactly when a composition is live.
        //
        // NOT `self.app.text_input_state()`, tempting as it is — that takes the
        // android_app lock, and this runs inside the input-iteration callback
        // which already holds it. Doing so aborted the process (SIGABRT in
        // `android_main`) on the first touch after a keystroke.
        let Some(text) = self.last_ime_text.borrow().clone() else { return };
        let cursor = self.last_ime_selection.get().1.min(text.len());
        // deliver_ime_state normally gives Slint the complete IME text as
        // committed text, even while the IME still marks a composing region.
        // Replacing identical text here fires TextInput.edited anyway. A tap
        // on a search result then clears/rebuilds its rows between touch Down
        // and Up, so the pressed row never receives a click (Samsung IME on
        // Android 10 keeps that composing region active for every word).
        // Only sync Slint when it actually has different text or preedit.
        let slint_needs_commit = self
            .ime_client
            .borrow()
            .as_ref()
            .and_then(|c| c.upgrade())
            .and_then(|c| c.downcast::<i_slint_core::items::TextInput>())
            .is_some_and(|field| {
                let field = field.as_pin_ref();
                field.text().as_str() != text.as_str() || !field.preedit_text().is_empty()
            });
        if slint_needs_commit {
            let mut key_event = KeyEvent::default();
            key_event.text = text.as_str().into();
            WindowInner::from_pub(&self.window).process_key_input(InternalKeyEvent {
                event_type: KeyEventType::CommitComposition,
                replacement_range: Some(i32::MIN..i32::MAX),
                // Deliberately no cursor: the tap that triggered this commit is
                // about to set the caret, and handing Slint the pre-tap offset here
                // made it reassert that offset a frame later, dragging the caret
                // back to the end of the text (§112o).
                cursor_position: None,
                anchor_position: None,
                key_event,
                ..Default::default()
            });
        }
        self.had_compose.set(false);
        *self.pending_self_echo.borrow_mut() = Some((text.clone(), cursor));
        self.stale_commit_cursor.set(Some(cursor));
        *self.last_ime_text.borrow_mut() = Some(text.clone());
        self.last_ime_selection.set((cursor, cursor));
        *self.last_pushed_text.borrow_mut() = Some(text.clone());
        self.ime_states_seen.borrow_mut().clear();
        self.set_game_text_input_state(TextInputState {
            text,
            selection: TextSpan { start: cursor, end: cursor },
            compose_region: None,
        });
    }

    /// Convert every outbound state, including finish_composing and clears,
    /// at the boundary. Echo guards and pending cursor state remain UTF-8.
    /// `false` only when a bound gate could not be reached; the state is then
    /// owed (`publish_pending`) and the next outbound Update re-publishes.
    fn set_game_text_input_state(
        &self,
        mut state: android_activity::input::TextInputState,
    ) -> bool {
        use android_activity::input::TextSpan;
        let (start, end) = crate::text_input_offsets::utf8_span_to_utf16(
            &state.text,
            (state.selection.start, state.selection.end),
        );
        state.selection = TextSpan { start, end };
        state.compose_region = state.compose_region.map(|span| {
            let (start, end) = crate::text_input_offsets::utf8_span_to_utf16(
                &state.text,
                (span.start, span.end),
            );
            TextSpan { start, end }
        });
        let bridge = self.text_bridge.borrow().clone();
        let reached = match bridge {
            Some(bridge) => bridge.push(&TextInputPush {
                text: state.text,
                selection: (state.selection.start, state.selection.end),
                compose: state.compose_region.map(|s| (s.start, s.end)),
                owner: self.ime_owner.get(),
                generation: self.ime_generation.get(),
            }),
            None => {
                self.app.set_text_input_state(state);
                true
            }
        };
        self.publication.pushed(reached);
        reached
    }

    pub(crate) fn set_text_input_bridge(&self, bridge: Rc<dyn TextInputBridge>) {
        *self.text_bridge.borrow_mut() = Some(bridge);
    }

    /// Chatter patch: an accepted send took the IME client's text; the app
    /// has cleared the field. Consume the IME's copy as of a new generation.
    ///
    /// The identity check is the field itself: the IME's client must be live
    /// and must now be empty, which is the field the app just took. Another
    /// field (a search box behind the GIF picker's send) is left alone; so is
    /// a field with no IME client, whose next Enable publishes its actual
    /// (empty) text anyway.
    ///
    /// Nothing waits on the UI thread. The gate empties the Editable before
    /// the IME's next operation or from a posted fallback, whichever comes
    /// first, and tags every state it reports with the generation applied by
    /// then; `handle_text_input_state` drops the older ones. The echo guards
    /// are reset to the state the IME will hold: Slint's render-time
    /// `Update("")` is then the IME's own state and is not pushed again.
    pub(crate) fn consume_text_input(&self) {
        let Some(client) = self.ime_client.borrow().as_ref().and_then(|c| c.upgrade()) else {
            return;
        };
        let Some(field) = client.downcast::<i_slint_core::items::TextInput>() else {
            return;
        };
        let field = field.as_pin_ref();
        // The composer is the one multi-line field a send consumes; a
        // single-line one (the GIF picker's search box behind an attachment
        // send) is never the consumed client, empty or not.
        if field.single_line() || !field.text().is_empty() || !field.preedit_text().is_empty() {
            return;
        }
        let generation = self.ime_generation.get() + 1;
        self.ime_generation.set(generation);
        self.publication.begin_reset();
        self.had_compose.set(false);
        *self.pending_self_echo.borrow_mut() = None;
        self.stale_commit_cursor.set(None);
        *self.last_ime_text.borrow_mut() = Some(String::new());
        self.last_ime_selection.set((0, 0));
        *self.last_pushed_text.borrow_mut() = Some(String::new());
        self.last_pushed_cursor.set(0);
        *self.last_ime_epoch.borrow_mut() = None;
        self.ime_states_seen.borrow_mut().clear();
        self.deferred_ime_update.take();
        let bridge = self.text_bridge.borrow().clone();
        match bridge {
            Some(bridge) => {
                if !bridge.consume(self.ime_owner.get(), generation) {
                    // Nothing reached the gate. The generation stays: the
                    // field was consumed whether or not the gate heard, and
                    // a snapshot of the consumed text must not come back as
                    // typing. The render's Update("") re-publishes it as a
                    // push under this generation (`publish_pending`), which
                    // the gate applies in place of the reset it never got.
                    self.publication.failed_consume();
                }
            }
            None => {
                use android_activity::input::{TextInputState, TextSpan};
                self.set_game_text_input_state(TextInputState {
                    text: String::new(),
                    selection: TextSpan { start: 0, end: 0 },
                    compose_region: None,
                });
            }
        }
    }

    /// Chatter patch: the gate applied the reset of generation `generation`
    /// for `owner` on the UI thread. Only the pending flag changes; the
    /// field's text is never touched here, so an acknowledgement arriving
    /// after a failed send restored the draft, or after new typing, changes
    /// nothing visible.
    pub(crate) fn handle_text_input_ack(&self, owner: u64, generation: u64) {
        if owner == self.ime_owner.get() && generation == self.ime_generation.get() {
            self.publication.clear_reset();
        }
    }

    /// Chatter patch: the app put text back into the IME client's field
    /// after `consume_text_input` (the draft an edit had held aside, a
    /// failed send's text). The reset emptied the IME's Editable and
    /// restarted input, and every gated IME operation after that publishes
    /// a snapshot of the Editable as it is then (GameTextInput 4.4.0
    /// `endBatchEdit` runs `stateUpdated` unconditionally), tagged with the
    /// consumed generation. The render-time push of the restored text
    /// carried that same generation, so a snapshot of the still-empty
    /// Editable landing after the restore was current by provenance and
    /// replaced the restored text with nothing (RC89, Galaxy S9: Save on an
    /// edit left the composer empty instead of the draft it had borrowed).
    ///
    /// The restore is a new app-initiated state of the field, so it gets the
    /// next generation and is pushed from here, not left to the render: the
    /// gate learns the generation even when Slint would not publish the text
    /// again. A snapshot the IME produced before this push applied is older
    /// and dropped; one produced after reflects the restored Editable. If a
    /// consumption is still unacknowledged, the gate applies this push in
    /// its place, so nothing of it stays pending here. Same identity check
    /// as the consumption, for a field that now holds text. No timer, focus
    /// change or content rule: provenance only.
    ///
    /// The same holds without a consumption: opening an edit replaces the
    /// draft of a composer that kept its IME client through the message
    /// menu, at the generation the IME still reports under. The render's
    /// push of the message text carried that generation too, so a snapshot
    /// of the old draft produced before that push applied was current by
    /// provenance and put the draft back under the edit banner (RC89,
    /// Galaxy S9, FUTO: Edit then typing at once extended the old draft).
    /// Every app-initiated replacement of the field's text goes through
    /// here, a clear included (the text the field holds is what is pushed).
    ///
    /// The caret goes to the end of the new text, on both sides, from here:
    /// Slint's caret is wherever the old text left it (clamped), and a
    /// keystroke the IME ran between a push of that caret and the
    /// composer's own move to the end (`changed edit-message-id`, next
    /// frame) would land mid-text under the new generation, accepted. With
    /// both carets at the end from the first push, that later move is an
    /// echo and pushes nothing.
    pub(crate) fn restore_text_input(self: &Rc<Self>) {
        use android_activity::input::{TextInputState, TextSpan};
        let Some(client) = self.ime_client.borrow().as_ref().and_then(|c| c.upgrade()) else {
            return;
        };
        let Some(field) = client.downcast::<i_slint_core::items::TextInput>() else {
            return;
        };
        let field = field.as_pin_ref();
        if field.single_line() || !field.preedit_text().is_empty() {
            return;
        }
        let text = field.text().to_string();
        let cursor = text.len();
        let anchor = cursor;
        let window_adapter = self.clone() as Rc<dyn WindowAdapter>;
        field.anchor_position_byte_offset.set(cursor as i32);
        field.set_cursor_position(
            cursor as i32,
            true,
            i_slint_core::items::TextChangeNotify::SkipCallbacks,
            &window_adapter,
            &client,
        );
        let generation = self.ime_generation.get() + 1;
        self.ime_generation.set(generation);
        self.publication.clear_reset();
        self.had_compose.set(false);
        *self.pending_self_echo.borrow_mut() = None;
        self.stale_commit_cursor.set(None);
        // The state the IME will hold: the render's own Update of the same
        // text and caret is then an echo and is not pushed again.
        self.note_app_state(&text, anchor, cursor);
        self.deferred_ime_update.take();
        // If this does not reach the gate the generation stays advanced and
        // `publish_pending` has the frame, input and Update paths re-publish
        // the field's state: it holds the new text whether or not the gate
        // heard, and a snapshot of the old text must not replace it.
        self.set_game_text_input_state(TextInputState {
            text,
            selection: TextSpan { start: anchor, end: cursor },
            compose_region: None,
        });
    }

    /// Chatter patch: the echo guards as of an app-initiated state of the
    /// field (`restore_text_input`, a re-publication of an owed state).
    fn note_app_state(&self, text: &str, anchor: usize, cursor: usize) {
        *self.last_ime_text.borrow_mut() = Some(text.to_string());
        self.last_ime_selection.set((anchor, cursor));
        *self.last_pushed_text.borrow_mut() = Some(text.to_string());
        self.last_pushed_cursor.set(cursor);
        *self.last_ime_epoch.borrow_mut() = Some(text.to_string());
        self.ime_states_seen.borrow_mut().clear();
        self.ime_states_seen.borrow_mut().push((text.to_string(), anchor, cursor, None));
    }

    /// Chatter patch: a state owed to the gate (`publish_pending`) is
    /// re-published from the paths the event loop runs anyway: a frame
    /// (`do_render`) and an input wake (`process_inputs`, before the input
    /// iterator takes the app lock), besides any outbound Update. Slint's
    /// render raises an Update only while the field's text differs from
    /// its own last published one, so after one failed retry an unchanged
    /// field would raise none. The field's current text and caret are what
    /// is pushed; nothing is requested (no redraw, no timer): a failed JNI
    /// call returns at once and the next frame or wake tries again, a
    /// push that lands clears the flag.
    fn republish_owed_text_input(&self) {
        use android_activity::input::{TextInputState, TextSpan};
        if !self.publication.publish_pending() {
            return;
        }
        let Some(client) = self.ime_client.borrow().as_ref().and_then(|c| c.upgrade()) else {
            return;
        };
        let Some(field) = client.downcast::<i_slint_core::items::TextInput>() else {
            return;
        };
        let field = field.as_pin_ref();
        if !field.preedit_text().is_empty() {
            return;
        }
        let text = field.text().to_string();
        let clamp = |offset: i32| (offset.max(0) as usize).min(text.len());
        let anchor = clamp(field.anchor_position_byte_offset());
        let cursor = clamp(field.cursor_position_byte_offset());
        if self.set_game_text_input_state(TextInputState {
            text: text.clone(),
            selection: TextSpan { start: anchor, end: cursor },
            compose_region: None,
        }) {
            self.note_app_state(&text, anchor, cursor);
        }
    }

    /// Chatter patch: a state the IME produced, as the gate tagged it. Dropped
    /// by provenance when it is another field's or predates the consumption
    /// applied since (its content is never consulted); otherwise it is the
    /// IME's current state and is delivered like any other.
    pub(crate) fn handle_text_input_state(&self, state: InboundTextInput) {
        use android_activity::input::{TextInputState, TextSpan};
        if state.owner != self.ime_owner.get() || state.generation < self.ime_generation.get() {
            return;
        }
        let (start, end) =
            crate::text_input_offsets::utf16_span_to_utf8(&state.text, state.selection);
        let compose_region = state.compose.map(|span| {
            let (start, end) = crate::text_input_offsets::utf16_span_to_utf8(&state.text, span);
            TextSpan { start, end }
        });
        self.deliver_ime_state(TextInputState {
            text: state.text,
            selection: TextSpan { start, end },
            compose_region,
        });
    }

    fn try_dispatch_key_event(&self, ev: WindowEvent) -> KeyEventResult {
        self.long_press.take();
        self.dismiss_selection_menu();
        #[cfg_attr(slint_nightly_test, allow(non_exhaustive_omitted_patterns))]
        match ev {
            WindowEvent::KeyPressed { text } => WindowInner::from_pub(&self.window)
                .process_key_input(InternalKeyEvent {
                    event_type: KeyEventType::KeyPressed,
                    key_event: {
                        let mut key_event = KeyEvent::default();
                        key_event.text = text;
                        key_event
                    },
                    ..Default::default()
                }),
            WindowEvent::KeyPressRepeated { text } => WindowInner::from_pub(&self.window)
                .process_key_input(InternalKeyEvent {
                    event_type: KeyEventType::KeyPressed,
                    key_event: {
                        let mut key_event = KeyEvent::default();
                        key_event.text = text;
                        key_event.repeat = true;
                        key_event
                    },
                    ..Default::default()
                }),
            WindowEvent::KeyReleased { text } => WindowInner::from_pub(&self.window)
                .process_key_input(InternalKeyEvent {
                    key_event: {
                        let mut key_event = KeyEvent::default();
                        key_event.text = text;
                        key_event
                    },
                    event_type: KeyEventType::KeyReleased,
                    ..Default::default()
                }),
            _ => KeyEventResult::EventIgnored,
        }
    }

    // Android IMEs can send Ctrl+letter with metaState only, without separate
    // modifier key events. Core derives modifiers from key transitions, ignoring
    // KeyEvent.modifiers, so bracket this native event with those transitions.
    fn dispatch_native_key_event(
        &self,
        native: &android_activity::input::KeyEvent,
    ) -> KeyEventResult {
        let Some(event) = map_key_event(native) else {
            return KeyEventResult::EventIgnored;
        };
        let previous_held = self.native_held_modifiers.get();
        let mut held = previous_held;
        let modifier = match native.key_code() {
            Keycode::CtrlLeft => Some(0),
            Keycode::CtrlRight => Some(1),
            Keycode::ShiftLeft => Some(2),
            Keycode::ShiftRight => Some(3),
            Keycode::AltLeft => Some(4),
            Keycode::AltRight => Some(5),
            Keycode::MetaLeft => Some(6),
            Keycode::MetaRight => Some(7),
            _ => None,
        };
        if let Some(index) = modifier {
            held[index] = native.action() != KeyAction::Up;
            self.native_held_modifiers.set(held);
        }
        let command = matches!(
            native.key_code(),
            Keycode::Copy | Keycode::Cut | Keycode::Paste
        );
        let modifiers = native_key_modifiers(native.meta_state(), command);
        for (index, (key, pressed)) in modifiers.into_iter().enumerate() {
            // The actual modifier key below supplies its own transition.
            if modifier == Some(index) || pressed == previous_held[index] {
                continue;
            }
            let mut key_event = KeyEvent::default();
            key_event.text = key.into();
            WindowInner::from_pub(&self.window).process_key_input(InternalKeyEvent {
                event_type: if pressed {
                    KeyEventType::KeyPressed
                } else {
                    KeyEventType::KeyReleased
                },
                key_event,
                ..Default::default()
            });
        }
        let result = self.try_dispatch_key_event(event);
        // Do not let a one-shot IME command leave Ctrl held for TextAction or
        // later events. Keep actual modifier key transitions held until Up.
        for (index, ((key, temporary), pressed)) in modifiers.into_iter().zip(held).enumerate() {
            if modifier == Some(index) || temporary == pressed {
                continue;
            }
            let mut key_event = KeyEvent::default();
            key_event.text = key.into();
            WindowInner::from_pub(&self.window).process_key_input(InternalKeyEvent {
                event_type: if pressed {
                    KeyEventType::KeyPressed
                } else {
                    KeyEventType::KeyReleased
                },
                key_event,
                ..Default::default()
            });
        }
        result
    }

    fn process_inputs(&self) -> Result<(), PlatformError> {
        // Chatter patch: before the app lock, which the gate's UI thread
        // takes inside `onTextInputEventNative`.
        self.republish_owed_text_input();
        let mut iter =
            self.app.input_events_iter().map_err(|e| PlatformError::Other(e.to_string()))?;
        loop {
            let mut result = Ok(());
            let read_input = iter.next(|event| match event {
                InputEvent::KeyEvent(key_event) => {
                    if self.dispatch_native_key_event(key_event) == KeyEventResult::EventAccepted {
                        InputStatus::Handled
                    } else {
                        InputStatus::Unhandled
                    }
                }
                InputEvent::MotionEvent(motion_event) => {
                    let offset = self.offset.get();
                    let scale = self.window.scale_factor();
                    let touch_pos = |p: &android_activity::input::Pointer<'_>| {
                        i_slint_core::lengths::logical_point_from_api(pointer_logical_position(
                            p.x(),
                            p.y(),
                            offset,
                            scale,
                        ))
                    };
                    match motion_event.action() {
                        MotionAction::ButtonPress => {
                            self.dismiss_selection_menu();
                            result = self.window.try_dispatch_event(WindowEvent::PointerPressed {
                                position: position_for_event(motion_event, offset, scale),
                                button: button_for_event(motion_event, &self.last_pressed_state),
                            });
                            InputStatus::Handled
                        }
                        MotionAction::ButtonRelease => {
                            result = self.window.try_dispatch_event(WindowEvent::PointerReleased {
                                position: position_for_event(motion_event, offset, scale),
                                button: button_for_event(motion_event, &self.last_pressed_state),
                            });
                            InputStatus::Handled
                        }
                        MotionAction::Down => {
                            self.long_press_selection.set(false);
                            self.dismiss_selection_menu();
                            let position = position_for_event(motion_event, offset, scale);

                            // A field destroyed since the last focus change
                            // must be released first, or `finish_composing`
                            // replays its text into the field being tapped
                            // (§112m).
                            self.release_vanished_ime_client(false);
                            // Chatter patch: a tap must not drag the composing
                            // region along with the caret (§112o).
                            if self.had_compose.get() {
                                self.finish_composing();
                            }

                            self.show_cursor_handles.set(true);
                            let _timer = Timer::default();
                            _timer.start(
                                TimerMode::SingleShot,
                                self.java_helper
                                    .long_press_timeout()
                                    .unwrap_or_else(|e| print_jni_error(&self.app, e)),
                                long_press_timeout,
                            );
                            self.long_press.replace(Some(LongPressDetection { position, _timer }));
                            if let Some(p) = motion_event.pointers().next() {
                                WindowInner::from_pub(&self.window).process_touch_input(
                                    p.pointer_id(),
                                    touch_pos(&p),
                                    TouchPhase::Started,
                                );
                            }
                            InputStatus::Handled
                        }
                        MotionAction::Up => {
                            self.long_press.take();
                            if let Some(p) = motion_event.pointers().next() {
                                WindowInner::from_pub(&self.window).process_touch_input(
                                    p.pointer_id(),
                                    touch_pos(&p),
                                    TouchPhase::Ended,
                                );
                            }
                            self.long_press_selection.set(false);
                            InputStatus::Handled
                        }
                        MotionAction::Move => {
                            // Slint still considers this a single-click drag.
                            // A move after select_word would shrink the selection
                            // back to the finger's byte offset; use the handles
                            // to extend a completed long-press selection instead.
                            if self.long_press_selection.get() {
                                return InputStatus::Handled;
                            }
                            let position = position_for_event(motion_event, offset, scale);

                            let mut lp = self.long_press.borrow_mut();
                            let sq = |x| x * x;
                            if lp.as_ref().map_or(false, |lp| {
                                sq(lp.position.x - position.x) + sq(lp.position.y - position.y)
                                    > 100.
                            }) {
                                *lp = None;
                            }
                            drop(lp);

                            self.last_motion.set(Some(std::time::Instant::now()));
                            let runtime_window = WindowInner::from_pub(&self.window);
                            for p in motion_event.pointers() {
                                runtime_window.process_touch_input(
                                    p.pointer_id(),
                                    touch_pos(&p),
                                    TouchPhase::Moved,
                                );
                            }
                            InputStatus::Handled
                        }
                        MotionAction::PointerDown => {
                            // A second finger means no long-press.
                            self.long_press.take();
                            let idx = motion_event.pointer_index();
                            if let Some(p) = motion_event.pointers().nth(idx) {
                                WindowInner::from_pub(&self.window).process_touch_input(
                                    p.pointer_id(),
                                    touch_pos(&p),
                                    TouchPhase::Started,
                                );
                            }
                            InputStatus::Handled
                        }
                        MotionAction::PointerUp => {
                            let idx = motion_event.pointer_index();
                            if let Some(p) = motion_event.pointers().nth(idx) {
                                WindowInner::from_pub(&self.window).process_touch_input(
                                    p.pointer_id(),
                                    touch_pos(&p),
                                    TouchPhase::Ended,
                                );
                            }
                            InputStatus::Handled
                        }
                        MotionAction::HoverMove => {
                            let position = position_for_event(motion_event, offset, scale);
                            let window_event = WindowEvent::PointerMoved { position };
                            result = self.window.try_dispatch_event(window_event);
                            InputStatus::Handled
                        }
                        MotionAction::Cancel | MotionAction::Outside => {
                            self.long_press.take();
                            self.long_press_selection.set(false);
                            let runtime_window = WindowInner::from_pub(&self.window);
                            for p in motion_event.pointers() {
                                runtime_window.process_touch_input(
                                    p.pointer_id(),
                                    touch_pos(&p),
                                    TouchPhase::Cancelled,
                                );
                            }
                            InputStatus::Handled
                        }
                        MotionAction::Scroll => todo!(),
                        MotionAction::HoverEnter | MotionAction::HoverExit => {
                            InputStatus::Unhandled
                        }
                        _ => InputStatus::Unhandled,
                    }
                }
                InputEvent::TextEvent(state) => {
                    // Chatter patch: with the producer gate bound, states
                    // arrive tagged through `handle_text_input_state`; this
                    // is the same state untagged, a wake-up only.
                    if self.text_bridge.borrow().is_some() {
                        return InputStatus::Handled;
                    }
                    // android-activity decodes the string, but leaves Java UTF-16
                    // span offsets unchanged. Everything below uses UTF-8 bytes.
                    let mut state = state.clone();
                    let (start, end) = crate::text_input_offsets::utf16_span_to_utf8(
                        &state.text,
                        (state.selection.start, state.selection.end),
                    );
                    state.selection = android_activity::input::TextSpan { start, end };
                    state.compose_region = state.compose_region.map(|span| {
                        let (start, end) = crate::text_input_offsets::utf16_span_to_utf8(
                            &state.text,
                            (span.start, span.end),
                        );
                        android_activity::input::TextSpan { start, end }
                    });
                    self.deliver_ime_state(state);
                    InputStatus::Handled
                }
                // The IME's action key (Done/Go/Search on a single-line field)
                // arrives from GameActivity as a TextAction, not a KeyEvent, so
                // it was dropped: TextInput `accepted` never fired on Android
                // (login, PIN, search-by-handle). It is the Return of a
                // single-line field, so deliver it as one.
                InputEvent::TextAction(_) => {
                    let text: SharedString = Key::Return.into();
                    self.try_dispatch_key_event(WindowEvent::KeyPressed { text: text.clone() });
                    self.try_dispatch_key_event(WindowEvent::KeyReleased { text });
                    InputStatus::Handled
                }
                _ => InputStatus::Unhandled,
            });

            result?;

            if !read_input {
                return Ok(());
            }
        }
    }

    /// Deliver a state the IME holds (UTF-8 offsets) into Slint, through the
    /// echo guards. Shared by the gate path and android-activity's own
    /// `TextEvent` (used only without a bridge).
    fn deliver_ime_state(&self, state: android_activity::input::TextInputState) {
        // No live field owns the input method: this is the IME
        // still reporting on a field that was destroyed (or a
        // late report after it was released). Delivering it would
        // hand the dead field's text to whatever is focused next
        // (§112m); recording it would poison the echo guards.
        if self.ime_client.borrow().is_none() || self.ime_client_vanished() {
            self.release_vanished_ime_client(false);
            return;
        }
        // Drop GameTextInput's echo of our own finish_composing
        // push. It carries the pre-tap cursor and would undo the
        // caret move the tap just made (§112o).
        let self_echo = self
            .pending_self_echo
            .borrow()
            .as_ref()
            .is_some_and(|(t, c)| {
                t == &state.text
                    && state.selection.start == *c
                    && state.selection.end == *c
                    && state.compose_region.is_none()
            });
        if self_echo {
            *self.pending_self_echo.borrow_mut() = None;
            return;
        }
        // A selected-state echo after long-press is not typing. A
        // changed string, composition or collapsed caret is.
        if state.compose_region.is_some()
            || state.selection.start == state.selection.end
            || (self.last_ime_text.borrow().as_ref() != Some(&state.text)
                && self.last_pushed_text.borrow().as_ref() != Some(&state.text))
        {
            self.show_cursor_handles.set(false);
            self.dismiss_selection_menu();
        }
        // Remember what the IME believes, so the outbound path can
        // tell an app-initiated change from an echo of this (§112n).
        *self.last_ime_text.borrow_mut() = Some(state.text.clone());
        self.last_ime_selection
            .set((state.selection.start, state.selection.end));
        // The IME's own report is authoritative about whether a
        // composition is live; the outbound flag goes stale because
        // the dedupe guards return before setting it (§112o).
        self.had_compose.set(state.compose_region.is_some());
        let runtime_window = WindowInner::from_pub(&self.window);
        // Chatter patch: hand Slint the IME's text as plain
        // committed text, never as a pre-edit — the composing word
        // must look like normal text with a caret.
        //
        // i-slint-core 1.17.1 paints a TextInput's whole pre-edit
        // range with the selection colours whatever
        // `preedit_selection` says (textlayout/sharedparley.rs:1417
        // prefers `preedit_range` over `selection_range`), so ANY
        // composition Slint holds is drawn as a teal selection until
        // the IME commits it. Samsung Keyboard (S9, Android 10)
        // composes every word, re-marking already-committed text with
        // `setComposingRegion` (logcat `gti.InputConnection:
        // setComposingRegion: 6:12` beside `setSelection: n:n`, a
        // collapsed caret) — a composition, not a selection — which is
        // the reported S9 highlight. The S25/Gboard "pass" was typed with
        // `adb input text`, which commits without composing, so it
        // never exercised this path.
        //
        // This also removes the old reasons to reject a mid-text
        // composition: Slint pinned the pre-edit offset to the end of
        // the committed text ("abc def" recomposing (0,3) read
        // " defabc", §112o). With no pre-edit there is nothing to
        // misplace, so the IME may keep composing anywhere.
        //
        // The IME keeps its composition: the outbound path pushes
        // nothing back while Slint's text and selection equal what
        // the IME just reported (the `last_ime_text` guard), and any
        // real app-side change (send, clear) is pushed with no compose
        // region, which ends the composition as it should.
        //
        // A real user selection (long-press) is a non-empty
        // `state.selection` with no composition and still reaches
        // Slint as anchor..cursor below.
        let mut key_event = KeyEvent::default();
        key_event.text = state.text.as_str().into();
        let event = InternalKeyEvent {
            event_type: KeyEventType::CommitComposition,
            replacement_range: Some(i32::MIN..i32::MAX),
            cursor_position: Some(state.selection.end as _),
            anchor_position: Some(state.selection.start as _),
            key_event,
            ..Default::default()
        };
        self.ime_dispatching.set(true);
        runtime_window.process_key_input(event);
        self.ime_dispatching.set(false);
        if let Some(props) = self.deferred_ime_update.take() {
            i_slint_core::window::WindowAdapterInternal::input_method_request(
                self,
                InputMethodRequest::Update(props),
            );
        }
    }

    fn resize(&self) -> Result<(), PlatformError> {
        let Some(win) = self.app.native_window() else { return Ok(()) };
        let size = PhysicalSize { width: win.width() as u32, height: win.height() as u32 };

        let scale_factor = self.window.scale_factor();
        self.window
            .try_dispatch_event(WindowEvent::Resized { size: size.to_logical(scale_factor) })?;
        WindowInner::from_pub(&self.window).set_window_item_safe_area(
            self.internal(i_slint_core::InternalToken)
                .map(|internal| internal.safe_area_inset().to_logical(scale_factor))
                .unwrap_or_default(),
        );
        self.offset.set(Default::default());
        Ok(())
    }

    /// Chatter patch: reports whether a frame reached the screen, so the event
    /// loop can tell a presented frame from one that was skipped (no native
    /// window, an occluded or timed-out surface) and pace itself either way.
    pub fn do_render(&self) -> Result<bool, PlatformError> {
        self.republish_owed_text_input();
        let Some(win) = self.app.native_window() else { return Ok(false) };
        let o = self.offset.get();
        let outcome = self.renderer.render_transformed_with_post_callback(
            0.,
            (o.x as f32, o.y as f32),
            PhysicalSize { width: win.width() as _, height: win.height() as _ },
            None,
        )?;
        Ok(matches!(outcome, i_slint_core::renderer::DrawOutcome::Success))
    }

    pub fn set_requested_graphics_api(
        &self,
        requested_graphics_api: Option<i_slint_core::graphics::RequestedGraphicsAPI>,
    ) {
        *self.requested_graphics_api.borrow_mut() = requested_graphics_api;
    }

    pub(super) fn update_window_insets(
        &self,
        authoritative_geometry: bool,
        window_origin: PhysicalPosition,
        window_size: PhysicalSize,
        safe_area: PhysicalEdges,
        keyboard: PhysicalEdges,
    ) {
        crate::inset_dispatch::publish_window_insets(
            &self.window, authoritative_geometry, window_origin, window_size, safe_area, keyboard,
        );
    }
}

fn long_press_timeout() {
    let Some(adaptor) = CURRENT_WINDOW.with_borrow(|x| x.upgrade()) else { return };
    let Some(current) = adaptor.long_press.take() else { return };
    if let Some(focus_item) =
        i_slint_core::window::WindowInner::from_pub(&adaptor.window).focus_item.borrow().upgrade()
    {
        if let Some(text_input) = focus_item.downcast::<i_slint_core::items::TextInput>() {
            let text_input = text_input.as_pin_ref();
            let geometry = focus_item
                .geometry()
                .translate(focus_item.map_to_window(Default::default()).to_vector());
            if !geometry.contains(i_slint_core::lengths::logical_point_from_api(current.position)) {
                return;
            };
            let window_adapter = adaptor.clone() as Rc<dyn WindowAdapter>;
            let position = i_slint_core::lengths::logical_point_from_api(current.position)
                - geometry.origin.to_vector();
            let cursor = text_input.byte_offset_for_position(position, &window_adapter, &focus_item);
            let text = text_input.text();
            if text.is_empty() || text.as_bytes().get(cursor) == Some(&b'\n') {
                return;
            }
            // Tiny moves before the timeout may already have made a partial
            // selection. Start at the held point, then expand BOTH boundaries.
            text_input.anchor_position_byte_offset.set(cursor as i32);
            text_input.set_cursor_position(
                cursor as i32,
                true,
                i_slint_core::items::TextChangeNotify::SkipCallbacks,
                &window_adapter,
                &focus_item,
            );
            adaptor.show_cursor_handles.set(true);
            adaptor.begin_selection_change();
            text_input.select_word(&window_adapter, &focus_item);
            let (cursor, anchor) = text_input.selection_anchor_and_cursor();
            if cursor == anchor {
                return;
            }
            adaptor.long_press_selection.set(true);
            adaptor
                .java_helper
                .show_action_menu()
                .unwrap_or_else(|e| print_jni_error(&adaptor.app, e))
        }
    };
}

/// Convert raw pointer coordinates to a LogicalPosition, applying the
/// display offset and scale factor.
fn pointer_logical_position(
    x: f32,
    y: f32,
    offset: PhysicalPosition,
    scale_factor: f32,
) -> LogicalPosition {
    PhysicalPosition::new(x as i32 - offset.x, y as i32 - offset.y).to_logical(scale_factor)
}

fn position_for_event(
    motion_event: &MotionEvent,
    offset: PhysicalPosition,
    scale: f32,
) -> LogicalPosition {
    motion_event
        .pointers()
        .next()
        .map_or_else(Default::default, |p| pointer_logical_position(p.x(), p.y(), offset, scale))
}

fn button_for_event(
    motion_event: &MotionEvent,
    last_pressed_cell: &Cell<ButtonState>,
) -> PointerEventButton {
    //
    // The motion_event API has a method called action_button() which can be used to directly
    // determine the button associated with the event. However, the disadvantage of using the
    // action_button() API is that it relies on NDK 33 or higher, which implies that the output
    // application will only run on Android 13 or higher.
    //
    // This functionally equivalent method of computing the action button relies on the
    // button_state() call from the motion event, rather than action_button(). It is a bit more
    // complex than using action_button() directly, since the previous button state must be
    // tracked and used in the calculation for computing which button was toggled. However, this
    // will run on Android 12 (and possibly lower).
    //
    // See here for further discussion:
    //
    // https://stackoverflow.com/questions/75718566/amotionevent-getbuttonstate-returns-0-for-every-button-during-mouse-button-relea
    //
    let cur_pressed_state = motion_event.button_state();
    let last_pressed_state = last_pressed_cell.get();
    #[cfg_attr(slint_nightly_test, allow(non_exhaustive_omitted_patterns))]
    let toggled = match motion_event.action() {
        MotionAction::ButtonPress => {
            last_pressed_cell.set(cur_pressed_state);
            ButtonState((last_pressed_state.0 ^ cur_pressed_state.0) & cur_pressed_state.0)
        }
        MotionAction::ButtonRelease => {
            last_pressed_cell.set(cur_pressed_state);
            ButtonState((last_pressed_state.0 ^ cur_pressed_state.0) & last_pressed_state.0)
        }
        _ => ButtonState(0),
    };

    // if multiple buttons toggled, primary takes precedence, then secondary, etc.
    if toggled.primary() {
        return PointerEventButton::Left;
    }
    if toggled.secondary() {
        return PointerEventButton::Right;
    }
    if toggled.teriary() {
        return PointerEventButton::Middle;
    }
    if toggled.back() {
        return PointerEventButton::Back;
    }
    if toggled.forward() {
        return PointerEventButton::Forward;
    }
    return PointerEventButton::Other;
}

// Dedicated Android clipboard keys carry no Ctrl meta bit. Translate them to
// the same core shortcuts as Ctrl+C/X/V, irrespective of incidental modifiers.
fn native_key_modifiers(
    meta: android_activity::input::MetaState,
    command: bool,
) -> [(Key, bool); 8] {
    [
        (
            Key::Control,
            command || meta.ctrl_on() || meta.ctrl_left_on(),
        ),
        (Key::ControlR, !command && meta.ctrl_right_on()),
        (
            Key::Shift,
            !command && (meta.shift_on() || meta.shift_left_on()),
        ),
        (Key::ShiftR, !command && meta.shift_right_on()),
        (
            Key::Alt,
            // AltGr remains separate from public Alt, matching physical AltRight.
            !command && (meta.alt_left_on() || (meta.alt_on() && !meta.alt_right_on())),
        ),
        (Key::AltGr, !command && meta.alt_right_on()),
        (
            Key::Meta,
            !command && (meta.meta_on() || meta.meta_left_on()),
        ),
        (Key::MetaR, !command && meta.meta_right_on()),
    ]
}

fn map_key_event(key_event: &android_activity::input::KeyEvent) -> Option<WindowEvent> {
    let text = map_key_code(key_event.key_code())?;
    let repeat = key_event.repeat_count() > 0;
    match key_event.action() {
        KeyAction::Down if repeat => Some(WindowEvent::KeyPressRepeated { text }),
        KeyAction::Down => Some(WindowEvent::KeyPressed { text }),
        KeyAction::Up => Some(WindowEvent::KeyReleased { text }),
        KeyAction::Multiple if repeat => Some(WindowEvent::KeyPressRepeated { text }),
        KeyAction::Multiple => Some(WindowEvent::KeyPressed { text }),
        _ => None,
    }
}

fn map_key_code(code: android_activity::input::Keycode) -> Option<SharedString> {
    match code {
        Keycode::Unknown => None,
        Keycode::SoftLeft => None,
        Keycode::SoftRight => None,
        Keycode::Home => None,
        Keycode::Back => Some(Key::Back.into()),
        Keycode::Call => None,
        Keycode::Endcall => None,
        Keycode::Keycode0 => Some("0".into()),
        Keycode::Keycode1 => Some("1".into()),
        Keycode::Keycode2 => Some("2".into()),
        Keycode::Keycode3 => Some("3".into()),
        Keycode::Keycode4 => Some("4".into()),
        Keycode::Keycode5 => Some("5".into()),
        Keycode::Keycode6 => Some("6".into()),
        Keycode::Keycode7 => Some("7".into()),
        Keycode::Keycode8 => Some("8".into()),
        Keycode::Keycode9 => Some("9".into()),
        Keycode::Star => Some("*".into()),
        Keycode::Pound => Some("#".into()),
        Keycode::DpadUp => Some(Key::UpArrow.into()),
        Keycode::DpadDown => Some(Key::DownArrow.into()),
        Keycode::DpadLeft => Some(Key::LeftArrow.into()),
        Keycode::DpadRight => Some(Key::RightArrow.into()),
        Keycode::DpadCenter => Some(Key::Return.into()),
        Keycode::VolumeUp => None,
        Keycode::VolumeDown => None,
        Keycode::Power => None,
        Keycode::Camera => None,
        Keycode::Clear => None,
        Keycode::A => Some("a".into()),
        Keycode::B => Some("b".into()),
        Keycode::C => Some("c".into()),
        Keycode::D => Some("d".into()),
        Keycode::E => Some("e".into()),
        Keycode::F => Some("f".into()),
        Keycode::G => Some("g".into()),
        Keycode::H => Some("h".into()),
        Keycode::I => Some("i".into()),
        Keycode::J => Some("j".into()),
        Keycode::K => Some("k".into()),
        Keycode::L => Some("l".into()),
        Keycode::M => Some("m".into()),
        Keycode::N => Some("n".into()),
        Keycode::O => Some("o".into()),
        Keycode::P => Some("p".into()),
        Keycode::Q => Some("q".into()),
        Keycode::R => Some("r".into()),
        Keycode::S => Some("s".into()),
        Keycode::T => Some("t".into()),
        Keycode::U => Some("u".into()),
        Keycode::V => Some("v".into()),
        Keycode::W => Some("w".into()),
        Keycode::X => Some("x".into()),
        Keycode::Y => Some("y".into()),
        Keycode::Z => Some("z".into()),
        Keycode::Comma => Some(",".into()),
        Keycode::Period => Some(".".into()),
        Keycode::AltLeft => Some(Key::Alt.into()),
        Keycode::AltRight => Some(Key::AltGr.into()),
        Keycode::ShiftLeft => Some(Key::Shift.into()),
        Keycode::ShiftRight => Some(Key::ShiftR.into()),
        Keycode::Tab => Some("\t".into()),
        Keycode::Space => Some(" ".into()),
        Keycode::Sym => None,
        Keycode::Explorer => None,
        Keycode::Envelope => None,
        Keycode::Enter => Some(Key::Return.into()),
        Keycode::Del => Some(Key::Backspace.into()),
        Keycode::Grave => Some("`".into()),
        Keycode::Minus => Some("-".into()),
        Keycode::Equals => Some("=".into()),
        Keycode::LeftBracket => Some("[".into()),
        Keycode::RightBracket => Some("]".into()),
        Keycode::Backslash => Some("\\".into()),
        Keycode::Semicolon => Some(";".into()),
        Keycode::Apostrophe => Some("'".into()),
        Keycode::Slash => Some("/".into()),
        Keycode::At => Some("@".into()),
        Keycode::Num => None,
        Keycode::Headsethook => None,
        Keycode::Focus => None,
        Keycode::Plus => Some("+".into()),
        Keycode::Menu => Some(Key::Menu.into()),
        Keycode::Notification => None,
        Keycode::Search => None,
        Keycode::MediaPlayPause => None,
        Keycode::MediaStop => None,
        Keycode::MediaNext => None,
        Keycode::MediaPrevious => None,
        Keycode::MediaRewind => None,
        Keycode::MediaFastForward => None,
        Keycode::Mute => None,
        Keycode::PageUp => Some(Key::PageUp.into()),
        Keycode::PageDown => Some(Key::PageDown.into()),
        Keycode::Pictsymbols => None,
        Keycode::SwitchCharset => None,
        Keycode::ButtonA => None,
        Keycode::ButtonB => None,
        Keycode::ButtonC => None,
        Keycode::ButtonX => None,
        Keycode::ButtonY => None,
        Keycode::ButtonZ => None,
        Keycode::ButtonL1 => None,
        Keycode::ButtonR1 => None,
        Keycode::ButtonL2 => None,
        Keycode::ButtonR2 => None,
        Keycode::ButtonThumbl => None,
        Keycode::ButtonThumbr => None,
        Keycode::ButtonStart => None,
        Keycode::ButtonSelect => None,
        Keycode::ButtonMode => None,
        Keycode::Escape => Some(Key::Escape.into()),
        Keycode::ForwardDel => Some(Key::Delete.into()),
        Keycode::CtrlLeft => Some(Key::Control.into()),
        Keycode::CtrlRight => Some(Key::ControlR.into()),
        Keycode::CapsLock => None,
        Keycode::ScrollLock => Some(Key::ScrollLock.into()),
        Keycode::MetaLeft => Some(Key::Meta.into()),
        Keycode::MetaRight => Some(Key::MetaR.into()),
        Keycode::Function => None,
        Keycode::Sysrq => Some(Key::SysReq.into()),
        Keycode::Break => None,
        Keycode::MoveHome => Some(Key::Home.into()),
        Keycode::MoveEnd => Some(Key::End.into()),
        Keycode::Insert => Some(Key::Insert.into()),
        Keycode::Forward => None,
        Keycode::MediaPlay => None,
        Keycode::MediaPause => None,
        Keycode::MediaClose => None,
        Keycode::MediaEject => None,
        Keycode::MediaRecord => None,
        Keycode::F1 => Some(Key::F1.into()),
        Keycode::F2 => Some(Key::F2.into()),
        Keycode::F3 => Some(Key::F3.into()),
        Keycode::F4 => Some(Key::F4.into()),
        Keycode::F5 => Some(Key::F5.into()),
        Keycode::F6 => Some(Key::F6.into()),
        Keycode::F7 => Some(Key::F7.into()),
        Keycode::F8 => Some(Key::F8.into()),
        Keycode::F9 => Some(Key::F9.into()),
        Keycode::F10 => Some(Key::F10.into()),
        Keycode::F11 => Some(Key::F11.into()),
        Keycode::F12 => Some(Key::F12.into()),
        Keycode::NumLock => None,
        Keycode::Numpad0 => Some("0".into()),
        Keycode::Numpad1 => Some("1".into()),
        Keycode::Numpad2 => Some("2".into()),
        Keycode::Numpad3 => Some("3".into()),
        Keycode::Numpad4 => Some("4".into()),
        Keycode::Numpad5 => Some("5".into()),
        Keycode::Numpad6 => Some("6".into()),
        Keycode::Numpad7 => Some("7".into()),
        Keycode::Numpad8 => Some("8".into()),
        Keycode::Numpad9 => Some("9".into()),
        Keycode::NumpadDivide => Some("/".into()),
        Keycode::NumpadMultiply => Some("*".into()),
        Keycode::NumpadSubtract => Some("-".into()),
        Keycode::NumpadAdd => Some("+".into()),
        Keycode::NumpadDot => Some(".".into()),
        Keycode::NumpadComma => Some(",".into()),
        Keycode::NumpadEnter => Some("\n".into()),
        Keycode::NumpadEquals => Some("=".into()),
        Keycode::NumpadLeftParen => Some("(".into()),
        Keycode::NumpadRightParen => Some(")".into()),
        Keycode::VolumeMute => None,
        Keycode::Info => None,
        Keycode::ChannelUp => None,
        Keycode::ChannelDown => None,
        Keycode::ZoomIn => None,
        Keycode::ZoomOut => None,
        Keycode::Tv => None,
        Keycode::Window => None,
        Keycode::Guide => None,
        Keycode::Dvr => None,
        Keycode::Bookmark => None,
        Keycode::Captions => None,
        Keycode::Settings => None,
        Keycode::TvPower => None,
        Keycode::TvInput => None,
        Keycode::StbPower => None,
        Keycode::StbInput => None,
        Keycode::AvrPower => None,
        Keycode::AvrInput => None,
        Keycode::ProgRed => None,
        Keycode::ProgGreen => None,
        Keycode::ProgYellow => None,
        Keycode::ProgBlue => None,
        Keycode::AppSwitch => None,
        Keycode::Button1 => None,
        Keycode::Button2 => None,
        Keycode::Button3 => None,
        Keycode::Button4 => None,
        Keycode::Button5 => None,
        Keycode::Button6 => None,
        Keycode::Button7 => None,
        Keycode::Button8 => None,
        Keycode::Button9 => None,
        Keycode::Button10 => None,
        Keycode::Button11 => None,
        Keycode::Button12 => None,
        Keycode::Button13 => None,
        Keycode::Button14 => None,
        Keycode::Button15 => None,
        Keycode::Button16 => None,
        Keycode::LanguageSwitch => None,
        Keycode::MannerMode => None,
        Keycode::Keycode3dMode => None,
        Keycode::Contacts => None,
        Keycode::Calendar => None,
        Keycode::Music => None,
        Keycode::Calculator => None,
        Keycode::ZenkakuHankaku => None,
        Keycode::Eisu => None,
        Keycode::Muhenkan => None,
        Keycode::Henkan => None,
        Keycode::KatakanaHiragana => None,
        Keycode::Yen => None,
        Keycode::Ro => None,
        Keycode::Kana => None,
        Keycode::Assist => None,
        Keycode::BrightnessDown => None,
        Keycode::BrightnessUp => None,
        Keycode::MediaAudioTrack => None,
        Keycode::Sleep => None,
        Keycode::Wakeup => None,
        Keycode::Pairing => None,
        Keycode::MediaTopMenu => None,
        Keycode::Keycode11 => None,
        Keycode::Keycode12 => None,
        Keycode::LastChannel => None,
        Keycode::TvDataService => None,
        Keycode::VoiceAssist => None,
        Keycode::TvRadioService => None,
        Keycode::TvTeletext => None,
        Keycode::TvNumberEntry => None,
        Keycode::TvTerrestrialAnalog => None,
        Keycode::TvTerrestrialDigital => None,
        Keycode::TvSatellite => None,
        Keycode::TvSatelliteBs => None,
        Keycode::TvSatelliteCs => None,
        Keycode::TvSatelliteService => None,
        Keycode::TvNetwork => None,
        Keycode::TvAntennaCable => None,
        Keycode::TvInputHdmi1 => None,
        Keycode::TvInputHdmi2 => None,
        Keycode::TvInputHdmi3 => None,
        Keycode::TvInputHdmi4 => None,
        Keycode::TvInputComposite1 => None,
        Keycode::TvInputComposite2 => None,
        Keycode::TvInputComponent1 => None,
        Keycode::TvInputComponent2 => None,
        Keycode::TvInputVga1 => None,
        Keycode::TvAudioDescription => None,
        Keycode::TvAudioDescriptionMixUp => None,
        Keycode::TvAudioDescriptionMixDown => None,
        Keycode::TvZoomMode => None,
        Keycode::TvContentsMenu => None,
        Keycode::TvMediaContextMenu => None,
        Keycode::TvTimerProgramming => None,
        Keycode::Help => None,
        Keycode::NavigatePrevious => None,
        Keycode::NavigateNext => None,
        Keycode::NavigateIn => None,
        Keycode::NavigateOut => None,
        Keycode::StemPrimary => None,
        Keycode::Stem1 => None,
        Keycode::Stem2 => None,
        Keycode::Stem3 => None,
        Keycode::DpadUpLeft => None,
        Keycode::DpadDownLeft => None,
        Keycode::DpadUpRight => None,
        Keycode::DpadDownRight => None,
        Keycode::MediaSkipForward => None,
        Keycode::MediaSkipBackward => None,
        Keycode::MediaStepForward => None,
        Keycode::MediaStepBackward => None,
        Keycode::SoftSleep => None,
        Keycode::Cut => Some("x".into()),
        Keycode::Copy => Some("c".into()),
        Keycode::Paste => Some("v".into()),
        Keycode::SystemNavigationUp => None,
        Keycode::SystemNavigationDown => None,
        Keycode::SystemNavigationLeft => None,
        Keycode::SystemNavigationRight => None,
        Keycode::AllApps => None,
        Keycode::Refresh => None,
        Keycode::ThumbsUp => None,
        Keycode::ThumbsDown => None,
        Keycode::ProfileSwitch => None,
        _ => None,
    }
}

/// A dummy display handle that's Send + Sync. Required by wgpu, but harmless as
/// raw_window_handel::AndroidDisplayHandle is an empty struct.
struct DummyDisplayHandle;
impl raw_window_handle::HasDisplayHandle for DummyDisplayHandle {
    fn display_handle(
        &self,
    ) -> Result<raw_window_handle::DisplayHandle<'_>, raw_window_handle::HandleError> {
        Ok(raw_window_handle::DisplayHandle::android())
    }
}

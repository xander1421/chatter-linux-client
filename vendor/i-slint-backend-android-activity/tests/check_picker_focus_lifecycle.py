#!/usr/bin/env python3
"""Execute the production GameActivity Disable and unowned-input guard on a
recording Android API seam. No Cargo, SDK, device, services, or application data.
This proves the native focus-loss boundary, not OEM asynchronous event ordering.
"""
from pathlib import Path
import subprocess
import tempfile

backend = Path(__file__).resolve().parents[1]
source = (backend / "androidwindowadapter.rs").read_text()
start = source.index("            InputMethodRequest::Disable => {", source.index('#[cfg(not(feature = "native-activity"))]'))
end = source.index("            _ => return,", start)
disable = source[start:end]
start = source.index("                    // No live field")
end = source.index("                    // Drop GameTextInput", start)
inbound_guard = source[start:end]
start = source.index("            InputMethodRequest::Update(props) => {", source.index('#[cfg(not(feature = "native-activity"))]'))
end = source.index("            InputMethodRequest::Disable => {", start)
update = source[start:end]

harness = r'''
use std::cell::{Cell, RefCell};
#[derive(Clone, Debug, PartialEq)]
pub struct TextSpan { start: usize, end: usize }
#[derive(Clone, Debug, PartialEq)]
pub struct TextInputState { text: String, selection: TextSpan, compose_region: Option<TextSpan> }
mod android_activity { pub mod input { pub use crate::{TextInputState, TextSpan}; } }
enum InputMethodRequest { Disable, Update(()), Other }
#[derive(Debug, PartialEq)]
enum InputStatus { Handled }
#[derive(Default)]
struct RecordingApp { hidden: Cell<usize>, native: RefCell<Option<TextInputState>>, pushes: Cell<usize> }
impl RecordingApp {
    fn hide_soft_input(&self, _: bool) { self.hidden.set(self.hidden.get() + 1); }
    fn set_text_input_state(&self, state: TextInputState) {
        self.pushes.set(self.pushes.get() + 1);
        *self.native.borrow_mut() = Some(state);
    }
}
#[derive(Default)]
struct AndroidWindowAdapter {
    app: RecordingApp,
    ime_client: RefCell<Option<usize>>,
    deferred_ime_update: RefCell<Option<()>>,
    had_compose: Cell<bool>,
    last_ime_epoch: RefCell<Option<()>>,
    ime_states_seen: RefCell<Vec<()>>,
    forgets: Cell<usize>,
    delivered: RefCell<Vec<TextInputState>>,
    ime_dispatching: Cell<bool>,
    updates: Cell<usize>,
}
impl AndroidWindowAdapter {
    fn disable(&self) {
        use android_activity::input::{TextInputState, TextSpan};
        match InputMethodRequest::Disable {
DISABLE
            _ => return,
        }
    }
    fn update(&self) {
        let _props = match InputMethodRequest::Update(()) {
UPDATE
            _ => return,
        };
        self.updates.set(self.updates.get() + 1);
    }
    fn forget_ime_session(&self) {
        self.had_compose.set(false);
        self.forgets.set(self.forgets.get() + 1);
    }
    fn dismiss_selection_menu(&self) {}
    fn set_game_text_input_state(&self, state: TextInputState) { self.app.set_text_input_state(state); }
    fn ime_client_vanished(&self) -> bool { false }
    fn release_vanished_ime_client(&self, _: bool) {}
    fn incoming(&self, state: TextInputState) -> InputStatus {
GUARD
        self.delivered.borrow_mut().push(state);
        InputStatus::Handled
    }
}
#[test]
fn picker_cancel_selection_and_pause_do_not_publish_a_draft_erase() {
    for composing in [true, false] {
        for selection in [(2, 2), (6, 2), (2, 6)] {
            let adapter = AndroidWindowAdapter::default();
            let draft = TextInputState {
                text: "ab😀cd".into(),
                selection: TextSpan { start: selection.0, end: selection.1 },
                compose_region: composing.then_some(TextSpan { start: 2, end: 6 }),
            };
            *adapter.app.native.borrow_mut() = Some(draft.clone());
            *adapter.ime_client.borrow_mut() = Some(42);
            *adapter.deferred_ime_update.borrow_mut() = Some(());
            adapter.had_compose.set(composing);
            adapter.disable();
            assert_eq!(adapter.app.pushes.get(), 0, "FocusOut must not queue empty full-text replacement");
            assert_eq!(*adapter.app.native.borrow(), Some(draft));
            assert!(adapter.ime_client.borrow().is_none(), "released field still owns inbound IME events");
            assert!(adapter.deferred_ime_update.borrow().is_none());
            assert_eq!(adapter.app.hidden.get(), 1);
            adapter.update(); // delayed core notification after FocusOut
            assert_eq!(adapter.updates.get(), 0);
            // GameTextInput may still deliver an empty/old report while the
            // picker is open. The real ownership guard must swallow it.
            adapter.incoming(TextInputState {
                text: String::new(), selection: TextSpan { start: 0, end: 0 }, compose_region: None,
            });
            assert!(adapter.delivered.borrow().is_empty());
            adapter.disable(); // repeated pause/disable remains harmless
            assert_eq!(adapter.app.pushes.get(), 0);
        }
    }
}
'''
harness = harness.replace("DISABLE", disable).replace("GUARD", inbound_guard).replace("UPDATE", update)
with tempfile.TemporaryDirectory(prefix="chatter-picker-focus-") as tmp:
    tmp = Path(tmp)
    test = tmp / "picker_focus.rs"
    test.write_text(harness)
    executable = tmp / "picker_focus"
    subprocess.run(["rustc", "--edition=2024", "--test", "-Awarnings", str(test), "-o", str(executable)], check=True)
    subprocess.run([str(executable), "--nocapture"], check=True)

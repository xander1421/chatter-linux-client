#!/usr/bin/env python3
"""Run the production GameActivity offset boundary on a host Rust API seam.

No Cargo/Android build. Uses the installed i-slint-common Unicode utilities;
pass SLINT_UNICODE_UTILS if they are stored outside the default Cargo registry.
This covers conversion/wiring, not an OEM IME or its asynchronous lifecycle.
"""
import json
import os
from pathlib import Path
import subprocess
import tempfile


backend = Path(__file__).resolve().parents[1]
source = (backend / "androidwindowadapter.rs").read_text()
outbound = source[source.index("    fn set_game_text_input_state("):
                  source.index("    pub(crate) fn set_text_input_bridge(")]
incoming_start = source.index("                    let mut state = state.clone();")
incoming = source[incoming_start:source.index("                    self.deliver_ime_state(state);", incoming_start)]
unicode_path = os.environ.get("SLINT_UNICODE_UTILS")
if unicode_path is None:
    candidates = sorted((Path.home() / ".cargo/registry/src").glob(
        "*/i-slint-common-1.17.1/unicode_utils.rs"))
    if not candidates:
        raise SystemExit("Set SLINT_UNICODE_UTILS to the installed i-slint-common unicode_utils.rs")
    unicode_path = str(candidates[0])

harness = r'''
extern crate self as i_slint_common;
#[path = UNICODE_PATH]
pub mod unicode_utils;
#[path = OFFSETS_PATH]
mod text_input_offsets;
mod android_activity {
    pub mod input {
        #[derive(Clone, Copy, Debug, PartialEq)]
        pub struct TextSpan { pub start: usize, pub end: usize }
        #[derive(Clone, Debug, PartialEq)]
        pub struct TextInputState {
            pub text: String,
            pub selection: TextSpan,
            pub compose_region: Option<TextSpan>,
        }
    }
}
use android_activity::input::{TextInputState, TextSpan};
#[derive(Default)]
struct RecordingApp(std::cell::RefCell<Option<TextInputState>>);
impl RecordingApp {
    fn set_text_input_state(&self, state: TextInputState) { *self.0.borrow_mut() = Some(state); }
}
#[derive(Clone, Debug, PartialEq)]
struct TextInputPush {
    text: String, selection: (usize,usize), compose: Option<(usize,usize)>, owner: u64, generation: u64,
}
trait TextInputBridge { fn push(&self, state: &TextInputPush) -> bool; }
#[derive(Default)]
struct Publication;
impl Publication { fn pushed(&self, _: bool) {} }
#[derive(Default)]
struct AndroidWindowAdapter {
    app: RecordingApp,
    text_bridge: std::cell::RefCell<Option<std::rc::Rc<dyn TextInputBridge>>>,
    ime_owner: std::cell::Cell<u64>,
    ime_generation: std::cell::Cell<u64>,
    publication: Publication,
}
impl AndroidWindowAdapter {
OUTBOUND
}
fn inbound(state: &TextInputState) -> TextInputState {
INCOMING
    state
}
#[test]
fn real_boundary_preserves_text_selection_and_native_composition() {
    let adapter = AndroidWindowAdapter::default();
    for (text, selection, compose, byte_selection, byte_compose) in [
        ("а", (1,1), Some((0,1)), (2,2), Some((0,2))),
        ("ап", (2,2), Some((0,2)), (4,4), Some((0,4))),
        ("апр", (3,3), Some((0,3)), (6,6), Some((0,6))),
        ("ап", (2,2), Some((0,2)), (4,4), Some((0,4))),
        ("а", (1,1), Some((0,1)), (2,2), Some((0,2))),
        ("", (0,0), None, (0,0), None),
        ("а😀e\u{301}中z", (6,1), Some((1,5)), (12,2), Some((2,9))),
        ("abc", (3,1), None, (3,1), None),
    ] {
        let span = |(start,end)| TextSpan { start, end };
        let native = TextInputState {
            text: text.into(), selection: span(selection), compose_region: compose.map(span),
        };
        let core = inbound(&native);
        assert_eq!(core.text, native.text);
        assert_eq!(core.selection, span(byte_selection));
        assert_eq!(core.compose_region, byte_compose.map(span));
        assert!(core.text.is_char_boundary(core.selection.end));
        adapter.set_game_text_input_state(core);
        assert_eq!(adapter.app.0.borrow().as_ref(), Some(&native));
    }
}
'''.replace("UNICODE_PATH", json.dumps(unicode_path)).replace(
    "OFFSETS_PATH", json.dumps(str(backend / "text_input_offsets.rs"))
).replace("OUTBOUND", outbound).replace("INCOMING", incoming)

with tempfile.TemporaryDirectory(prefix="chatter-ime-offsets-") as temporary:
    root = Path(temporary)
    (root / "offsets.rs").write_text(harness)
    subprocess.run(["rustc", "--edition=2021", "--test", str(root / "offsets.rs"),
                    "-o", str(root / "check")], check=True)
    subprocess.run([str(root / "check")], check=True)

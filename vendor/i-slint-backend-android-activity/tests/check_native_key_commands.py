#!/usr/bin/env python3
"""Execute production native mapping/dispatch and core shortcut classification.

rustc + javac only; recording seams do not access clipboard/device/application
state and do not claim full TextInput, JNI, or OEM IME execution coverage.
"""
from pathlib import Path
import re
import subprocess
import tempfile
import argparse

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--baseline", action="store_true", help="extract pre-fix 60d79d0d^ production mapper/plain dispatch and Java accessor")
parser.add_argument("--evidence-dir", type=Path, help="retain generated executable sources and output")
parser.add_argument("--clipboard-ref", help="optional production Java revision for privacy negative control")
args = parser.parse_args()

backend = Path(__file__).resolve().parents[1]
def production(path):
    if args.baseline:
        return subprocess.check_output(['git', 'show', '60d79d0d^:vendor/i-slint-backend-android-activity/' + path], cwd=backend, text=True)
    return (backend / path).read_text()

# The baseline wrapper composes the actual old process_inputs key branch:
# map_key_event -> try_dispatch_key_event. It does not emulate the new bracketing.
source = production('androidwindowadapter.rs')
core = (backend.parent / 'i-slint-core/input.rs').read_text()


def function(text, marker):
    start = text.index(marker)
    opening = text.index('{', start)
    depth = 1
    end = opening + 1
    while depth:
        depth += (text[end] == '{') - (text[end] == '}')
        end += 1
    return text[start:end]


if args.baseline:
    assert 'InputEvent::KeyEvent(key_event) => match map_key_event(key_event)' in source
    assert 'self.try_dispatch_key_event(ev)' in source
mapping = function(source, 'fn map_key_code(')
keys = sorted(set(re.findall(r'Key::(\w+)', source)) | {'Control','ControlR','Shift','ShiftR','Alt','AltGr','Meta','MetaR'})
codes = sorted(set(re.findall(r'Keycode::(\w+)', mapping)))
# Distinct private-use values are sufficient for the API seam; modifier tracking
# below uses the production state_update body with the same named constants.
constants = '\n'.join(f'pub const {k}: char = \'\\u{{{0xf700+i:x}}}\';' for i, k in enumerate(keys))
state_update = function(core, 'pub(crate) fn state_update(')
modifier_methods = '\n'.join(function(core, 'pub fn ' + name + '(') for name in ['shift', 'alt', 'meta', 'control', 'modifiers_for'])
state_struct = core[core.index('pub(crate) struct InternalKeyboardModifierState'):core.index('impl InternalKeyboardModifierState')]
shortcut = function(core, 'pub fn shortcut(')
standard = function(core, 'pub enum StandardShortcut')
harness = r'''
use std::cell::{Cell, RefCell};
type SharedString = String;
#[derive(Clone, Copy, Debug)]
enum Key { KEYS }
impl From<Key> for String { fn from(k: Key) -> Self { match k { KEY_ARMS } } }
mod key_codes { CONSTANTS }
#[derive(Clone, Copy, Debug)]
enum Keycode { CODES }
#[derive(Clone, Copy, PartialEq)]
enum KeyAction { Down, Up, Multiple, Other }
#[derive(Clone, Copy)]
struct MetaState(u32);
impl MetaState { META_METHODS }
struct NativeKey { code: Keycode, action: KeyAction, meta: MetaState, repeat: i32 }
impl NativeKey {
    fn key_code(&self) -> Keycode { self.code }
    fn action(&self) -> KeyAction { self.action }
    fn meta_state(&self) -> MetaState { self.meta }
    fn repeat_count(&self) -> i32 { self.repeat }
}
mod android_activity { pub mod input { pub(crate) use crate::{NativeKey as KeyEvent, Keycode, MetaState}; } }
#[derive(Default, Clone, Copy, Debug)]
struct KeyboardModifiers { control: bool, shift: bool, alt: bool, meta: bool }
#[derive(Default)]
struct KeyEvent { text: String, repeat: bool, modifiers: KeyboardModifiers }
#[derive(Default, PartialEq)]
enum KeyEventType { #[default] KeyPressed, KeyReleased }
#[derive(Default)]
struct InternalKeyEvent { key_event: KeyEvent, event_type: KeyEventType }
impl InternalKeyEvent { SHORTCUT }
STANDARD
#[derive(Clone, Copy, Default)]
STATE_STRUCT
impl InternalKeyboardModifierState { STATE_UPDATE MODIFIER_METHODS }
#[derive(Debug, PartialEq)]
enum KeyEventResult { EventAccepted, EventIgnored }
enum WindowEvent { WindowActiveChanged(bool), KeyPressed { text: String }, KeyPressRepeated { text: String }, KeyReleased { text: String }, Other }
#[derive(Default)]
struct Window { state: Cell<InternalKeyboardModifierState>, received: RefCell<Vec<(String, bool, KeyboardModifiers, Option<StandardShortcut>, bool)>> }
struct WindowInner;
impl WindowInner {
    fn from_pub(w: &Window) -> &Window { w }
}
impl Window {
    fn try_dispatch_event(&self, e: WindowEvent) -> Result<(), ()> {
        if let WindowEvent::WindowActiveChanged(false) = e { CORE_FOCUS_RESET }
        Ok(())
    }
    fn process_key_input(&self, mut e: InternalKeyEvent) -> KeyEventResult {
        if let Some(s) = self.state.get().state_update(e.event_type == KeyEventType::KeyPressed, &e.key_event.text) { self.state.set(s); }
        e.key_event.modifiers = self.state.get().modifiers_for(&e);
        let shortcut = e.shortcut();
        let accepted = shortcut.is_some() && e.event_type == KeyEventType::KeyPressed;
        self.received.borrow_mut().push((e.key_event.text.clone(), e.key_event.repeat, e.key_event.modifiers, shortcut, e.event_type == KeyEventType::KeyPressed));
        if accepted { KeyEventResult::EventAccepted } else { KeyEventResult::EventIgnored }
    }
}
#[derive(Default)]
struct AndroidWindowAdapter { window: Window, long_press: Cell<Option<()>>, long_press_selection: Cell<bool>, native_held_modifiers: Cell<[bool;8]> }
impl AndroidWindowAdapter {
    fn dismiss_selection_menu(&self) {}
    fn lose_focus(&self) -> Result<(), ()> { LOST_FOCUS Ok(()) }
    TRY_DISPATCH
    NATIVE_DISPATCH
}
MODIFIERS
MAP_EVENT
MAP_CODE
fn key(code: Keycode, meta: u32, action: KeyAction, repeat: i32) -> NativeKey { NativeKey { code, meta: MetaState(meta), action, repeat } }
#[test]
fn meta_only_gboard_commands_and_dedicated_clipboard_keys() {
    for (code, text) in [(Keycode::A,"a"),(Keycode::C,"c"),(Keycode::X,"x"),(Keycode::V,"v")] {
        for meta in [0x1000, 0x2000, 0x4000] { // aggregate, left-only, right-only Ctrl
            let a = AndroidWindowAdapter::default();
            assert_eq!(a.dispatch_native_key_event(&key(code,meta,KeyAction::Down,0)), KeyEventResult::EventAccepted);
            let records = a.window.received.borrow();
            let e = records.iter().find(|e| e.0 == text).unwrap();
            let expected = match text { "a" => StandardShortcut::SelectAll, "c" => StandardShortcut::Copy, "x" => StandardShortcut::Cut, _ => StandardShortcut::Paste };
            assert!(e.2.control && e.4);
            assert_eq!(format!("{:?}",e.3), format!("{:?}",Some(expected)));
            assert_eq!(records.len(), 3, "one modifier press + real key + modifier release");
            let s = a.window.state.get();
            assert!(!s.left_control && !s.right_control);
        }
    }
    for (code,text) in [(Keycode::Copy,"c"),(Keycode::Cut,"x"),(Keycode::Paste,"v")] {
        let a = AndroidWindowAdapter::default();
        a.dispatch_native_key_event(&key(code,0x70003,KeyAction::Down,0));
        assert!(a.window.received.borrow().iter().any(|e| e.0 == text && e.2.control && !e.2.shift && !e.2.alt && !e.2.meta && matches!(e.3, Some(StandardShortcut::Copy)) == (text == "c") && matches!(e.3, Some(StandardShortcut::Cut)) == (text == "x") && matches!(e.3, Some(StandardShortcut::Paste)) == (text == "v")));
        a.dispatch_native_key_event(&key(Keycode::V,0,KeyAction::Down,0));
        let records = a.window.received.borrow();
        let e = records.iter().rev().find(|e| e.0 == "v").unwrap();
        assert!(!e.2.control && e.3.is_none()); // a command must not make subsequent typing Ctrl+V
    }
}
#[test]
fn actions_repeat_modifier_snapshot_and_ordinary_typing() {
    for action in [KeyAction::Down,KeyAction::Multiple,KeyAction::Up] {
        let a = AndroidWindowAdapter::default();
        a.dispatch_native_key_event(&key(Keycode::V,0x1000,action,2));
        let records = a.window.received.borrow();
        let e = records.iter().find(|e| e.0 == "v").unwrap();
        assert_eq!(e.4, !matches!(action,KeyAction::Up));
        assert_eq!(e.1, !matches!(action,KeyAction::Up));
    }
    let physical = AndroidWindowAdapter::default();
    physical.dispatch_native_key_event(&key(Keycode::CtrlLeft,0x1000,KeyAction::Down,0));
    assert!(physical.window.state.get().left_control);
    physical.dispatch_native_key_event(&key(Keycode::C,0x1000,KeyAction::Down,0));
    assert!(physical.window.state.get().left_control);
    physical.dispatch_native_key_event(&key(Keycode::CtrlLeft,0,KeyAction::Up,0));
    assert!(!physical.window.state.get().left_control);
    let a = AndroidWindowAdapter::default();
    a.dispatch_native_key_event(&key(Keycode::ShiftLeft,1,KeyAction::Down,0));
    assert!(a.window.state.get().left_shift);
    a.dispatch_native_key_event(&key(Keycode::Paste,0,KeyAction::Down,0));
    assert!(a.window.state.get().left_shift);
    a.dispatch_native_key_event(&key(Keycode::ShiftLeft,0,KeyAction::Up,0));
    assert!(!a.window.state.get().left_shift);
    a.dispatch_native_key_event(&key(Keycode::V,0,KeyAction::Down,0));
    assert!(a.window.received.borrow().iter().any(|e| e.0 == "v" && !e.2.control && e.3.is_none()));
    let plain = AndroidWindowAdapter::default();
    plain.dispatch_native_key_event(&key(Keycode::V,0,KeyAction::Down,0));
    assert_eq!(plain.window.received.borrow().len(),1, "ordinary input must not emit synthetic modifiers");
    assert!(map_key_event(&key(Keycode::V,0,KeyAction::Other,0)).is_none());
    assert!(map_key_event(&key(Keycode::Unknown,0,KeyAction::Down,0)).is_none());
    for bit in [1, 0x40, 0x80] { assert!(native_key_modifiers(MetaState(bit),false).iter().any(|(k,v)| matches!(k,Key::Shift|Key::ShiftR) && *v)); }
    for bit in [2, 0x10, 0x20] { assert!(native_key_modifiers(MetaState(bit),false).iter().any(|(k,v)| matches!(k,Key::Alt|Key::AltGr) && *v)); }
    for bit in [0x10000, 0x20000, 0x40000] { assert!(native_key_modifiers(MetaState(bit),false).iter().any(|(k,v)| matches!(k,Key::Meta|Key::MetaR) && *v)); }
}
'''
harness += r'''
#[test]
fn exact_ctrl_v_shortcut_one_press_release_none() {
    let a = AndroidWindowAdapter::default();
    a.dispatch_native_key_event(&key(Keycode::V,0x1000,KeyAction::Down,0));
    { let r=a.window.received.borrow();
      assert_eq!(r.iter().filter(|e| e.4 && matches!(e.3,Some(StandardShortcut::Paste))).count(),1,"exact Ctrl+V Paste operation at recording seam"); }
    a.dispatch_native_key_event(&key(Keycode::V,0x1000,KeyAction::Up,0));
    assert_eq!(a.window.received.borrow().iter().filter(|e| e.4 && matches!(e.3,Some(StandardShortcut::Paste))).count(),1,"release adds no operation");
}
#[test]
fn exact_dedicated_paste_shortcut_one_press_release_none() {
    let a = AndroidWindowAdapter::default();
    a.dispatch_native_key_event(&key(Keycode::Paste,0x70003,KeyAction::Down,0));
    assert_eq!(a.window.received.borrow().iter().filter(|e| e.4 && matches!(e.3,Some(StandardShortcut::Paste))).count(),1,"exact dedicatedPaste operation at recording seam");
    a.dispatch_native_key_event(&key(Keycode::Paste,0x70003,KeyAction::Up,0));
    assert_eq!(a.window.received.borrow().iter().filter(|e| e.4 && matches!(e.3,Some(StandardShortcut::Paste))).count(),1,"release adds no operation");
}
#[test]
fn held_left_right_altgr_and_extracted_focus_cleanup() {
    for (code,bit,index) in [(Keycode::CtrlLeft,0x3000,0),(Keycode::CtrlRight,0x5000,1),(Keycode::ShiftLeft,0x41,2),(Keycode::ShiftRight,0x81,3),(Keycode::AltLeft,0x12,4),(Keycode::AltRight,0x22,5),(Keycode::MetaLeft,0x30000,6),(Keycode::MetaRight,0x50000,7)] {
        let a=AndroidWindowAdapter::default();
        a.dispatch_native_key_event(&key(code,bit,KeyAction::Down,0));
        assert!(a.native_held_modifiers.get()[index]);
        if index==5 { assert!(a.window.state.get().altgr); assert!(!a.window.state.get().left_alt && !a.window.state.get().alt()); }
        a.dispatch_native_key_event(&key(Keycode::Paste,0,KeyAction::Down,0));
        assert!(a.native_held_modifiers.get()[index]);
        let state=a.window.state.get();
        assert!([state.left_control,state.right_control,state.left_shift,state.right_shift,state.left_alt,state.altgr,state.left_meta,state.right_meta][index]);
        a.dispatch_native_key_event(&key(code,0,KeyAction::Up,0));
        assert!(!a.native_held_modifiers.get()[index]);
        a.dispatch_native_key_event(&key(code,bit,KeyAction::Down,0));
        a.lose_focus().unwrap();
        assert_eq!(a.native_held_modifiers.get(),[false;8]);
        let state=a.window.state.get();
        assert!(!state.control() && !state.shift() && !state.alt() && !state.meta() && !state.altgr);
        a.dispatch_native_key_event(&key(Keycode::V,0,KeyAction::Down,0));
        assert!(a.window.received.borrow().iter().rev().find(|e|e.0=="v").unwrap().3.is_none());
    }
    let both=AndroidWindowAdapter::default();
    both.dispatch_native_key_event(&key(Keycode::V,0x32,KeyAction::Down,0));
    assert!(both.window.received.borrow().iter().find(|e|e.0=="v").unwrap().2.alt,"explicit left Alt still sets public Alt alongside AltGr");
    for bits in [0x20,0x22] {
        let a=AndroidWindowAdapter::default();
        a.dispatch_native_key_event(&key(Keycode::V,bits,KeyAction::Down,0));
        let r=a.window.received.borrow();
        assert!(r.iter().any(|e| e.0==key_codes::AltGr.to_string()));
        assert!(!r.iter().any(|e| e.0==key_codes::Alt.to_string()),"right Alt must not synthesize left Alt");
        assert!(!r.iter().find(|e|e.0=="v").unwrap().2.alt,"public Alt excludes AltGr as in core physical semantics");
    }
}
'''
if args.baseline:
    harness = harness.replace(function(harness, "fn actions_repeat_modifier_snapshot_and_ordinary_typing("), "fn actions_repeat_modifier_snapshot_and_ordinary_typing() {}")
meta_bits = {'ctrl_on':0x1000,'ctrl_left_on':0x2000,'ctrl_right_on':0x4000,'shift_on':1,'shift_left_on':0x40,'shift_right_on':0x80,'alt_on':2,'alt_left_on':0x10,'alt_right_on':0x20,'meta_on':0x10000,'meta_left_on':0x20000,'meta_right_on':0x40000}
replacements = {
    'KEYS': ','.join(keys), 'CODES': ','.join(codes), 'CONSTANTS': constants,
    'KEY_ARMS': ','.join(f'Key::{k} => key_codes::{k}.to_string()' for k in keys),
    'META_METHODS': '\n'.join(f'fn {k}(self) -> bool {{ self.0 & {v} != 0 }}' for k,v in meta_bits.items()),
    'STATE_STRUCT': state_struct, 'STATE_UPDATE': state_update, 'MODIFIER_METHODS': modifier_methods, 'SHORTCUT': shortcut,
    'STANDARD': '#[derive(Debug)]\n' + standard,
    'TRY_DISPATCH': function(source,'fn try_dispatch_key_event('),
    'NATIVE_DISPATCH': function(source,'fn dispatch_native_key_event(') if not args.baseline else 'fn dispatch_native_key_event(&self, native: &NativeKey) -> KeyEventResult { map_key_event(native).map(|event| self.try_dispatch_key_event(event)).unwrap_or(KeyEventResult::EventIgnored) }',
    'MODIFIERS': function(source,'fn native_key_modifiers(') if not args.baseline else '',
    'LOST_FOCUS': source.split('PollEvent::Main(MainEvent::LostFocus) => {',1)[1].split('\n            }',1)[0] if not args.baseline else '',
    'CORE_FOCUS_RESET': (backend.parent / 'i-slint-core/window.rs').read_text().split('if !have_focus {',1)[1].split('}',1)[0].strip().replace('self.context().0.modifiers', 'self.state'),
    'MAP_EVENT': function(source,'fn map_key_event('), 'MAP_CODE': mapping,
}
for name, value in replacements.items():
    harness = harness.replace(name, value)

java = (subprocess.check_output(['git', 'show', args.clipboard_ref + ':vendor/i-slint-backend-android-activity/java/SlintAndroidJavaHelper.java'], cwd=backend, text=True) if args.clipboard_ref else production('java/SlintAndroidJavaHelper.java'))
get_clipboard = function(java, 'public String get_clipboard()')
java_harness = r'''
import java.util.concurrent.FutureTask;
import java.util.concurrent.Callable;
class Context { static String CLIPBOARD_SERVICE = "clipboard"; }
class ClipData { static class Item { CharSequence text, coerced; boolean throwing; Item(CharSequence t) { text=t; coerced=t; } Item(CharSequence t, CharSequence c) { text=t; coerced=c; } CharSequence getText() { return text; } CharSequence coerceToText(Activity a) { if (throwing) throw new IllegalStateException("SYNTHETIC_PRIVATE_PROVIDER_MARKER"); return coerced; } } Item[] items; ClipData(Item... i) { items=i; } int getItemCount() { return items.length; } Item getItemAt(int i) { return items[i]; } }
class ClipboardManager { ClipData clip; boolean hasPrimaryClip() { return clip != null; } ClipData getPrimaryClip() { return clip; } }
class Activity { ClipboardManager clipboard=new ClipboardManager(); Object getSystemService(String s) { return clipboard; } void runOnUiThread(Runnable r) { r.run(); } }
public class ClipboardCheck {
    Activity mActivity=new Activity();
    GET_CLIPBOARD
    public static void main(String[] args) {
        ClipboardCheck h=new ClipboardCheck();
        if (!h.get_clipboard().equals("")) throw new AssertionError();
        h.mActivity.clipboard.clip=new ClipData();
        if (!h.get_clipboard().equals("")) throw new AssertionError();
        h.mActivity.clipboard.clip=new ClipData(new ClipData.Item(null));
        if (!h.get_clipboard().equals("")) throw new AssertionError();
        for (String text: new String[]{"uuuuuuu", "română 😀 апр", "a\nb"}) {
            h.mActivity.clipboard.clip=new ClipData(new ClipData.Item(new StringBuilder(text)));
            if (!h.get_clipboard().equals(text)) throw new AssertionError();
        }
        h.mActivity.clipboard.clip=new ClipData(new ClipData.Item(null,"coerced synthetic Unicode 😀"));
        if (!h.get_clipboard().equals("coerced synthetic Unicode 😀")) throw new AssertionError("nullDirect_nonemptyCoercion");
        ClipData.Item throwing=new ClipData.Item(null); throwing.throwing=true;
        h.mActivity.clipboard.clip=new ClipData(throwing);
        java.io.ByteArrayOutputStream errors=new java.io.ByteArrayOutputStream();
        java.io.PrintStream original=System.err;
        String result;
        try { System.setErr(new java.io.PrintStream(errors)); result=h.get_clipboard(); } finally { System.setErr(original); }
        if (!result.equals("")) throw new AssertionError("throwingProvider_returnsEmpty");
        String stderr=errors.toString();
        if (stderr.contains("SYNTHETIC_PRIVATE_PROVIDER_MARKER") || !stderr.equals("Clipboard read failed"+System.lineSeparator())) throw new AssertionError("throwingProvider_fixedMarkerOnly");
        System.out.println("PASS: nullDirect_nonemptyCoercion and throwingProvider_fixedMarkerOnly; absent/empty/null/Unicode");
    }
}
'''.replace('GET_CLIPBOARD',get_clipboard)
def run(root):
    root.mkdir(parents=True, exist_ok=True)
    (root / 'keys.rs').write_text(harness)
    (root / 'ClipboardCheck.java').write_text(java_harness)
    commands = [
        ['rustc','--edition=2024','--test','-Awarnings',str(root/'keys.rs'),'-o',str(root/'keys')],
        [str(root/'keys'),'--nocapture'] + (['exact_'] if args.baseline else []),
        ['javac',str(root/'ClipboardCheck.java')],
        ['java','-cp',str(root),'ClipboardCheck'],
    ]
    failures = []
    for i, command in enumerate(commands):
        result=subprocess.run(command,text=True,capture_output=True)
        output=result.stdout+result.stderr
        (root / f'step-{i}.log').write_text(output)
        print(output, end='')
        print(f'STEP {i} exit={result.returncode}')
        failures.append(result.returncode)
        if i in (0,2) and result.returncode:
            raise SystemExit('Compilation failure is not valid behavioral evidence')
    if args.baseline:
        rust=(root/'step-1.log').read_text()
        java=(root/'step-3.log').read_text()
        assert failures[1] and failures[3], 'baseline must fail both behavioral suites'
        assert 'exact_ctrl_v_shortcut_one_press_release_none ... FAILED' in rust
        assert 'exact_dedicated_paste_shortcut_one_press_release_none ... FAILED' in rust
        assert 'nullDirect_nonemptyCoercion' in java
        print('PASS: baseline compiled and failed named Rust/Java behavioral assertions')
    elif any(failures):
        raise SystemExit(1)

if args.evidence_dir:
    run(args.evidence_dir.resolve())
else:
    with tempfile.TemporaryDirectory(prefix='chatter-native-keys-') as tmp:
        run(Path(tmp))

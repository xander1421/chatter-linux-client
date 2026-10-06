#!/usr/bin/env python3
"""Execute production native mapping/dispatch and core shortcut classification.

rustc + javac only; recording seams do not access clipboard/device/application
state and do not claim full TextInput, JNI, or OEM IME execution coverage.
"""
from pathlib import Path
import re
import subprocess
import tempfile

backend = Path(__file__).resolve().parents[1]
source = (backend / 'androidwindowadapter.rs').read_text()
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


mapping = function(source, 'fn map_key_code(')
keys = sorted(set(re.findall(r'Key::(\w+)', source)))
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
enum WindowEvent { KeyPressed { text: String }, KeyPressRepeated { text: String }, KeyReleased { text: String }, Other }
#[derive(Default)]
struct Window { state: Cell<InternalKeyboardModifierState>, received: RefCell<Vec<(String, bool, KeyboardModifiers, Option<StandardShortcut>, bool)>> }
struct WindowInner;
impl WindowInner {
    fn from_pub(w: &Window) -> &Window { w }
}
impl Window {
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
struct AndroidWindowAdapter { window: Window, long_press: Cell<Option<()>>, native_held_modifiers: Cell<[bool;8]> }
impl AndroidWindowAdapter {
    fn dismiss_selection_menu(&self) {}
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
            assert!(e.2.control && e.3.is_some() && e.4);
            assert_eq!(records.len(), 3, "one modifier press + real key + modifier release");
            let s = a.window.state.get();
            assert!(!s.left_control && !s.right_control);
        }
    }
    for (code,text) in [(Keycode::Copy,"c"),(Keycode::Cut,"x"),(Keycode::Paste,"v")] {
        let a = AndroidWindowAdapter::default();
        a.dispatch_native_key_event(&key(code,0x70003,KeyAction::Down,0));
        assert!(a.window.received.borrow().iter().any(|e| e.0 == text && e.2.control && !e.2.shift && !e.2.alt && !e.2.meta && e.3.is_some()));
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
meta_bits = {'ctrl_on':0x1000,'ctrl_left_on':0x2000,'ctrl_right_on':0x4000,'shift_on':1,'shift_left_on':0x40,'shift_right_on':0x80,'alt_on':2,'alt_left_on':0x10,'alt_right_on':0x20,'meta_on':0x10000,'meta_left_on':0x20000,'meta_right_on':0x40000}
replacements = {
    'KEYS': ','.join(keys), 'CODES': ','.join(codes), 'CONSTANTS': constants,
    'KEY_ARMS': ','.join(f'Key::{k} => key_codes::{k}.to_string()' for k in keys),
    'META_METHODS': '\n'.join(f'fn {k}(self) -> bool {{ self.0 & {v} != 0 }}' for k,v in meta_bits.items()),
    'STATE_STRUCT': state_struct, 'STATE_UPDATE': state_update, 'MODIFIER_METHODS': modifier_methods, 'SHORTCUT': shortcut,
    'STANDARD': '#[derive(Debug)]\n' + standard,
    'TRY_DISPATCH': function(source,'fn try_dispatch_key_event('),
    'NATIVE_DISPATCH': function(source,'fn dispatch_native_key_event('),
    'MODIFIERS': function(source,'fn native_key_modifiers('),
    'MAP_EVENT': function(source,'fn map_key_event('), 'MAP_CODE': mapping,
}
for name, value in replacements.items():
    harness = harness.replace(name, value)

java = (backend / 'java/SlintAndroidJavaHelper.java').read_text()
get_clipboard = function(java, 'public String get_clipboard()')
java_harness = r'''
import java.util.concurrent.FutureTask;
import java.util.concurrent.Callable;
class Context { static String CLIPBOARD_SERVICE = "clipboard"; }
class Item { CharSequence text; Item(CharSequence t) { text=t; } CharSequence coerceToText(Activity a) { return text; } }
class ClipData { Item[] items; ClipData(Item... i) { items=i; } int getItemCount() { return items.length; } Item getItemAt(int i) { return items[i]; } }
class ClipboardManager { ClipData clip; ClipData getPrimaryClip() { return clip; } }
class Activity { ClipboardManager clipboard=new ClipboardManager(); Object getSystemService(String s) { return clipboard; } void runOnUiThread(Runnable r) { r.run(); } }
public class ClipboardCheck {
    Activity mActivity=new Activity();
    GET_CLIPBOARD
    public static void main(String[] args) {
        ClipboardCheck h=new ClipboardCheck();
        if (!h.get_clipboard().equals("")) throw new AssertionError();
        h.mActivity.clipboard.clip=new ClipData();
        if (!h.get_clipboard().equals("")) throw new AssertionError();
        h.mActivity.clipboard.clip=new ClipData(new Item(null));
        if (!h.get_clipboard().equals("")) throw new AssertionError();
        for (String text: new String[]{"uuuuuuu", "română 😀 апр", "a\nb"}) {
            h.mActivity.clipboard.clip=new ClipData(new Item(new StringBuilder(text)));
            if (!h.get_clipboard().equals(text)) throw new AssertionError();
        }
        System.out.println("PASS: production clipboard accessor, absent/empty/null/styled Unicode synthetic clips");
    }
}
'''.replace('GET_CLIPBOARD',get_clipboard)
with tempfile.TemporaryDirectory(prefix='chatter-native-keys-') as tmp:
    root = Path(tmp)
    (root / 'keys.rs').write_text(harness)
    subprocess.run(['rustc','--edition=2024','--test','-Awarnings',str(root/'keys.rs'),'-o',str(root/'keys')],check=True)
    subprocess.run([str(root/'keys'),'--nocapture'],check=True)
    (root/'ClipboardCheck.java').write_text(java_harness)
    subprocess.run(['javac',str(root/'ClipboardCheck.java')],check=True)
    subprocess.run(['java','-cp',str(root),'ClipboardCheck'],check=True)

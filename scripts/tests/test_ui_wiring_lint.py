"""Parser regressions for touch-reachable context menus (no UI runtime needed)."""
import importlib.util
from pathlib import Path
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / 'ui-wiring-lint.py'
spec = importlib.util.spec_from_file_location('ui_wiring_lint', SCRIPT)
lint = importlib.util.module_from_spec(spec)
spec.loader.exec_module(lint)

FIXTURE = '''
export component Message inherits Rectangle {
    callback menu();
    callback unrelated();
    function start(x: length) { root.arm(); }
    function arm() { hold.running = true; }
    function open() { root.menu(); }
    hold := Timer {
        running: false;
        interval: 500ms;
        triggered => { root.open(); }
    }
    row := TouchArea {
        pointer-event(event) => {
            if (event.button == PointerEventButton.right && event.kind == PointerEventKind.up) {
                root.menu();
                return;
            }
            if (event.kind == PointerEventKind.down && event.button == PointerEventButton.left) {
                root.start(self.mouse-x);
            }
            if event.kind == PointerEventKind.cancel { hold.running = false; }
        }
    }
}
'''


class TouchMenuParserTests(unittest.TestCase):
    def gaps(self, source):
        return lint.right_click_only_areas({str(Path(lint.UI_DIR) / 'fixture.slint'): source})

    def test_reachable_helper_chain(self):
        self.assertEqual(self.gaps(FIXTURE), [])

    def test_inline_timer(self):
        self.assertEqual(self.gaps(FIXTURE.replace('root.start(self.mouse-x);', 'hold.running = true;')), [])

    def test_missing_press_call(self):
        self.assertEqual(len(self.gaps(FIXTURE.replace('root.start(self.mouse-x);', ''))), 1)

    def test_release_only_helper_is_not_a_hold(self):
        self.assertEqual(len(self.gaps(FIXTURE.replace('PointerEventKind.down', 'PointerEventKind.up'))), 1)

    def test_right_button_down_is_not_touch(self):
        self.assertEqual(len(self.gaps(FIXTURE.replace('PointerEventButton.left', 'PointerEventButton.right'))), 1)

    def test_outer_right_guard_is_not_touch(self):
        source = FIXTURE.replace('root.start(self.mouse-x);', 'if event.button == PointerEventButton.right { root.start(self.mouse-x); }')
        self.assertEqual(len(self.gaps(source)), 1)

    def test_timer_must_exist(self):
        self.assertEqual(len(self.gaps(FIXTURE.replace('hold := Timer', 'other := Timer'))), 1)

    def test_timer_must_open_the_same_menu(self):
        self.assertEqual(len(self.gaps(FIXTURE.replace('triggered => { root.open(); }', 'triggered => { root.unrelated(); }'))), 1)

    def test_dead_timer_callback(self):
        self.assertEqual(len(self.gaps(FIXTURE.replace('triggered =>', 'unused =>'))), 1)

    def test_comment_and_string_braces_do_not_break_enclosing_scope(self):
        source = FIXTURE.replace('root.start(self.mouse-x);', '// } root.fake();\n root.start(self.mouse-x);')
        source = source.replace('callback menu();', 'property <string> note: "} PointerEventButton.right {"; /* } { */\n callback menu();')
        self.assertEqual(self.gaps(source), [])

    def test_commented_press_call_does_not_count(self):
        self.assertEqual(len(self.gaps(FIXTURE.replace('root.start(self.mouse-x);', '/* root.start(self.mouse-x); */'))), 1)

    def test_different_component_helper_does_not_count(self):
        source = FIXTURE.replace('function arm() { hold.running = true; }', '')
        source += '\ncomponent Other inherits Rectangle { function arm() { hold.running = true; } }'
        self.assertEqual(len(self.gaps(source)), 1)

    def test_recursive_helpers_terminate(self):
        source = FIXTURE.replace('function arm() { hold.running = true; }', 'function arm() { root.start(0px); }')
        self.assertEqual(len(self.gaps(source)), 1)

    def test_hold_area_remains_supported(self):
        source = 'component Row inherits HoldArea { pointer-event(e) => { if e.button == PointerEventButton.right { root.menu(); } } }'
        self.assertEqual(self.gaps(source), [])

    def test_unrelated_inline_running_assignment_does_not_hide_gap(self):
        source = FIXTURE.replace('root.start(self.mouse-x);', 'other.running = true;')
        self.assertEqual(len(self.gaps(source)), 1)

    def test_real_message_touch_areas(self):
        path = Path(lint.UI_DIR) / 'chat/message_item.slint'
        self.assertEqual(lint.right_click_only_areas({str(path): path.read_text()}), [])

    def test_real_message_missing_press_paths_are_detected(self):
        path = Path(lint.UI_DIR) / 'chat/message_item.slint'
        source = path.read_text().replace('root.begin-message-press(', 'root.missing-press(')
        gaps = lint.right_click_only_areas({str(path): source})
        self.assertEqual(len(gaps), 2)


if __name__ == '__main__':
    unittest.main()

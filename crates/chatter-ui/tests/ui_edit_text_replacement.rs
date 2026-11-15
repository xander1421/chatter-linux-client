//! Android sends whole-field snapshots when editing existing form values.
use slint::ComponentHandle;

slint::slint! {
    import { Input } from "../../ui/components/input.slint";
    export component EditProbe inherits Window {
        width: 400px;
        height: 260px;
        in property <bool> password: false;
        in-out property <string> draft: "Original";
        in-out property <string> linked-draft: "Original";
        in-out property <string> document: "Existing message";
        out property <string> actual: field.text;
        out property <string> linked-actual: linked-field.text;
        out property <int> edits: 0;
        public function focus-field(linked: bool) {
            if linked { linked-field.focus(); } else { field.focus(); }
        }
        public function select-field(linked: bool) {
            if linked { linked-field.select-all(); } else { field.select-all(); }
        }
        public function focus-document() { document-field.focus(); }
        field := Input {
            width: 350px;
            password: root.password;
            text: root.draft;
            edited(value) => { root.draft = value; root.edits += 1; }
        }
        linked-field := Input {
            y: 80px;
            width: 350px;
            password: root.password;
            text <=> root.linked-draft;
            edited(value) => { root.edits += 1; }
        }
        document-field := TextInput {
            y: 160px;
            width: 350px;
            height: 80px;
            single-line: false;
            text <=> root.document;
        }
    }
}

fn replace_snapshot(w: &EditProbe, text: &str) {
    let mut event = i_slint_core::input::InternalKeyEvent::default();
    event.event_type = i_slint_core::input::KeyEventType::CommitComposition;
    event.replacement_range = Some(i32::MIN..i32::MAX);
    event.key_event.text = text.into();
    event.cursor_position = Some(text.len() as i32);
    event.anchor_position = event.cursor_position;
    w.window()
        .dispatch_event(i_slint_core::platform::WindowEvent::internal(event));
    slint::platform::update_timers_and_animations();
}

#[test]
fn native_whole_field_replacement_does_not_repeat_existing_form_values() {
    i_slint_backend_testing::init_no_event_loop();
    let w = EditProbe::new().unwrap();
    w.show().unwrap();
    // Callback-backed forms, two-way profile/contact drafts, and masked inputs.
    for password in [false, true] {
        w.set_password(password);
        w.set_draft("Original".into());
        w.set_linked_draft("Original".into());
        for linked in [false, true] {
            w.invoke_focus_field(linked);
            for expected in ["Originalq", "Originalqw", "Originalw", "", "é🙂", "éx🙂"] {
                let edits_before = w.get_edits();
                replace_snapshot(&w, expected);
                let (actual, draft) = if linked {
                    (w.get_linked_actual(), w.get_linked_draft())
                } else {
                    (w.get_actual(), w.get_draft())
                };
                assert_eq!(
                    actual.as_str(),
                    expected,
                    "linked={linked}, password={password}"
                );
                assert_eq!(
                    draft.as_str(),
                    expected,
                    "linked={linked}, password={password}"
                );
                assert_eq!(
                    w.get_edits(),
                    edits_before + 1,
                    "one edit notification, including clearing"
                );
            }
            // Selection replacement through ordinary keys uses the same path.
            w.invoke_select_field(linked);
            w.window()
                .dispatch_event(slint::platform::WindowEvent::KeyPressed {
                    text: "replacement".into(),
                });
            slint::platform::update_timers_and_animations();
            let actual = if linked {
                w.get_linked_actual()
            } else {
                w.get_actual()
            };
            assert_eq!(actual.as_str(), "replacement");
        }
    }
    // Multiline fields use the same native item for bios, descriptions and edits.
    w.invoke_focus_document();
    for expected in [
        "Existing messageq",
        "Existing\nmessage",
        "",
        "é🙂\nsecond line",
    ] {
        replace_snapshot(&w, expected);
        assert_eq!(w.get_document().as_str(), expected);
    }
}

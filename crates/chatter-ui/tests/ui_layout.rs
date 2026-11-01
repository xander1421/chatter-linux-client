//! VALIDATED-UI layout regression — geometry-level proofs.
//!
//! `ui_wiring.rs` proves callbacks reach core; this file proves elements LAND
//! ON SCREEN. Separate test binary because the integration testing backend
//! (needed for real layout passes) is once-per-process and `ui_wiring.rs`
//! already claims it for the settings-modal routing test.
//!
//! Regression under test: the thread panel opened (state set, panel visible in
//! the fullscreen-call layout) but never appeared in the NORMAL desktop layout
//! — reported 2026-07-19 with screenshots. A wiring test can't see this class
//! of bug: the conditional instantiates fine; it's the layout pass that must
//! place it inside the window.

use chatter_ui::MainWindow;
use i_slint_backend_testing as testing;
use slint::ComponentHandle;

const WIN_W: f32 = 1400.0;
const WIN_H: f32 = 900.0;

/// Force a real layout pass: resize to the given size and spin the event loop
/// briefly so conditional sub-trees materialize and get geometry. Varying the
/// size between pumps forces a full relayout (a same-size event is a no-op).
fn pump_at(w: &MainWindow, width: f32, height: f32) {
    w.window().set_size(slint::LogicalSize::new(width, height));
    // ComponentHandle::run() hides the window on return, enqueueing another
    // Quit in this backend. Keep it shown between pumps so later runs cannot
    // consume a stale Quit instead of advancing timers and spring animations.
    w.show().unwrap();
    let settled = std::rc::Rc::new(std::cell::Cell::new(false));
    let capture = settled.clone();
    slint::Timer::single_shot(std::time::Duration::from_millis(1000), move || {
        capture.set(true);
        slint::quit_event_loop().unwrap();
    });
    slint::run_event_loop_until_quit().unwrap();
    assert!(
        settled.get(),
        "layout pump returned before its settling timer"
    );
}

fn click(w: &MainWindow, label: &str) {
    let element = testing::ElementHandle::find_by_accessible_label(w, label)
        .next()
        .unwrap();
    let pos = element.absolute_position();
    let size = element.size();
    let position = slint::LogicalPosition::new(pos.x + size.width / 2.0, pos.y + size.height / 2.0);
    w.window()
        .dispatch_event(slint::platform::WindowEvent::PointerPressed {
            position,
            button: slint::platform::PointerEventButton::Left,
        });
    w.window()
        .dispatch_event(slint::platform::WindowEvent::PointerReleased {
            position,
            button: slint::platform::PointerEventButton::Left,
        });
}

fn pump(w: &MainWindow) {
    pump_at(w, WIN_W, WIN_H);
}

/// Fail with the cause, not the symptom, when the element-query API is inert.
///
/// `ElementHandle` needs debug info that the Slint compiler only emits when
/// the `ui-test-debug` feature is enabled at build time. Without it every query
/// returns nothing, and the assertions here then report a UI that looks
/// structurally broken -- "ThreadPanel was never instantiated", "there are no
/// Rectangles" -- when the only thing wrong is the command line. Check the real
/// precondition first.
fn require_element_queries() {
    assert!(
        chatter_ui::SLINT_DEBUG_INFO_ENABLED,
        "the ElementHandle query API is inert: chatter-ui was built without \
         the ui-test-debug feature, so every element query returns nothing and \
         nothing asserted below can mean anything. Run the UI suites with:\n\n    \
         ./scripts/test-ui.sh\n\n  or directly:\n\n    \
         cargo test -p chatter-ui --features ui-test-debug --test ui_wiring --test ui_layout"
    );
}

#[test]
fn thread_panel_lands_inside_the_normal_desktop_layout() {
    require_element_queries();
    testing::init_integration_test_with_system_time();
    let w = MainWindow::new().expect("MainWindow::new");
    w.set_current_view("main".into());

    let count = || testing::ElementHandle::find_by_element_type_name(&w, "ThreadPanel").count();

    // Negative control: panel closed → not in the tree.
    pump(&w);
    assert_eq!(
        count(),
        0,
        "ThreadPanel present before thread-panel-visible was set"
    );

    // Open the panel exactly the way the Rust thread handler does.
    w.global::<chatter_ui::ChatPane>()
        .set_thread_panel_visible(true);
    w.global::<chatter_ui::ChatPane>()
        .set_thread_root_sender_name("Test Sender".into());
    w.global::<chatter_ui::ChatPane>()
        .set_thread_root_content_preview("root message".into());
    w.global::<chatter_ui::ChatPane>()
        .set_channel_name("test-channel".into());
    pump(&w);

    let panel = testing::ElementHandle::find_by_element_type_name(&w, "ThreadPanel")
        .next()
        .expect("thread-panel-visible=true did not surface a visible ThreadPanel");

    let pos = panel.absolute_position();
    let size = panel.size();
    // NOTE: the testing backend ignores Resized events — assert against the
    // ACTUAL window width, not the requested one.
    let win = w.window().size().to_logical(w.window().scale_factor());
    println!(
        "window={}x{} panel: x={} y={} w={} h={}",
        win.width, win.height, pos.x, pos.y, size.width, size.height
    );

    // The panel must occupy real width…
    assert!(
        size.width >= 200.0,
        "ThreadPanel laid out with width {} — squeezed out of the layout",
        size.width
    );
    // …and sit fully INSIDE the window (the reported bug: panel pushed past the
    // right edge and clipped invisible while the chat column kept full width).
    assert!(
        pos.x >= 0.0 && pos.x + size.width <= win.width + 0.5,
        "ThreadPanel laid out OFF-WINDOW: x={} width={} window-width={}",
        pos.x,
        size.width,
        win.width
    );
    // Desktop layout: panel docks at the right edge.
    assert!(
        (pos.x + size.width) >= win.width - 40.0,
        "ThreadPanel not docked at the right edge: x={} width={} window-width={}",
        pos.x,
        size.width,
        win.width
    );
    composer_drawer_and_recording_have_usable_geometry();
}

/// The drawer must fit the conversation and leave its composer in view; a
/// recording needs its own height instead of overlaying four controls on 56px.
fn composer_drawer_and_recording_have_usable_geometry() {
    use chatter_ui::VoiceRecording;
    require_element_queries();
    let w = MainWindow::new().unwrap();
    w.set_current_view("main".into());
    chatter_ui::bin_ui::features::composer::wire_text_format(&w);
    w.global::<chatter_ui::ChatPane>()
        .set_channel_name("Composer geometry".into());
    w.global::<chatter_ui::SidebarPane>()
        .set_mobile_show_chat(true);
    w.global::<chatter_ui::ChatPane>()
        .set_composer_text("Draft stays here".into());
    for (mobile, width, height) in [
        (true, 320.0, 800.0),
        (true, 360.0, 480.0),
        (false, 1200.0, 900.0),
    ] {
        w.set_is_mobile_platform(mobile);
        w.window().set_size(slint::LogicalSize::new(width, height));
        pump_at(&w, width, height);
        click(&w, "Message input");
        w.window()
            .dispatch_event(slint::platform::WindowEvent::KeyPressed {
                text: slint::platform::Key::End.into(),
            });
        w.window()
            .dispatch_event(slint::platform::WindowEvent::KeyPressed {
                text: slint::platform::Key::LeftArrow.into(),
            });
        click(&w, "Emoji & GIF picker");
        pump_at(&w, width, height);
        assert!(
            w.global::<chatter_ui::ChatPane>()
                .get_composer_emoji_picker_visible()
        );
        assert!(
            (w.window()
                .size()
                .to_logical(w.window().scale_factor())
                .width
                - width)
                .abs()
                < 1.0
        );
        w.invoke_insert_composer_emoji("😀".into());
        pump_at(&w, width, height);
        assert_eq!(
            w.global::<chatter_ui::ChatPane>()
                .get_composer_text()
                .as_str(),
            "Draft stays her😀e"
        );
        // The open-state label names what the click does: on touch it hands
        // the keyboard back, on desktop it closes the drawer.
        click(
            &w,
            if mobile {
                "Show keyboard"
            } else {
                "Close picker"
            },
        );
        pump_at(&w, width, height);
        assert!(
            !w.global::<chatter_ui::ChatPane>()
                .get_composer_emoji_picker_visible()
        );
        w.window()
            .dispatch_event(slint::platform::WindowEvent::KeyPressed {
                text: slint::platform::Key::Backspace.into(),
            });
        assert_eq!(
            w.global::<chatter_ui::ChatPane>()
                .get_composer_text()
                .as_str(),
            "Draft stays here"
        );
        w.global::<chatter_ui::ChatPane>()
            .set_composer_emoji_picker_visible(true);
        pump_at(&w, width, height);
        let chat = testing::ElementHandle::find_by_element_type_name(&w, "ChatLayout")
            .next()
            .unwrap();
        let emoji = testing::ElementHandle::find_by_element_type_name(&w, "EmojiPicker")
            .next()
            .unwrap();
        let composer = testing::ElementHandle::find_by_element_type_name(&w, "MessageComposer")
            .next()
            .unwrap();
        let cp = chat.absolute_position();
        let ep = emoji.absolute_position();
        // Touch: the drawer spans the pane. Desktop: a bounded panel under
        // the emoji button, never the whole 1200px pane.
        if mobile {
            assert!(emoji.size().width >= chat.size().width - 50.0);
        } else {
            assert!(emoji.size().width <= 420.0 && emoji.size().width >= 300.0);
        }
        assert!(ep.x >= cp.x && ep.x + emoji.size().width <= cp.x + chat.size().width + 1.0);
        // Touch docks the drawer under the composer; desktop floats it as a
        // popover directly above the composer (f417a0b7).
        let composer_top = composer.absolute_position().y;
        if mobile {
            assert!(ep.y >= composer_top + composer.size().height - 1.0);
        } else {
            assert!(ep.y >= cp.y && ep.y + emoji.size().height <= composer_top + 1.0);
        }
        assert!(ep.y + emoji.size().height <= cp.y + chat.size().height + 1.0);
        // A user edit must not detach the search field from later tab/reset
        // updates. Inspect the visible input, not just the backing query.
        click(&w, "Search emoji and GIFs");
        for text in ["c", "a", "t"] {
            w.window()
                .dispatch_event(slint::platform::WindowEvent::KeyPressed { text: text.into() });
        }
        pump_at(&w, width, height);
        assert_eq!(
            w.global::<chatter_ui::ChatPane>()
                .get_composer_emoji_search()
                .as_str(),
            "cat"
        );
        let search_value = || {
            testing::ElementHandle::find_by_accessible_label(&w, "Search emoji and GIFs")
                .next()
                .unwrap()
                .accessible_value()
                .unwrap()
                .to_string()
        };
        assert_eq!(search_value(), "cat");
        w.global::<chatter_ui::CallView>()
            .set_active_call_fullscreen(true);
        pump_at(&w, width, height);
        assert!(
            testing::ElementHandle::find_by_element_type_name(&w, "ChatLayout")
                .next()
                .is_none()
        );
        w.global::<chatter_ui::CallView>()
            .set_active_call_fullscreen(false);
        pump_at(&w, width, height);
        assert_eq!(
            search_value(),
            "cat",
            "remounted drawer lost its visible query"
        );
        assert_eq!(
            w.global::<chatter_ui::ChatPane>()
                .get_composer_text()
                .as_str(),
            "Draft stays here",
            "open drawer remount replayed emoji insertion"
        );
        click(&w, "GIFs");
        pump_at(&w, width, height);
        assert_eq!(
            search_value(),
            w.global::<chatter_ui::ChatPane>()
                .get_composer_gif_picker_search()
                .as_str()
        );
        w.global::<chatter_ui::ChatPane>()
            .set_composer_gif_picker_search("dogs".into());
        pump_at(&w, width, height);
        assert_eq!(search_value(), "dogs");
        click(&w, "Emoji");
        pump_at(&w, width, height);
        assert_eq!(search_value(), "cat");
        w.global::<chatter_ui::ChatPane>()
            .set_composer_emoji_search("".into());
        pump_at(&w, width, height);
        assert_eq!(search_value(), "");
        // Opening a drawer leaves Back/Escape focused on desktop too.
        let backs = std::rc::Rc::new(std::cell::Cell::new(0));
        let capture = backs.clone();
        w.on_back_pressed(move || capture.set(capture.get() + 1));
        w.window()
            .dispatch_event(slint::platform::WindowEvent::KeyPressed {
                text: slint::platform::Key::Escape.into(),
            });
        assert_eq!(
            backs.get(),
            1,
            "drawer must retain a Back/Escape focus owner"
        );
        w.global::<chatter_ui::ChatPane>()
            .set_composer_emoji_picker_visible(false);
        w.global::<chatter_ui::ChatPane>()
            .set_composer_gif_picker_visible(true);
        pump_at(&w, width, height);
        let chat = testing::ElementHandle::find_by_element_type_name(&w, "ChatLayout")
            .next()
            .unwrap();
        let cp = chat.absolute_position();
        let gif = testing::ElementHandle::find_by_element_type_name(&w, "GifPicker")
            .next()
            .unwrap();
        if mobile {
            assert!(gif.size().width >= chat.size().width - 50.0);
        } else {
            assert!(gif.size().width <= 420.0 && gif.size().width >= 300.0);
        }
        assert!(gif.absolute_position().y + gif.size().height <= cp.y + chat.size().height + 1.0);
        w.global::<chatter_ui::ChatPane>()
            .set_composer_gif_picker_visible(false);
        pump_at(&w, width, height);
        composer_terminal_actions(&w, width, height);
        w.global::<chatter_ui::ChatPane>()
            .set_composer_text("Draft stays here".into());
        w.global::<VoiceRecording>().set_owner("main".into());
        w.global::<VoiceRecording>().set_active(true);
        pump_at(&w, width, height);
        let composer = testing::ElementHandle::find_by_element_type_name(&w, "MessageComposer")
            .next()
            .unwrap();
        // Each explicit action must fit inside the recording row.
        let bottom = composer.absolute_position().y + composer.size().height;
        let mut last_right = composer.absolute_position().x;
        for label in ["Cancel voice message", "Send voice message"] {
            let control = testing::ElementHandle::find_by_accessible_label(&w, label)
                .next()
                .unwrap();
            assert!(control.size().width >= 48.0 && control.size().height >= 48.0);
            assert!(control.absolute_position().x >= last_right);
            assert!(control.absolute_position().y >= composer.absolute_position().y);
            assert!(control.absolute_position().y + control.size().height <= bottom + 1.0);
            last_right = control.absolute_position().x + control.size().width;
        }
        assert_eq!(
            w.global::<chatter_ui::ChatPane>()
                .get_composer_text()
                .as_str(),
            "Draft stays here"
        );
        let finishes = std::rc::Rc::new(std::cell::Cell::new(0));
        let capture = finishes.clone();
        w.global::<VoiceRecording>()
            .on_finish(move || capture.set(capture.get() + 1));
        w.global::<VoiceRecording>().set_busy(true);
        pump_at(&w, width, height);
        testing::ElementHandle::find_by_accessible_label(&w, "Send voice message")
            .next()
            .unwrap()
            .invoke_accessible_default_action();
        assert_eq!(finishes.get(), 0, "busy recording must not send");
        w.global::<VoiceRecording>().set_busy(false);
        pump_at(&w, width, height);
        testing::ElementHandle::find_by_accessible_label(&w, "Send voice message")
            .next()
            .unwrap()
            .invoke_accessible_default_action();
        assert_eq!(finishes.get(), 1);
        assert_eq!(
            w.global::<chatter_ui::ChatPane>()
                .get_composer_text()
                .as_str(),
            "Draft stays here"
        );
        w.global::<VoiceRecording>().set_active(false);
        w.global::<chatter_ui::ChatPane>()
            .set_composer_text("".into());
        pump_at(&w, width, height);
        // Tap-to-record owns capture until an explicit Cancel or Send. A
        // pointer release must not implicitly finish the voice message.
        let weak = w.as_weak();
        w.global::<VoiceRecording>().on_start(move || {
            weak.upgrade()
                .unwrap()
                .global::<VoiceRecording>()
                .set_active(true);
        });
        let cancels = std::rc::Rc::new(std::cell::Cell::new(0));
        let capture = cancels.clone();
        let weak = w.as_weak();
        w.global::<VoiceRecording>().on_cancel(move || {
            capture.set(capture.get() + 1);
            weak.upgrade()
                .unwrap()
                .global::<VoiceRecording>()
                .set_active(false);
        });
        click(&w, "Record voice message");
        pump_at(&w, width, height);
        assert!(w.global::<VoiceRecording>().get_active());
        assert_eq!(finishes.get(), 1, "tap/release must not send a recording");
        click(&w, "Cancel voice message");
        assert_eq!(cancels.get(), 1);
        assert!(!w.global::<VoiceRecording>().get_active());
        pump_at(&w, width, height);
        click(&w, "Record voice message");
        pump_at(&w, width, height);
        assert!(w.global::<VoiceRecording>().get_active());
        click(&w, "Send voice message");
        assert_eq!(finishes.get(), 2, "explicit recording Send must finish");
        assert_eq!(cancels.get(), 1);
        w.global::<VoiceRecording>().set_active(false);
        w.global::<chatter_ui::ChatPane>()
            .set_composer_text("Draft stays here".into());
        pump_at(&w, width, height);
        click(&w, "Message input");
        w.global::<chatter_ui::ChatPane>().set_selection_mode(true);
        pump_at(&w, width, height);
        w.window()
            .dispatch_event(slint::platform::WindowEvent::KeyPressed { text: "x".into() });
        assert_eq!(
            w.global::<chatter_ui::ChatPane>()
                .get_composer_text()
                .as_str(),
            "Draft stays here",
            "hidden composer retained typing focus"
        );
        w.global::<chatter_ui::ChatPane>().set_selection_mode(false);
        w.global::<chatter_ui::SidebarPane>()
            .set_mobile_show_chat(false);
        pump_at(&w, width, height);
        w.global::<chatter_ui::SidebarPane>()
            .set_mobile_show_chat(true);
        pump_at(&w, width, height);
        assert_eq!(
            w.global::<chatter_ui::ChatPane>()
                .get_composer_text()
                .as_str(),
            "Draft stays here",
            "mount replayed the previous emoji insertion"
        );
    }
}

/// Exercise the real composer through its public input paths. Label search
/// includes the mounted, transparent microphone during the Send transition.
fn composer_terminal_actions(w: &MainWindow, width: f32, height: f32) {
    use chatter_ui::{ChatPane, VoiceRecording};
    use std::{cell::Cell, rc::Rc};
    let starts = Rc::new(Cell::new(0));
    let capture = starts.clone();
    w.global::<VoiceRecording>()
        .on_start(move || capture.set(capture.get() + 1));
    let sends = Rc::new(Cell::new(0));
    let capture = sends.clone();
    w.global::<ChatPane>().on_send(move |text| {
        assert!(!text.is_empty());
        capture.set(capture.get() + 1);
        false // Keep the draft so each input path can be exercised independently.
    });
    let mic = || {
        testing::ElementHandle::find_by_accessible_label(w, "Record voice message")
            .next()
            .expect("mounted microphone")
    };
    let key = |text: slint::SharedString| {
        w.window()
            .dispatch_event(slint::platform::WindowEvent::KeyPressed { text: text.clone() });
        w.window()
            .dispatch_event(slint::platform::WindowEvent::KeyReleased { text });
    };

    // Empty-draft positive controls: pointer, accessible action, and keyboard
    // must all reach recording, otherwise the negative controls prove nothing.
    w.global::<ChatPane>().set_composer_text("".into());
    pump_at(w, width, height);
    assert_eq!(mic().accessible_enabled(), Some(true));
    assert!(mic().computed_opacity() > 0.99);
    assert!(
        testing::ElementHandle::find_by_accessible_label(w, "Send message")
            .next()
            .is_none()
    );
    click(w, "Record voice message");
    assert_eq!(starts.get(), 1, "empty-draft pointer recording");
    mic().invoke_accessible_default_action();
    assert_eq!(starts.get(), 2, "empty-draft accessible recording");
    click(w, "Message input");
    // Tab order includes other composer controls and varies with platform.
    // Walk with real keys, inspecting focus without activating other buttons.
    let mic_has_focus = || {
        let window = i_slint_core::window::WindowInner::from_pub(w.window());
        let mut item = window.focus_item.borrow().upgrade();
        while let Some(current) = item {
            if current
                .accessible_string_property(
                    i_slint_core::accessibility::AccessibleStringProperty::Label,
                )
                .is_some_and(|label| label == "Record voice message")
            {
                return true;
            }
            item =
                current.parent_item(i_slint_core::item_tree::ParentItemTraversalMode::StopAtPopups);
        }
        false
    };
    for _ in 0..64 {
        key(slint::platform::Key::Tab.into());
        if mic_has_focus() {
            break;
        }
    }
    assert!(
        mic_has_focus(),
        "empty microphone must be keyboard reachable"
    );
    key(slint::platform::Key::Space.into());
    assert_eq!(
        starts.get(),
        3,
        "Space on focused empty microphone must record"
    );

    // Keep keyboard focus from the working microphone, then make it inactive.
    w.global::<ChatPane>()
        .set_composer_text("Draft stays here".into());
    pump_at(w, width, height);
    let microphone = mic();
    let send = testing::ElementHandle::find_by_accessible_label(w, "Send message")
        .next()
        .expect("nonempty draft must offer Send");
    println!(
        "composer {width}x{height}: mic opacity={} enabled={:?}, Send opacity={}",
        microphone.computed_opacity(),
        microphone.accessible_enabled(),
        send.computed_opacity()
    );
    assert!(
        microphone.computed_opacity() <= 0.001,
        "settled microphone must not render"
    );
    assert_eq!(microphone.accessible_enabled(), Some(false));
    assert!(
        send.computed_opacity() > 0.99,
        "Send must render after settling"
    );
    microphone.invoke_accessible_default_action();
    assert_eq!(
        starts.get(),
        3,
        "inactive microphone accepted accessible activation"
    );
    println!(
        "inactive microphone retains keyboard focus: {}",
        mic_has_focus()
    );
    key(slint::platform::Key::Space.into());
    let after_space = starts.get();
    key(slint::platform::Key::Return.into());
    let after_keyboard = starts.get();
    println!(
        "record callbacks: before=3 after-default=3 after-Space={after_space} after-Return={after_keyboard}"
    );
    // Both controls occupy the same terminal slot. A physical click there
    // must send rather than start recording, even though the mic is mounted.
    let before = sends.get();
    click(w, "Record voice message");
    assert_eq!(
        starts.get(),
        after_keyboard,
        "transparent microphone intercepted pointer input"
    );
    assert_eq!(sends.get(), before + 1, "terminal-slot pointer must send");
    send.invoke_accessible_default_action();
    assert_eq!(sends.get(), before + 2, "Send accessible action must work");
    println!("terminal-slot pointer and accessible Send callbacks both passed");
    assert_eq!(
        after_space, 3,
        "inactive microphone accepted Space activation"
    );
    assert_eq!(
        after_keyboard, 3,
        "inactive microphone accepted Return activation"
    );
}

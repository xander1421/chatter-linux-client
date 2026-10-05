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
    // Re-entering run() after quit returns immediately. Drive real frames and
    // wall-clock time so size/focus/height animations settle on every pump.
    for _ in 0..12 {
        w.window().dispatch_event(slint::platform::WindowEvent::Resized {
            size: slint::LogicalSize::new(width, height),
        });
        slint::Timer::single_shot(std::time::Duration::from_millis(1), || {
            slint::quit_event_loop().unwrap();
        });
        w.run().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

fn click(w: &MainWindow, label: &str) {
    let element = testing::ElementHandle::find_by_accessible_label(w, label).next().unwrap();
    let pos = element.absolute_position();
    let size = element.size();
    let position = slint::LogicalPosition::new(pos.x + size.width / 2.0, pos.y + size.height / 2.0);
    w.window().dispatch_event(slint::platform::WindowEvent::PointerPressed {
        position, button: slint::platform::PointerEventButton::Left,
    });
    w.window().dispatch_event(slint::platform::WindowEvent::PointerReleased {
        position, button: slint::platform::PointerEventButton::Left,
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
    w.set_thread_panel_visible(true);
    w.set_thread_root_sender_name("Test Sender".into());
    w.set_thread_root_content_preview("root message".into());
    w.set_channel_name("test-channel".into());
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
    w.set_channel_name("Composer geometry".into());
    w.set_mobile_show_chat(true);
    w.set_composer_text("Draft stays here".into());
    for (mobile, width, height) in [(true, 320.0, 800.0), (true, 360.0, 480.0), (false, 1200.0, 900.0)] {
        w.set_is_mobile_platform(mobile);
        w.window().set_size(slint::LogicalSize::new(width, height));
        pump_at(&w, width, height);
        click(&w, "Message input");
        w.window().dispatch_event(slint::platform::WindowEvent::KeyPressed { text: slint::platform::Key::End.into() });
        w.window().dispatch_event(slint::platform::WindowEvent::KeyPressed { text: slint::platform::Key::LeftArrow.into() });
        click(&w, "Emoji & GIF picker");
        pump_at(&w, width, height);
        assert!(w.get_composer_emoji_picker_visible());
        assert!((w.window().size().to_logical(w.window().scale_factor()).width - width).abs() < 1.0);
        w.invoke_insert_composer_emoji("😀".into());
        pump_at(&w, width, height);
        assert_eq!(w.get_composer_text().as_str(), "Draft stays her😀e");
        click(&w, "Show keyboard");
        pump_at(&w, width, height);
        assert!(!w.get_composer_emoji_picker_visible());
        w.window().dispatch_event(slint::platform::WindowEvent::KeyPressed { text: slint::platform::Key::Backspace.into() });
        assert_eq!(w.get_composer_text().as_str(), "Draft stays here");
        w.set_composer_emoji_picker_visible(true);
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
        assert!(emoji.size().width >= chat.size().width - 50.0);
        assert!(ep.x >= cp.x && ep.x + emoji.size().width <= cp.x + chat.size().width + 1.0);
        assert!(ep.y >= composer.absolute_position().y + composer.size().height - 1.0);
        assert!(ep.y + emoji.size().height <= cp.y + chat.size().height + 1.0);
        w.set_composer_emoji_picker_visible(false);
        w.set_composer_gif_picker_visible(true);
        pump_at(&w, width, height);
        let gif = testing::ElementHandle::find_by_element_type_name(&w, "GifPicker")
            .next()
            .unwrap();
        assert!(gif.size().width >= chat.size().width - 50.0);
        assert!(gif.absolute_position().y + gif.size().height <= cp.y + chat.size().height + 1.0);
        w.set_composer_gif_picker_visible(false);
        pump_at(&w, width, height);
        assert!(
            testing::ElementHandle::find_by_accessible_label(&w, "Record voice message")
                .next()
                .is_none()
        );
        assert!(
            testing::ElementHandle::find_by_accessible_label(&w, "Send message")
                .next()
                .is_some()
        );
        w.set_composer_text("".into());
        pump_at(&w, width, height);
        assert!(
            testing::ElementHandle::find_by_accessible_label(&w, "Record voice message")
                .next()
                .is_some()
        );
        assert!(
            testing::ElementHandle::find_by_accessible_label(&w, "Send message")
                .next()
                .is_none()
        );
        w.set_composer_text("Draft stays here".into());
        w.global::<VoiceRecording>().set_active(true);
        w.global::<VoiceRecording>().set_locked(true);
        pump_at(&w, width, height);
        let composer = testing::ElementHandle::find_by_element_type_name(&w, "MessageComposer")
            .next()
            .unwrap();
        assert!(composer.size().height >= 112.0);
        let mut last_right = composer.absolute_position().x;
        for label in [
            "Cancel voice message",
            "Lock recording",
            "Send voice message",
        ] {
            let control = testing::ElementHandle::find_by_accessible_label(&w, label)
                .next()
                .unwrap();
            assert!(control.size().width >= 48.0 && control.size().height >= 48.0);
            assert!(control.absolute_position().x >= last_right);
            last_right = control.absolute_position().x + control.size().width;
        }
        assert_eq!(w.get_composer_text().as_str(), "Draft stays here");
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
        assert_eq!(w.get_composer_text().as_str(), "Draft stays here");
        w.global::<VoiceRecording>().set_active(false);
        pump_at(&w, width, height);
        click(&w, "Message input");
        w.set_selection_mode(true);
        pump_at(&w, width, height);
        w.window().dispatch_event(slint::platform::WindowEvent::KeyPressed { text: "x".into() });
        assert_eq!(w.get_composer_text().as_str(), "Draft stays here", "hidden composer retained typing focus");
        w.set_selection_mode(false);
        w.set_mobile_show_chat(false);
        pump_at(&w, width, height);
        w.set_mobile_show_chat(true);
        pump_at(&w, width, height);
        assert_eq!(w.get_composer_text().as_str(), "Draft stays here", "mount replayed the previous emoji insertion");
    }
}

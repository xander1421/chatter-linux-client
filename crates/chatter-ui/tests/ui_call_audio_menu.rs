//! In-call audio menu availability, pointer wiring, and touch target geometry.
use chatter_ui::MainWindow;
use i_slint_backend_testing as testing;
use slint::ComponentHandle;

fn pump_at(w: &MainWindow, width: f32, height: f32) {
    w.window().set_size(slint::LogicalSize::new(width, height));
    // Re-entering run() after quit returns immediately. Drive real frames and
    // wall-clock time so size/focus/height animations settle on every pump.
    for _ in 0..12 {
        w.window()
            .dispatch_event(slint::platform::WindowEvent::Resized {
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

/// Exercise the real dock -> menu -> picker callback path with pointer clicks.
/// No client or audio backend is created: callbacks stand in for enumeration.
#[test]
fn call_audio_menu_is_reachable_at_both_densities() {
    assert!(chatter_ui::SLINT_DEBUG_INFO_ENABLED);
    testing::init_integration_test_with_system_time();
    use chatter_ui::CallView;
    use std::{cell::Cell, rc::Rc};

    let w = MainWindow::new().unwrap();
    w.set_current_view("main".into());
    w.global::<chatter_ui::SidebarPane>()
        .set_mobile_show_chat(true);
    w.global::<chatter_ui::ChatPane>()
        .set_channel_name("Call test".into());
    let view = w.global::<CallView>();
    view.set_active_call_visible(true);
    let mut data = view.get_active_call_data();
    data.call_id = "audio-menu-test".into();
    data.title = "Call test".into();
    data.is_connected = true;
    view.set_active_call_data(data);
    let inputs = Rc::new(Cell::new(0));
    let outputs = Rc::new(Cell::new(0));
    for output in [false, true] {
        let weak = w.as_weak();
        let count = if output {
            outputs.clone()
        } else {
            inputs.clone()
        };
        let show = move || {
            count.set(count.get() + 1);
            let w = weak.upgrade().unwrap();
            let view = w.global::<CallView>();
            view.set_device_picker_type(
                if output || view.get_call_speaker_toggle() {
                    "audio_output"
                } else {
                    "audio"
                }
                .into(),
            );
            view.set_device_picker_visible(true);
        };
        if output {
            view.on_show_audio_output_picker(show);
        } else {
            view.on_show_audio_device_picker(show);
        }
    }
    for (linked, width) in [(false, 1400.0), (true, 360.0)] {
        view.set_call_speaker_toggle(linked);
        for fullscreen in [false, true] {
            view.set_active_call_fullscreen(fullscreen);
            for video in [false, true] {
                view.set_call_is_video_enabled(video);
                pump_at(&w, width, 800.0);
                let audio_label = if linked {
                    "Audio devices, microphone and output linked"
                } else {
                    "Audio devices"
                };
                for label in [
                    audio_label,
                    "Mute microphone",
                    if video {
                        "Turn camera off"
                    } else {
                        "Turn camera on"
                    },
                    "Share screen",
                    "End call",
                ] {
                    let element = testing::ElementHandle::find_by_accessible_label(&w, label)
                        .next()
                        .unwrap_or_else(|| panic!("Missing {label}"));
                    let pos = element.absolute_position();
                    let size = element.size();
                    assert!(
                        size.width >= 44.0 && size.height >= 44.0,
                        "Small target {label}: {size:?}"
                    );
                    assert!(
                        pos.x >= 0.0
                            && pos.y >= 0.0
                            && pos.x + size.width <= width + 0.5
                            && pos.y + size.height <= 800.5,
                        "Offscreen {label}: {pos:?} {size:?}"
                    );
                }
                let before = inputs.get();
                click(&w, audio_label);
                pump_at(&w, width, 800.0);
                assert!(view.get_device_picker_visible());
                if linked {
                    assert_eq!(inputs.get(), before + 1);
                    assert_eq!(view.get_device_picker_type().as_str(), "audio_output");
                } else {
                    assert_eq!(view.get_device_picker_type().as_str(), "audio_menu");
                    click(&w, "Microphone (input)");
                    assert_eq!(inputs.get(), before + 1);
                    assert_eq!(view.get_device_picker_type().as_str(), "audio");
                    view.set_device_picker_visible(false);
                    pump_at(&w, width, 800.0);
                    click(&w, audio_label);
                    pump_at(&w, width, 800.0);
                    let before = outputs.get();
                    click(&w, "Speaker or headphones (output)");
                    assert_eq!(outputs.get(), before + 1);
                    assert_eq!(view.get_device_picker_type().as_str(), "audio_output");
                }
                view.set_device_picker_visible(false);
                pump_at(&w, width, 800.0);
            }
        }
    }
}

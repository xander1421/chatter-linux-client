//! In-call audio arrows: direct picker wiring and non-overlapping pointer targets.
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

/// Exercise the real dock -> picker callbacks with pointer clicks.
/// No client or audio backend is created: callbacks stand in for enumeration.
#[test]
fn call_audio_arrows_are_independent_at_both_densities() {
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
    view.set_audio_input_name("Antlion USB Microphone long device name".into());
    view.set_audio_output_name("Scarlett Solo Headphones long device name".into());
    let refreshes = Rc::new(Cell::new(0));
    {
        let count = refreshes.clone();
        let weak = w.as_weak();
        view.on_refresh_audio_controls(move || {
            count.set(count.get() + 1);
            let w = weak.upgrade().unwrap();
            let view = w.global::<CallView>();
            let linked = view.get_call_speaker_toggle();
            view.set_audio_input_name(
                if linked {
                    "Speakerphone"
                } else {
                    "Antlion USB Microphone long device name"
                }
                .into(),
            );
            view.set_audio_output_name(
                if linked {
                    "Speakerphone"
                } else {
                    "Scarlett Solo Headphones long device name"
                }
                .into(),
            );
        });
    }
    let ends = Rc::new(Cell::new(0));
    {
        let count = ends.clone();
        let weak = w.as_weak();
        view.on_call_end(move || {
            count.set(count.get() + 1);
            weak.upgrade()
                .unwrap()
                .global::<CallView>()
                .set_active_call_visible(false);
        });
    }
    let mutes = Rc::new(Cell::new(0));
    let speakers = Rc::new(Cell::new(0));
    let routes = Rc::new(Cell::new(0));
    {
        let count = mutes.clone();
        let weak = w.as_weak();
        view.on_call_toggle_mute(move || {
            count.set(count.get() + 1);
            let w = weak.upgrade().unwrap();
            let view = w.global::<CallView>();
            view.set_call_is_muted(!view.get_call_is_muted());
        });
        let count = speakers.clone();
        view.on_call_toggle_speaker(move || count.set(count.get() + 1));
        let count = routes.clone();
        view.on_audio_device_selected(move |_| count.set(count.get() + 1));
        let count = routes.clone();
        view.on_audio_output_selected(move |_| count.set(count.get() + 1));
    }
    view.set_audio_devices(slint::ModelRc::new(slint::VecModel::from(vec![
        chatter_ui::AudioDeviceData {
            id: "input-1".into(),
            name: "Test microphone".into(),
            is_default: true,
        },
    ])));
    view.set_audio_output_devices(slint::ModelRc::new(slint::VecModel::from(vec![
        chatter_ui::AudioDeviceData {
            id: "output-1".into(),
            name: "Test output".into(),
            is_default: true,
        },
    ])));
    for (linked, width) in [
        (false, 1400.0),
        (false, 400.0),
        (false, 368.0),
        (false, 320.0),
        (true, 360.0),
        (true, 320.0),
    ] {
        w.set_is_mobile_platform(linked);
        view.set_call_speaker_toggle(linked);
        let input_name = if linked {
            "Speakerphone"
        } else {
            "Antlion USB Microphone long device name"
        };
        let output_name = if linked {
            "Speakerphone"
        } else {
            "Scarlett Solo Headphones long device name"
        };
        view.set_audio_input_name(input_name.into());
        view.set_audio_output_name(output_name.into());
        for fullscreen in [false, true] {
            view.set_active_call_fullscreen(fullscreen);
            for video in [false, true] {
                view.set_call_is_video_enabled(video);
                pump_at(&w, width, 800.0);
                let input_label = if linked {
                    "Select microphone, linked audio route"
                } else {
                    "Select microphone"
                };
                let output_label = if linked {
                    "Select output, linked audio route"
                } else {
                    "Select output"
                };
                let speaker_label = if linked {
                    "Toggle speaker"
                } else {
                    "Select audio output"
                };
                let mut boxes: Vec<(&str, f32, f32, f32, f32)> = Vec::new();
                for label in [
                    "Mute microphone",
                    input_label,
                    speaker_label,
                    output_label,
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
                        "Offscreen {label}: {pos:?} {size:?}; linked={linked} width={width} fullscreen={fullscreen} video={video}"
                    );
                    for (other, x, y, right, bottom) in &boxes {
                        assert!(
                            pos.x >= *right - 0.5
                                || pos.x + size.width <= *x + 0.5
                                || pos.y >= *bottom - 0.5
                                || pos.y + size.height <= *y + 0.5,
                            "Overlapping targets {label} and {other}"
                        );
                        assert!(
                            (pos.y - y).abs() < 0.5,
                            "Controls must stay on a single row"
                        );
                    }
                    boxes.push((label, pos.x, pos.y, pos.x + size.width, pos.y + size.height));
                }
                assert!(
                    testing::ElementHandle::find_by_accessible_label(&w, "Audio devices")
                        .next()
                        .is_none()
                );
                let mute_before = mutes.get();
                let input_before = inputs.get();
                let output_before = outputs.get();
                click(&w, "Mute microphone");
                assert_eq!(mutes.get(), mute_before + 1);
                assert_eq!((inputs.get(), outputs.get()), (input_before, output_before));
                assert!(!view.get_device_picker_visible());
                assert!(view.get_call_is_muted());
                click(&w, "Unmute microphone");
                assert_eq!(mutes.get(), mute_before + 2);
                assert!(!view.get_call_is_muted());
                assert_eq!((inputs.get(), outputs.get()), (input_before, output_before));
                for (label, output) in [(input_label, false), (output_label, true)] {
                    let speaker_before = speakers.get();
                    let before = if output { outputs.get() } else { inputs.get() };
                    let refreshed = refreshes.get();
                    click(&w, label);
                    pump_at(&w, width, 800.0);
                    assert!(view.get_audio_controls_visible());
                    assert!(!view.get_device_picker_visible());
                    assert_eq!(refreshes.get(), refreshed + 1);
                    let input_pill = format!("Microphone: {input_name}");
                    let output_pill = format!("Output: {output_name}");
                    for pill in [&input_pill, &output_pill] {
                        let element = testing::ElementHandle::find_by_accessible_label(&w, pill)
                            .next()
                            .unwrap();
                        let pos = element.absolute_position();
                        let size = element.size();
                        assert!(size.width >= 44.0 && size.height >= 44.0);
                        assert!(pos.x >= 0.0 && pos.x + size.width <= width + 0.5);
                        assert!(
                            pos.y >= 0.0 && pos.y + size.height <= boxes[0].2 + 0.5,
                            "Popover must sit above controls: {pos:?} {size:?}"
                        );
                    }
                    click(&w, if output { &output_pill } else { &input_pill });
                    pump_at(&w, width, 800.0);
                    assert!(!view.get_audio_controls_visible());
                    assert!(view.get_device_picker_visible());
                    assert_eq!(
                        if output { outputs.get() } else { inputs.get() },
                        before + 1
                    );
                    assert_eq!(
                        view.get_device_picker_type().as_str(),
                        if output || linked {
                            "audio_output"
                        } else {
                            "audio"
                        }
                    );
                    let route_before = routes.get();
                    click(
                        &w,
                        if output || linked {
                            "Test output"
                        } else {
                            "Test microphone"
                        },
                    );
                    assert_eq!(routes.get(), route_before + 1);
                    assert_eq!(speakers.get(), speaker_before);
                    assert_eq!(ends.get(), 0);
                    assert!(!view.get_device_picker_visible());
                    assert_eq!(mutes.get(), mute_before + 2);
                    pump_at(&w, width, 800.0);
                }
                let speaker_before = speakers.get();
                let output_before = outputs.get();
                click(&w, speaker_label);
                if linked {
                    assert_eq!(speakers.get(), speaker_before + 1);
                    assert_eq!(outputs.get(), output_before);
                    assert!(!view.get_device_picker_visible());
                } else {
                    assert_eq!(speakers.get(), speaker_before);
                    assert_eq!(outputs.get(), output_before + 1);
                    assert!(view.get_device_picker_visible());
                    assert_eq!(view.get_device_picker_type().as_str(), "audio_output");
                    view.set_device_picker_visible(false);
                }
                pump_at(&w, width, 800.0);
                for key in [slint::platform::Key::Escape, slint::platform::Key::Back] {
                    click(&w, input_label);
                    pump_at(&w, width, 800.0);
                    assert!(view.get_audio_controls_visible());
                    let text: slint::SharedString = key.into();
                    w.window()
                        .dispatch_event(slint::platform::WindowEvent::KeyPressed {
                            text: text.clone(),
                        });
                    w.window()
                        .dispatch_event(slint::platform::WindowEvent::KeyReleased { text });
                    assert!(!view.get_audio_controls_visible());
                    assert!(view.get_active_call_visible());
                }
                click(&w, input_label);
                pump_at(&w, width, 800.0);
                click(&w, "End call"); // Outside popover dismisses without activating the covered action.
                assert!(!view.get_audio_controls_visible());
                assert_eq!(ends.get(), 0);
                click(&w, input_label);
                assert!(chatter_ui::bin_ui::features::back::handle_back(&w));
                assert!(!view.get_audio_controls_visible());
                click(&w, input_label);
                view.set_active_call_visible(false);
                pump_at(&w, width, 800.0);
                assert!(!view.get_audio_controls_visible());
                view.set_active_call_visible(true);
                pump_at(&w, width, 800.0);
            }
        }
    }
    click(&w, "End call");
    pump_at(&w, 320.0, 800.0);
    assert_eq!(ends.get(), 1);
    assert!(!view.get_active_call_visible());
    assert!(!view.get_audio_controls_visible());
}

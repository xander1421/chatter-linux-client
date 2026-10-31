//! Software renderer captures of the live call surfaces at phone and desktop widths.
use i_slint_backend_testing::{self as testing, TestingBackend, TestingBackendOptions};
use slint::{ComponentHandle, Model};

slint::slint! {
    import { IncomingCallModal } from "../../ui/calls/incoming_call_modal.slint";
    import { DevicePicker } from "../../ui/calls/device_picker.slint";
    import { ReturnToCallBar } from "../../ui/calls/return_to_call_bar.slint";
    import { ActiveCallView } from "../../ui/calls/active_call_view.slint";
    import { IncomingCallData, ActiveCallData, CallParticipantData } from "../../ui/calls/types.slint";
    import { Colors } from "../../ui/theme/colors.slint";
    import { Appearance } from "../../ui/theme/appearance.slint";

    export component CallsHarness inherits Window {
        in property <bool> desktop: false;
        width: desktop ? 1024px : 412px;
        height: 760px;
        background: Colors.background;
        in property <IncomingCallData> incoming;
        in property <ActiveCallData> active;
        in property <int> scene: 0;
        callback theme(string);
        callback motion(bool);
        callback motion-speed(float);
        pure callback select-tiles([CallParticipantData], string) -> [int];
        theme(value) => { Appearance.theme = value; }
        motion(value) => { Appearance.reduce-motion = value; }
        motion-speed(value) => { Appearance.animation-speed = value; }

        Rectangle {
            background: Colors.background;
            Text {
                text: "Calls";
                x: 24px;
                y: 36px;
                color: Colors.text-primary;
                font-size: 24px;
            }
        }

        ReturnToCallBar {
            x: 24px;
            y: 96px;
            width: min(360px, parent.width - 48px);
            call-data: root.active;
            visible: root.scene == 0;
        }

        IncomingCallModal {
            show: root.scene == 1;
            call-data: root.incoming;
        }

        DevicePicker {
            show: root.scene == 2;
            title: "Audio input";
            picker-type: "audio";
            audio-devices: [
                { id: "built-in", name: "Built-in microphone", is-default: true },
                { id: "studio", name: "Studio USB microphone", is-default: false },
            ];
            selected-audio-id: "built-in";
        }

        ActiveCallView {
            x: 0;
            y: 0;
            width: parent.width;
            height: 440px;
            viewport-height: parent.height;
            show: root.scene == 3;
            call-data: root.active;
            select-tile-indices(participants, pinned) => { return root.select-tiles(participants, pinned); }
            speaker-toggle: parent.width < 600px;
            is-video-enabled: true;
        }
    }
}

fn advance(ms: u64) {
    let mut remaining = ms;
    while remaining > 0 {
        let step = remaining.min(16);
        testing::testing_backend::mock_elapsed_time(step);
        slint::platform::update_timers_and_animations();
        remaining -= step;
    }
}

fn frame(window: &CallsHarness, name: &str) {
    // Render every checkpoint even when PNG capture is disabled. Slint only
    // evaluates layout and starts geometry animations on a render pass.
    let pixels = window.window().take_snapshot().unwrap();
    let Some(dir) = std::env::var_os("U9b_CAPTURE_DIR") else {
        return;
    };
    image::save_buffer(
        std::path::Path::new(&dir).join(name),
        pixels.as_bytes(),
        pixels.width(),
        pixels.height(),
        image::ColorType::Rgba8,
    )
    .unwrap();
}

#[test]
fn calls_arrive_and_settle_in_both_themes_and_widths() {
    slint::platform::set_platform(Box::new(TestingBackend::new(TestingBackendOptions {
        mock_time: true,
        threading: false,
        renderer_name: Some("software".into()),
    })))
    .unwrap();
    let window = CallsHarness::new().unwrap();
    // Sample the standard 320ms transition at 80ms. The app's current 4x
    // default otherwise finishes it at the first measured frame.
    window.invoke_motion_speed(1.0);
    window.on_select_tiles(|participants, pinned| {
        let participants: Vec<_> = participants.iter().collect();
        let indices = chatter_core::calls::tiles::select_visible_tiles(
            &participants,
            pinned.as_str(),
            |participant| participant.user_id.as_str(),
            |participant| participant.is_screen_sharing,
            |participant| participant.is_speaking,
        );
        slint::ModelRc::new(slint::VecModel::from(
            indices.into_iter().map(|index| index as i32).collect::<Vec<_>>(),
        ))
    });
    window.show().unwrap();
    window.set_incoming(IncomingCallData {
        caller_name: "Morgan Rivera".into(),
        caller_initials: "MR".into(),
        is_video_call: true,
        ..Default::default()
    });
    let active = |count: i32| {
        let mut peers = vec![CallParticipantData {
            display_name: "Morgan Rivera".into(),
            initials: "MR".into(),
            ..Default::default()
        }];
        if count == 3 {
            peers.push(CallParticipantData {
                display_name: "Avery".into(),
                initials: "AV".into(),
                ..Default::default()
            });
        }
        peers.push(CallParticipantData {
            display_name: "You".into(),
            initials: "YO".into(),
            ..Default::default()
        });
        ActiveCallData {
            title: "Morgan Rivera".into(),
            is_connected: true,
            duration_seconds: 84,
            participant_count: count,
            participants: slint::ModelRc::new(slint::VecModel::from(peers)),
            ..Default::default()
        }
    };
    for (device, width) in [("phone", 412.0), ("desktop", 1024.0)] {
        window.set_desktop(device == "desktop");
        window
            .window()
            .set_size(slint::LogicalSize::new(width, 760.0));
        for theme in ["light", "dark"] {
            window.invoke_theme(theme.into());
            window.set_active(active(2));
            window.set_scene(0);
            advance(500);
            frame(&window, &format!("{device}-{theme}-01-return.png"));
            window.set_scene(1);
            advance(80);
            frame(&window, &format!("{device}-{theme}-02-ringing-arrival.png"));
            let arrival = window.window().take_snapshot().unwrap().as_bytes().to_vec();
            advance(520);
            frame(&window, &format!("{device}-{theme}-03-ringing-settled.png"));
            let settled = window.window().take_snapshot().unwrap().as_bytes().to_vec();
            assert!(arrival != settled, "ringing card must move between frames");
            window.set_scene(2);
            advance(500);
            frame(&window, &format!("{device}-{theme}-04-device-sheet.png"));
            window.set_scene(3);
            advance(500);
            frame(&window, &format!("{device}-{theme}-05-active-call.png"));
            window.set_active(active(3));
            // Evaluate the new roster at the transition boundary. Mock time
            // alone does not render it; without this snapshot the animation
            // starts at the first measured frame, 80 ms too late.
            let _ = window.window().take_snapshot().unwrap();
            advance(80);
            frame(&window, &format!("{device}-{theme}-06-grid-travel.png"));
            let travel = window.window().take_snapshot().unwrap().as_bytes().to_vec();
            advance(500);
            frame(&window, &format!("{device}-{theme}-07-grid-settled.png"));
            let grid = window.window().take_snapshot().unwrap().as_bytes().to_vec();
            assert!(travel != grid, "participant grid must settle after reflow");
        }
    }
}

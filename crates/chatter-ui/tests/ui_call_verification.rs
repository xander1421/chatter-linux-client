//! Actual active-call header geometry, using the native Slint testing backend.
use chatter_ui::{ActiveCallData, Appearance, MainWindow};
use i_slint_backend_testing as testing;
use slint::ComponentHandle;

fn pump(w: &MainWindow) {
    w.window().set_size(slint::LogicalSize::new(412.0, 891.0));
    for _ in 0..12 {
        w.window()
            .dispatch_event(slint::platform::WindowEvent::Resized {
                size: slint::LogicalSize::new(412.0, 891.0),
            });
        slint::Timer::single_shot(std::time::Duration::from_millis(1), || {
            slint::quit_event_loop().unwrap();
        });
        w.run().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

fn visible(w: &MainWindow, label: &str) -> Vec<testing::ElementHandle> {
    testing::ElementHandle::find_by_accessible_label(w, label)
        .filter(|e| e.size().width > 0.0 && e.size().height > 0.0)
        .collect()
}

#[test]
fn call_header_keeps_four_ordered_emojis_and_key_version_across_presentations() {
    assert!(
        chatter_ui::SLINT_DEBUG_INFO_ENABLED,
        "requires ui-test-debug"
    );
    testing::init_integration_test_with_system_time();
    let w = MainWindow::new().unwrap();
    w.set_current_view("main".into());
    w.set_is_mobile_platform(true);
    w.global::<chatter_ui::SidebarPane>()
        .set_mobile_show_chat(true);
    w.global::<chatter_ui::ChatPane>()
        .set_channel_name("Verification fixture".into());
    w.global::<chatter_ui::CallView>()
        .set_active_call_visible(true);
    w.global::<chatter_ui::CallView>()
        .set_call_is_video_enabled(false);
    let emojis = ["🐱", "🌳", "🚀", "🍋"];
    for incoming in [false, true] {
        for theme in ["light", "dark"] {
            w.global::<Appearance>().set_theme(theme.into());
            for expanded in [false, true] {
                w.global::<chatter_ui::CallView>()
                    .set_active_call_fullscreen(expanded);
                w.global::<chatter_ui::SidebarPane>()
                    .set_active_tab("chats".into());
                // Match the incoming connected / outgoing pending presentations.
                let data = ActiveCallData {
                    call_id: "verification-fixture".into(),
                    title: "Verification fixture".into(),
                    participant_count: 3,
                    is_connected: incoming,
                    is_encrypted: true,
                    verification_emoji_1: emojis[0].into(),
                    verification_emoji_2: emojis[1].into(),
                    verification_emoji_3: emojis[2].into(),
                    verification_emoji_4: emojis[3].into(),
                    verification_key_version: "4294967295".into(),
                    ..Default::default()
                };
                w.global::<chatter_ui::CallView>()
                    .set_active_call_data(data.clone());
                pump(&w);
                let mut previous_right: Option<f32> = None;
                let mut baseline: Option<f32> = None;
                for (index, emoji) in emojis.iter().enumerate() {
                    let elements = visible(
                        &w,
                        &format!("Call verification emoji {}: {emoji}", index + 1),
                    );
                    assert_eq!(
                        elements.len(),
                        1,
                        "{theme} expanded={expanded} incoming={incoming}"
                    );
                    let element = &elements[0];
                    let position = element.absolute_position();
                    let size = element.size();
                    assert!(position.x >= 0.0 && position.y >= 0.0);
                    assert!(position.x + size.width <= 412.5 && position.y + size.height <= 891.5);
                    if let Some(right) = previous_right {
                        assert!(position.x >= right - 0.5);
                    }
                    let center = position.y + size.height / 2.0;
                    if let Some(y) = baseline {
                        assert!((center - y).abs() < 1.0);
                    }
                    baseline = Some(center);
                    previous_right = Some(position.x + size.width);
                }
                let version = visible(&w, "Call key version 4294967295");
                assert_eq!(version.len(), 1);
                let position = version[0].absolute_position();
                assert!(position.x >= previous_right.unwrap() - 0.5);
                assert!(position.x + version[0].size().width <= 412.5);
                assert!(
                    (position.y + version[0].size().height / 2.0 - baseline.unwrap()).abs() < 1.0
                );
                // Pending removes all comparison slots and generation together.
                let mut pending = data;
                pending.verification_emoji_1 = "".into();
                pending.verification_emoji_2 = "".into();
                pending.verification_emoji_3 = "".into();
                pending.verification_emoji_4 = "".into();
                pending.verification_key_version = "".into();
                w.global::<chatter_ui::CallView>()
                    .set_active_call_data(pending);
                pump(&w);
                for (index, emoji) in emojis.iter().enumerate() {
                    assert!(
                        visible(
                            &w,
                            &format!("Call verification emoji {}: {emoji}", index + 1)
                        )
                        .is_empty()
                    );
                }
                assert!(visible(&w, "Call key version 4294967295").is_empty());
            }
        }
    }
}

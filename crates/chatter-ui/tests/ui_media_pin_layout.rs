//! Real phone layout: media actions remain reachable and mixed-height seeks land.
use chatter_ui::{
    ConversationKind, MainWindow, MessageAttachmentData, MessageItemData, ReactionEntry,
};
use i_slint_backend_testing as testing;
use slint::{ComponentHandle, Model, ModelRc, VecModel};

fn pump(w: &MainWindow, width: f32) {
    w.window().set_size(slint::LogicalSize::new(width, 891.0));
    w.show().unwrap();
    for _ in 0..20 {
        w.window()
            .dispatch_event(slint::platform::WindowEvent::Resized {
                size: slint::LogicalSize::new(width, 891.0),
            });
        slint::Timer::single_shot(std::time::Duration::from_millis(20), || {
            slint::quit_event_loop().unwrap();
        });
        w.run().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}
fn visible(w: &MainWindow, label: &str) -> testing::ElementHandle {
    testing::ElementHandle::find_by_accessible_label(w, label)
        .find(|e| e.size().width > 0.0 && e.size().height > 0.0)
        .unwrap_or_else(|| {
            panic!(
                "Missing visible {label}; scroll={}, estimated rows={}..{}",
                w.get_message_scroll_position(),
                w.get_message_first_visible_row(),
                w.get_message_last_visible_row()
            )
        })
}
#[test]
fn phone_media_actions_fit_and_mixed_height_pin_seek_lands_at_header() {
    assert!(chatter_ui::SLINT_DEBUG_INFO_ENABLED);
    testing::init_integration_test_with_system_time();
    let w = MainWindow::new().unwrap();
    // A delete delivered to the event queue must not affect a new chat that
    // happens to contain the same sender/timestamp. Use the real event queue.
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let _runtime = runtime.enter();
    let fixture = tempfile::tempdir().unwrap();
    // Off the real keyring: the marker must exist before the client opens.
    let db_path = chatter_core::system::Directories::with_base_dir(fixture.path().to_path_buf())
        .unwrap()
        .database_path();
    std::fs::write(
        db_path
            .parent()
            .unwrap()
            .join(chatter_core::system::keyring::NO_KEYRING_MARKER),
        b"",
    )
    .unwrap();
    let client = chatter_core::client::ChatterClient::builder()
        .data_dir(fixture.path().to_string_lossy().to_string())
        .build()
        .unwrap();
    let a = chatter_core::state::ActiveConversation::Channel(
        chatter_core::domain::ids::ChannelId::new(),
    );
    let b = chatter_core::state::ActiveConversation::Channel(
        chatter_core::domain::ids::ChannelId::new(),
    );
    client.select_conversation(a);
    w.set_messages(ModelRc::new(VecModel::from(vec![MessageItemData {
        message_id: "collision".into(),
        ..Default::default()
    }])));
    let queued_client = client.clone();
    w.as_weak()
        .upgrade_in_event_loop(move |w| {
            chatter_ui::bin_ui::features::chat::remove_messages_in_conversation(
                &w,
                &queued_client,
                a,
                |row| row.message_id == "collision",
            );
        })
        .unwrap();
    client.select_conversation(b);
    pump(&w, 412.0);
    assert_eq!(
        w.get_messages().row_count(),
        1,
        "Queued old-chat delete must preserve the new chat"
    );
    let queued_client = client.clone();
    w.as_weak()
        .upgrade_in_event_loop(move |w| {
            chatter_ui::bin_ui::features::chat::remove_messages_in_conversation(
                &w,
                &queued_client,
                b,
                |row| row.message_id == "collision",
            );
        })
        .unwrap();
    pump(&w, 412.0);
    assert_eq!(
        w.get_messages().row_count(),
        0,
        "Current-chat deletion must still remove the bubble"
    );
    w.set_current_view("main".into());
    w.set_is_mobile_platform(true);
    w.set_mobile_show_chat(true);
    w.set_conversation_kind(ConversationKind::Channel);
    w.set_channel_name("Owned layout fixture".into());
    w.set_media_viewer_has_previous(true);
    w.set_media_viewer_has_next(true);
    w.set_media_viewer_position("11 / 12".into());
    w.set_image_viewer_filename("very-long-peer-filename-".repeat(20).into());
    w.set_video_player_filename("very-long-peer-filename-".repeat(20).into());
    for width in [360.0, 412.0] {
        for video in [false, true] {
            w.set_image_viewer_visible(!video);
            w.set_video_player_visible(video);
            pump(&w, width);
            let labels = if video {
                vec!["Previous media", "Next media", "Download", "Open downloads"]
            } else {
                vec![
                    "Previous media",
                    "Next media",
                    "Download",
                    "Open downloads",
                    "Copy image",
                ]
            };
            let mut boxes = Vec::new();
            for label in labels {
                let e = visible(&w, label);
                let p = e.absolute_position();
                let s = e.size();
                assert!(
                    p.x >= -0.5 && p.x + s.width <= width + 0.5,
                    "{label} outside {width}px: x={}, width={}",
                    p.x,
                    s.width
                );
                assert!(p.y >= 0.0 && p.y + s.height <= 891.5);
                boxes.push((label, p.x, p.y, s.width, s.height));
            }
            for a in &boxes {
                for b in &boxes {
                    if a.0 != b.0 {
                        assert!(
                            a.1 + a.3 <= b.1 + 0.5
                                || b.1 + b.3 <= a.1 + 0.5
                                || a.2 + a.4 <= b.2 + 0.5
                                || b.2 + b.4 <= a.2 + 0.5,
                            "Media buttons overlap: {} and {}",
                            a.0,
                            b.0
                        );
                    }
                }
            }
        }
    }
    w.set_image_viewer_visible(false);
    w.set_video_player_visible(false);
    let rows: Vec<_> = (0..140)
        .map(|i| MessageItemData {
            message_id: format!("geometry-{i}").into(),
            sender_id: "fixture".into(),
            sender_name: "Fixture".into(),
            content: if i % 4 == 0 {
                "long text across several wrapped lines ".repeat(18).into()
            } else {
                format!("target-{i}").into()
            },
            timestamp: format!("clock-{i}").into(),
            is_call: i % 8 == 1,
            has_attachments: matches!(i % 8, 2..=5),
            attachment_count: if matches!(i % 8, 2..=5) { 1 } else { 0 },
            attachments: ModelRc::new(VecModel::from(match i % 8 {
                kind @ 2..=5 => vec![MessageAttachmentData {
                    id: format!("geometry-attachment-{i}").into(),
                    filename: format!("owned-fixture-{i}").into(),
                    category: match kind {
                        3 => "video",
                        4 => "audio",
                        _ => "image",
                    }
                    .into(),
                    mime_type: match kind {
                        3 => "video/mp4",
                        4 => "audio/wav",
                        5 => "image/gif",
                        _ => "image/png",
                    }
                    .into(),
                    is_image: kind == 2 || kind == 5,
                    is_video: kind == 3,
                    width: if i % 2 == 0 { 320 } else { 640 },
                    height: if i % 2 == 0 { 480 } else { 360 },
                    duration_display: "0:37".into(),
                    waveform: ModelRc::new(VecModel::from(vec![45; 32])),
                    ..Default::default()
                }],
                _ => Vec::new(),
            })),
            has_reactions: i % 6 == 0,
            reactions: ModelRc::new(VecModel::from(if i % 6 == 0 {
                vec![ReactionEntry {
                    emoji: "👍".into(),
                    count: 2,
                    own: false,
                }]
            } else {
                Vec::new()
            })),
            call_type: "incoming".into(),
            call_duration: "3 min".into(),
            show_sender: true,
            show_date_separator: i % 7 == 0,
            date_separator_text: format!("Date {i}").into(),
            ..Default::default()
        })
        .collect();
    w.set_messages(ModelRc::new(VecModel::from(rows)));
    pump(&w, 412.0);
    // Alternate distant forward/backward seeks at both phone widths. The
    // mixed delegates force Slint to rebase its live-row height estimate.
    for width in [360.0, 412.0] {
        for index in [62, 14, 102, 30] {
            w.set_scroll_to_message_index(index);
            // Check again after settling: an estimate or a stale delegate
            // coordinate must not appear to land and then move the target away.
            for _ in 0..2 {
                pump(&w, width);
                let bubble = visible(&w, &format!("Fixture says: target-{index}"));
                let header = testing::ElementHandle::find_by_element_type_name(&w, "ChatHeader")
                    .find(|e| e.size().height > 0.0)
                    .unwrap();
                let expected = header.absolute_position().y + header.size().height;
                let landed = bubble.absolute_position().y;
                assert!(
                    landed >= expected - 1.0 && landed <= expected + 24.0,
                    "Seek {index} at {width}px landed at {landed}, header bottom {expected}"
                );
            }
        }
    }
}

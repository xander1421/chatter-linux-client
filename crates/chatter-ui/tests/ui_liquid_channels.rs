//! Focused software-renderer frames for channel and group navigation.
use i_slint_backend_testing::{self as testing, TestingBackend, TestingBackendOptions};
use slint::ComponentHandle;

slint::slint! {
    import { ChannelSettingsModal } from "../../ui/modals/channel_settings.slint";
    import { CreateChannelModal } from "../../ui/modals/create_channel.slint";
    import { JarvisMemoryModal } from "../../ui/modals/jarvis_memory.slint";
    import { Appearance } from "../../ui/theme/appearance.slint";
    import { Colors } from "../../ui/theme/colors.slint";

    export component ChannelsHarness inherits Window {
        in-out property <int> viewport-width: 412;
        in-out property <int> viewport-height: 840;
        width: root.viewport-width * 1px;
        height: root.viewport-height * 1px;
        background: Colors.background;
        in-out property <bool> settings-open: true;
        in-out property <bool> private-group: false;
        in-out property <string> section: "overview";
        in-out property <int> create-tab: 0;
        in-out property <bool> memory-open: false;
        callback theme(string);
        theme(value) => { Appearance.theme = value; }

        ChannelSettingsModal {
            width: parent.width;
            height: parent.height;
            show-modal: root.settings-open && !root.memory-open;
            channel-id: "sample-channel";
            channel-name: root.private-group ? "Quiet Circle" : "Design Circle";
            channel-description: "A calm place for notes, ideas, and conversations.";
            channel-name-draft: root.private-group ? "Quiet Circle" : "Design Circle";
            channel-description-draft: "A calm place for notes, ideas, and conversations.";
            channel-type: root.private-group ? "private-group" : "public";
            member-count: 3;
            can-edit: true;
            can-manage-members: true;
            is-owner: true;
            active-section <=> root.section;
            members: [
                { user-id: "one", display-name: "Morgan Hale", username: "morgan", role: "owner", joined-at: "Today", avatar-source: @image-url(""), muted: false, can-ban: false, can-mute: false, can-make-owner: false, unverified: false, can-block: false, blocked: false },
                { user-id: "two", display-name: "Alex Rivers", username: "alex", role: "member", joined-at: "Today", avatar-source: @image-url(""), muted: false, can-ban: true, can-mute: true, can-make-owner: true, unverified: false, can-block: false, blocked: false },
            ];
        }
        CreateChannelModal {
            width: parent.width;
            height: parent.height;
            show-modal: !root.settings-open && !root.memory-open;
            active-tab <=> root.create-tab;
        }
        JarvisMemoryModal {
            x: (parent.width - self.width) / 2;
            y: (parent.height - self.height) / 2;
            width: min(860px, parent.width - 32px);
            height: min(560px, parent.height - 32px);
            show: root.memory-open;
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

fn frame(window: &ChannelsHarness, name: &str) {
    let Some(dir) = std::env::var_os("U12_CAPTURE_DIR") else {
        return;
    };
    let pixels = window.window().take_snapshot().unwrap();
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
fn settings_and_create_tabs_render_across_theme_and_width() {
    slint::platform::set_platform(Box::new(TestingBackend::new(TestingBackendOptions {
        mock_time: true,
        threading: false,
        renderer_name: Some("software".into()),
    })))
    .unwrap();
    let window = ChannelsHarness::new().unwrap();
    window.show().unwrap();

    for (width, label) in [(412, "phone"), (1080, "desktop")] {
        window.set_viewport_width(width);
        window
            .window()
            .set_size(slint::LogicalSize::new(width as f32, 840.0));
        for theme in ["light", "dark"] {
            window.invoke_theme(theme.into());
            window.set_settings_open(true);
            window.set_private_group(false);
            window.set_section("overview".into());
            advance(500);
            frame(&window, &format!("{label}-{theme}-01-info.png"));
            window.set_section("members".into());
            advance(80);
            frame(&window, &format!("{label}-{theme}-02-tabs-moving.png"));
            advance(460);
            frame(&window, &format!("{label}-{theme}-03-members.png"));
            window.set_private_group(true);
            window.set_section("overview".into());
            advance(500);
            frame(&window, &format!("{label}-{theme}-04-private-group.png"));
            window.set_settings_open(false);
            window.set_create_tab(0);
            advance(500);
            frame(&window, &format!("{label}-{theme}-05-create.png"));
            window.set_create_tab(1);
            advance(80);
            frame(&window, &format!("{label}-{theme}-06-join-moving.png"));
            advance(460);
            frame(&window, &format!("{label}-{theme}-07-join.png"));
            window.set_memory_open(true);
            advance(500);
            frame(&window, &format!("{label}-{theme}-08-memory.png"));
            window.set_memory_open(false);
        }
    }
}

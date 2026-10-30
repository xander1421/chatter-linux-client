//! Focused software-renderer frames for media arrival and picker motion.
use i_slint_backend_testing::{self as testing, TestingBackend, TestingBackendOptions};
use slint::{ComponentHandle, LogicalSize};

slint::slint! {
    import { ImageViewer } from "../../ui/components/image_viewer.slint";
    import { EmojiPicker, EmojiData } from "../../ui/components/emoji_picker.slint";
    import { AudioPlayer } from "../../ui/chat/audio_player.slint";
    import { Appearance } from "../../ui/theme/appearance.slint";
    import { Colors } from "../../ui/theme/colors.slint";

    export component MediaHarness inherits Window {
        in-out property <int> viewport-width: 412;
        in-out property <int> viewport-height: 760;
        width: root.viewport-width * 1px;
        height: root.viewport-height * 1px;
        background: Colors.background;
        in-out property <bool> viewer-open: false;
        in-out property <bool> picker-open: false;
        in-out property <image> picture;
        callback theme(string);
        theme(value) => { Appearance.theme = value; }

        EmojiPicker {
            x: (parent.width - self.width) / 2;
            y: 80px;
            width: min(320px, parent.width - 24px);
            max-picker-width: self.width;
            height: 340px;
            show: root.picker-open;
            touch: parent.width < 600px;
            emoji-list: [
                { emoji: "😀", name: "grin", category: 0 },
                { emoji: "🥹", name: "joy", category: 0 },
                { emoji: "💜", name: "heart", category: 0 },
                { emoji: "🌊", name: "wave", category: 0 },
                { emoji: "✨", name: "sparkles", category: 0 },
                { emoji: "🎵", name: "music", category: 0 }
            ];
        }
        AudioPlayer {
            x: 24px;
            y: parent.height - 96px;
            width: 260px;
            height: 72px;
            attachment-id: "sample";
            current-playing-attachment-id: "sample";
            is-media-playing: true;
            audio-progress: 0.44;
            filename: "voice note";
            duration-display: "0:12";
            waveform: [12, 48, 88, 34, 72, 100, 36, 20, 55, 80, 28, 44];
        }
        ImageViewer {
            width: 100%;
            height: 100%;
            show: root.viewer-open;
            touch: parent.width < 600px;
            source: root.picture;
            filename: "ocean.png";
            position-label: "1 / 1";
            thumbnail-x: 48px;
            thumbnail-y: 120px;
            thumbnail-width: 100px;
            thumbnail-height: 70px;
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

fn capture(window: &MediaHarness, name: &str) -> Vec<u8> {
    let pixels = window.window().take_snapshot().expect("software frame");
    if let Some(dir) = std::env::var_os("U8_CAPTURE_DIR") {
        image::save_buffer(
            std::path::Path::new(&dir).join(name),
            pixels.as_bytes(),
            pixels.width(),
            pixels.height(),
            image::ColorType::Rgba8,
        )
        .unwrap();
    }
    pixels.as_bytes().to_vec()
}

#[test]
fn media_arrival_and_picker_have_intermediate_frames() {
    slint::platform::set_platform(Box::new(TestingBackend::new(TestingBackendOptions {
        mock_time: true,
        threading: false,
        renderer_name: Some("software".into()),
    })))
    .unwrap();
    let fixture = tempfile::tempdir().unwrap();
    let path = fixture.path().join("ocean.png");
    image::RgbaImage::from_pixel(320, 180, image::Rgba([24, 142, 178, 255]))
        .save(&path)
        .unwrap();
    let window = MediaHarness::new().unwrap();
    window.set_picture(slint::Image::load_from_path(&path).unwrap());
    window.show().unwrap();

    for (width, height, layout) in [(412., 760., "phone"), (960., 700., "desktop")] {
        window.set_viewport_width(width as i32);
        window.set_viewport_height(height as i32);
        window.window().set_size(LogicalSize::new(width, height));
        for theme in ["light", "dark"] {
            window.invoke_theme(theme.into());
            window.set_viewer_open(false);
            window.set_picker_open(false);
            advance(600);
            let _ = capture(&window, &format!("{layout}-{theme}-00-before.png"));
            window.set_picker_open(true);
            let start = capture(&window, &format!("{layout}-{theme}-01-picker-start.png"));
            advance(400);
            let settled = capture(&window, &format!("{layout}-{theme}-02-picker-settled.png"));
            assert!(start != settled, "picker should travel into place");
            window.set_picker_open(false);
            advance(400);
            window.set_viewer_open(true);
            let early = capture(&window, &format!("{layout}-{theme}-03-viewer-start.png"));
            advance(480);
            let finished = capture(&window, &format!("{layout}-{theme}-04-viewer-settled.png"));
            assert!(early != finished, "image should grow from thumbnail bounds");
        }
    }
}

//! Pixel regression for the full image viewer, using Slint's headless software renderer.

use i_slint_backend_testing::{TestingBackend, TestingBackendOptions};
use slint::{ComponentHandle, LogicalPosition, LogicalSize, platform::WindowEvent};

slint::slint! {
    import { ImageViewer } from "../../ui/components/image_viewer.slint";
    export component ViewerHarness inherits Window {
        width: 400px;
        height: 800px;
        in-out property <image> picture;
        viewer := ImageViewer {
            width: 100%;
            height: 100%;
            show: true;
            touch: true;
            source: root.picture;
            filename: "regression.png";
            position-label: "1 / 1";
        }
    }
}

fn save_snapshot(w: &ViewerHarness, name: &str) -> image::RgbaImage {
    let pixels = w
        .window()
        .take_snapshot()
        .expect("software renderer snapshot");
    let bitmap =
        image::RgbaImage::from_raw(pixels.width(), pixels.height(), pixels.as_bytes().to_vec())
            .expect("RGBA buffer dimensions");
    if let Ok(directory) = std::env::var("P25_CAPTURE_DIR") {
        bitmap.save(format!("{directory}/p25-{name}.png")).unwrap();
    }
    bitmap
}

fn red_bounds(bitmap: &image::RgbaImage) -> Option<(u32, u32, u32, u32)> {
    let mut bounds: Option<(u32, u32, u32, u32)> = None;
    for (x, y, pixel) in bitmap.enumerate_pixels() {
        if pixel[0] > 200 && pixel[1] < 80 && pixel[2] < 80 {
            bounds = Some(match bounds {
                None => (x, y, x, y),
                Some((left, top, right, bottom)) => {
                    (left.min(x), top.min(y), right.max(x), bottom.max(y))
                }
            });
        }
    }
    bounds
}

#[test]
fn image_is_visible_at_fit_and_after_double_tap() {
    slint::platform::set_platform(Box::new(TestingBackend::new(TestingBackendOptions {
        mock_time: true,
        threading: false,
        renderer_name: Some("software".into()),
    })))
    .unwrap();

    let fixture = tempfile::tempdir().unwrap();
    let path = fixture.path().join("known-320x160.png");
    image::RgbaImage::from_pixel(320, 160, image::Rgba([255, 0, 0, 255]))
        .save(&path)
        .unwrap();
    let w = ViewerHarness::new().unwrap();
    w.window().set_size(LogicalSize::new(400., 800.));
    w.set_picture(slint::Image::load_from_path(&path).unwrap());
    let fit = save_snapshot(&w, "fit");
    let bounds = red_bounds(&fit).expect("full-size image absent at fit");
    assert_eq!(bounds, (8, 304, 391, 495), "fitted image rectangle");

    let point = LogicalPosition::new(200., 400.);
    for _ in 0..2 {
        w.window()
            .dispatch_event(WindowEvent::PointerMoved { position: point });
        w.window().dispatch_event(WindowEvent::PointerPressed {
            position: point,
            button: slint::platform::PointerEventButton::Left,
        });
        w.window().dispatch_event(WindowEvent::PointerReleased {
            position: point,
            button: slint::platform::PointerEventButton::Left,
        });
    }
    let zoomed = save_snapshot(&w, "zoom");
    let bounds = red_bounds(&zoomed).expect("full-size image absent after double tap");
    assert_eq!(
        bounds,
        (8, 160, 391, 639),
        "2.5x image rectangle after double tap"
    );
}

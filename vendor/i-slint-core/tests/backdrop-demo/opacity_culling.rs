//! Generated opacity groups must skip invisible paint, then redraw on reveal.
use slint::ComponentHandle;
use slint::platform::{
    Platform, WindowAdapter,
    software_renderer::{MinimalSoftwareWindow, RepaintBufferType},
};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};
include!("opacity_scene.rs");

struct Headless(Rc<MinimalSoftwareWindow>);
impl Platform for Headless {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        Ok(self.0.clone())
    }
}
#[test]
fn transparent_group_skips_paint_and_reveals_updated_children() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
    slint::platform::set_platform(Box::new(Headless(adapter.clone()))).unwrap();
    let probe = OpacityProbe::new().unwrap();
    let calls = Rc::new(Cell::new(0));
    let observed = calls.clone();
    probe.on_paint_color(move |color| {
        observed.set(observed.get() + 1);
        color
    });
    probe.show().unwrap();
    adapter.set_size(slint::PhysicalSize::new(80, 60));
    let buffer = RefCell::new(vec![slint::Rgb8Pixel::default(); 80 * 60]);
    let render = || {
        slint::platform::update_timers_and_animations();
        let mut pixels = buffer.borrow_mut();
        let mut painted = false;
        adapter.draw_if_needed(|r| {
            painted = true;
            r.render(&mut pixels, 80);
        });
        assert!(painted, "opacity/content transitions must request redraw");
        pixels.clone()
    };
    let black = slint::Rgb8Pixel::new(0, 0, 0);
    let red = slint::Rgb8Pixel::new(255, 0, 0);
    let blue = slint::Rgb8Pixel::new(0, 0, 255);
    assert_eq!(render()[10 * 80 + 10], black);
    assert_eq!(
        calls.get(),
        0,
        "hidden groups must not evaluate child paint"
    );
    probe.set_alpha(1.);
    assert_eq!(render()[10 * 80 + 10], red);
    assert!(calls.get() >= 2);
    probe.set_alpha(0.);
    let hidden = render();
    assert_eq!(hidden[10 * 80 + 10], black);
    assert_eq!(hidden[10 * 80 + 70], black);
    let before = calls.get();
    probe.set_child_color(slint::Color::from_rgb_u8(0, 0, 255));
    probe.window().request_redraw();
    assert_eq!(render()[10 * 80 + 10], black);
    assert_eq!(calls.get(), before);
    probe.set_alpha(1.);
    let pixels = render();
    assert_eq!(pixels[10 * 80 + 10], blue);
    assert_eq!(pixels[10 * 80 + 30], blue);
    assert_eq!(pixels[10 * 80 + 70], blue);
    assert!(calls.get() > before);
    probe.set_alpha(0.5);
    let pixels = render();
    // The software backend applies opacity per primitive; preserve that path.
    assert!((189..=193).contains(&pixels[10 * 80 + 30].b));
    assert!((126..=129).contains(&pixels[10 * 80 + 10].b));
    probe.hide().unwrap();
}

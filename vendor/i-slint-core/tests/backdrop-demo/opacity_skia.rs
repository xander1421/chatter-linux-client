//! Actual Skia item rendering with a reused raster target and partial damage.
use i_slint_core::{
    graphics::RequestedGraphicsAPI, partial_renderer::DirtyRegion, renderer::DrawOutcome,
};
use i_slint_renderer_skia::{SkiaRenderer, SkiaSharedContext, Surface};
use slint::platform::{Platform, PlatformError, Renderer, WindowAdapter, WindowEvent};
use slint::{ComponentHandle, PhysicalSize, Window, WindowSize};
use std::{
    cell::{Cell, RefCell},
    rc::{Rc, Weak},
    sync::Arc,
};
include!("opacity_scene.rs");
struct Raster(Rc<RefCell<skia_safe::Surface>>);
impl Surface for Raster {
    fn new(
        _: &SkiaSharedContext,
        _: Arc<dyn raw_window_handle::HasWindowHandle + Send + Sync>,
        _: Arc<dyn raw_window_handle::HasDisplayHandle + Send + Sync>,
        _: PhysicalSize,
        _: Option<RequestedGraphicsAPI>,
    ) -> Result<Self, PlatformError> {
        unreachable!()
    }
    fn name(&self) -> &'static str {
        "opacity-test-raster"
    }
    fn render(
        &self,
        _: &Window,
        _: PhysicalSize,
        callback: &dyn Fn(
            &skia_safe::Canvas,
            Option<&mut skia_safe::gpu::DirectContext>,
            u8,
        ) -> Option<DirtyRegion>,
        _: &RefCell<Option<Box<dyn FnMut()>>>,
    ) -> Result<DrawOutcome, PlatformError> {
        callback(self.0.borrow_mut().canvas(), None, 1);
        Ok(DrawOutcome::Success)
    }
    fn resize_event(&self, _: PhysicalSize) -> Result<(), PlatformError> {
        Ok(())
    }
    fn bits_per_pixel(&self) -> Result<u8, PlatformError> {
        Ok(32)
    }
    fn use_partial_rendering(&self) -> bool {
        true
    }
}
struct Adapter {
    window: Window,
    renderer: SkiaRenderer,
    size: Cell<PhysicalSize>,
    dirty: Cell<bool>,
    pixels: Rc<RefCell<skia_safe::Surface>>,
}
impl Adapter {
    fn new() -> Rc<Self> {
        Rc::new_cyclic(|weak: &Weak<Self>| {
            let pixels = Rc::new(RefCell::new(
                skia_safe::surfaces::raster_n32_premul((80, 60)).unwrap(),
            ));
            Self {
                window: Window::new(weak.clone()),
                renderer: SkiaRenderer::new_with_surface(
                    &SkiaSharedContext::default(),
                    Box::new(Raster(pixels.clone())),
                ),
                size: Cell::new(PhysicalSize::new(80, 60)),
                dirty: Cell::new(true),
                pixels,
            }
        })
    }
    fn render(&self) -> Vec<u8> {
        slint::platform::update_timers_and_animations();
        assert!(self.dirty.replace(false), "transition must request redraw");
        assert_eq!(self.renderer.render().unwrap(), DrawOutcome::Success);
        let mut out = vec![0; 80 * 60 * 4];
        assert!(self.pixels.borrow_mut().read_pixels(
            &skia_safe::ImageInfo::new_n32_premul((80, 60), None),
            &mut out,
            80 * 4,
            (0, 0)
        ));
        out
    }
}
impl WindowAdapter for Adapter {
    fn window(&self) -> &Window {
        &self.window
    }
    fn renderer(&self) -> &dyn Renderer {
        &self.renderer
    }
    fn size(&self) -> PhysicalSize {
        self.size.get()
    }
    fn set_size(&self, size: WindowSize) {
        self.size.set(size.to_physical(self.window.scale_factor()));
        self.window.dispatch_event(WindowEvent::Resized {
            size: size.to_logical(self.window.scale_factor()),
        });
    }
    fn request_redraw(&self) {
        self.dirty.set(true);
    }
}
struct Headless(Rc<Adapter>);
impl Platform for Headless {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        Ok(self.0.clone())
    }
}
#[test]
fn skia_hidden_group_culls_paint_and_preserves_reveal_damage_and_overflow() {
    let adapter = Adapter::new();
    slint::platform::set_platform(Box::new(Headless(adapter.clone()))).unwrap();
    let probe = OpacityProbe::new().unwrap();
    let calls = Rc::new(Cell::new(0));
    let observed = calls.clone();
    probe.on_paint_color(move |c| {
        observed.set(observed.get() + 1);
        c
    });
    probe.show().unwrap();
    probe.window().set_size(PhysicalSize::new(80, 60));
    let pixel = |out: &[u8], x: usize| -> [u8; 4] {
        out[(10 * 80 + x) * 4..(10 * 80 + x) * 4 + 4]
            .try_into()
            .unwrap()
    };
    let black = [0, 0, 0, 255];
    let red = [0, 0, 255, 255];
    let blue = [255, 0, 0, 255];
    let initial = adapter.render();
    assert_eq!(pixel(&initial, 10), black);
    assert_eq!(
        calls.get(),
        0,
        "Skia must not evaluate invisible child paint"
    );
    probe.set_alpha(1.);
    let visible = adapter.render();
    assert_eq!(pixel(&visible, 10), red);
    assert_eq!(pixel(&visible, 70), red);
    probe.set_alpha(0.);
    let hidden = adapter.render();
    assert_eq!(pixel(&hidden, 10), black);
    assert_eq!(pixel(&hidden, 70), black);
    let before = calls.get();
    probe.set_child_color(slint::Color::from_rgb_u8(0, 0, 255));
    probe.window().request_redraw();
    let hidden = adapter.render();
    assert_eq!(pixel(&hidden, 70), black);
    assert_eq!(calls.get(), before);
    probe.set_alpha(1.);
    let visible = adapter.render();
    assert_eq!(pixel(&visible, 10), blue);
    assert_eq!(pixel(&visible, 70), blue);
    assert!(calls.get() > before);
    probe.set_alpha(0.5);
    let fractional = adapter.render();
    assert_eq!(
        pixel(&fractional, 10),
        pixel(&fractional, 30),
        "Skia must preserve grouped alpha"
    );
    assert!((126..=129).contains(&pixel(&fractional, 10)[0]));
    probe.hide().unwrap();
}

// Exercise existing optics regressions with the same Skia/core dependencies.
#[path = "../../../i-slint-renderer-skia/backdrop.rs"]
mod backdrop;

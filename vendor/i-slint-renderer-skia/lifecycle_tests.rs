// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

//! Exercise the real renderer lifecycle without an OS window or graphics driver.
use super::*;
use raw_window_handle::{
    DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, WindowHandle,
};

#[derive(Default)]
struct Probe {
    events: RefCell<Vec<(&'static str, u64)>>,
    window_valid: Cell<bool>,
    fail_creation: Cell<bool>,
    fail_activation: Cell<bool>,
}

thread_local! {
    static PROBE: Rc<Probe> = Rc::default();
}

struct Handle(u64);
impl HasWindowHandle for Handle {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        // Only the fake factory reads this identifier. No native API receives it.
        Ok(unsafe {
            WindowHandle::borrow_raw(raw_window_handle::RawWindowHandle::Xlib(
                raw_window_handle::XlibWindowHandle::new(self.0 as _),
            ))
        })
    }
}
impl HasDisplayHandle for Handle {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        Ok(DisplayHandle::android())
    }
}

struct FakeSurface {
    probe: Rc<Probe>,
    generation: u64,
}
impl Drop for FakeSurface {
    fn drop(&mut self) {
        assert!(
            self.probe.window_valid.get(),
            "surface outlived window validity"
        );
        self.probe
            .events
            .borrow_mut()
            .push(("drop", self.generation));
    }
}
impl Surface for FakeSurface {
    fn new(
        _: &SkiaSharedContext,
        window: Arc<dyn HasWindowHandle + Send + Sync>,
        _: Arc<dyn HasDisplayHandle + Send + Sync>,
        size: PhysicalWindowSize,
        _: Option<RequestedGraphicsAPI>,
    ) -> Result<Self, PlatformError> {
        assert_eq!(size, PhysicalWindowSize::new(1440, 2960));
        let raw_window_handle::RawWindowHandle::Xlib(handle) = window
            .window_handle()
            .map_err(|error| error.to_string())?
            .as_raw()
        else {
            panic!("unexpected test window handle")
        };
        PROBE.with(|probe| {
            assert!(probe.window_valid.get());
            probe
                .events
                .borrow_mut()
                .push(("create", handle.window as u64));
            if probe.fail_creation.get() {
                return Err("injected factory failure".into());
            }
            Ok(Self {
                probe: probe.clone(),
                generation: handle.window as u64,
            })
        })
    }
    fn name(&self) -> &'static str {
        "lifecycle-test"
    }
    fn bits_per_pixel(&self) -> Result<u8, PlatformError> {
        Ok(32)
    }
    fn with_graphics_api(&self, callback: &mut dyn FnMut(GraphicsAPI<'_>)) {
        callback(GraphicsAPI::NativeOpenGL {
            get_proc_address: &|_| std::ptr::null(),
        });
    }
    fn with_active_surface(&self, callback: &mut dyn FnMut()) -> Result<(), PlatformError> {
        assert!(self.probe.window_valid.get());
        self.probe
            .events
            .borrow_mut()
            .push(("activate", self.generation));
        if self.probe.fail_activation.get() {
            return Err("injected lost context".into());
        }
        callback();
        Ok(())
    }
    fn render(
        &self,
        _: &Window,
        _: PhysicalWindowSize,
        _: &dyn Fn(
            &skia_safe::Canvas,
            Option<&mut skia_safe::gpu::DirectContext>,
            u8,
        ) -> Option<DirtyRegion>,
        _: &RefCell<Option<Box<dyn FnMut()>>>,
    ) -> Result<DrawOutcome, PlatformError> {
        unreachable!("this test drives setup directly; it does not claim presentation")
    }
    fn resize_event(&self, _: PhysicalWindowSize) -> Result<(), PlatformError> {
        Ok(())
    }
}

fn associate(renderer: &SkiaRenderer, generation: u64) -> Result<(), PlatformError> {
    renderer.set_window_handle(
        Arc::new(Handle(generation)),
        Arc::new(Handle(0)),
        PhysicalWindowSize::new(1440, 2960),
        None,
        false,
    )
}

fn setup(renderer: &SkiaRenderer) {
    let surface = renderer.surface.borrow();
    renderer
        .invoke_rendering_notifier_setup(surface.as_ref().unwrap().as_ref())
        .unwrap();
}

fn renderer() -> (SkiaRenderer, Rc<Probe>) {
    let probe = PROBE.with(Clone::clone);
    probe.events.borrow_mut().clear();
    probe.window_valid.set(true);
    probe.fail_creation.set(false);
    probe.fail_activation.set(false);
    let renderer = SkiaRenderer::new_with_surface_factory(
        &SkiaSharedContext::default(),
        surface_factory::<FakeSurface>,
        None,
    );
    let observer = probe.clone();
    *renderer.rendering_notifier.borrow_mut() =
        Some(Box::new(move |state, _: &GraphicsAPI<'_>| {
            assert!(observer.window_valid.get());
            let event = match state {
                RenderingState::RenderingSetup => "setup",
                RenderingState::RenderingTeardown => "teardown",
                _ => panic!("unexpected notification"),
            };
            observer.events.borrow_mut().push((event, 0));
        }));
    (renderer, probe)
}

#[test]
fn suspend_reassociate_releases_before_window_expires_and_preserves_notifier() {
    let (renderer, probe) = renderer();
    associate(&renderer, 1).unwrap();
    setup(&renderer);
    setup(&renderer); // Setup is delivered once per installed surface.
    renderer.suspend().unwrap();
    assert!(renderer.surface.borrow().is_none());
    assert_eq!(
        &*probe.events.borrow(),
        &[
            ("create", 1),
            ("setup", 0),
            ("activate", 1),
            ("teardown", 0),
            ("drop", 1),
        ]
    );
    probe.window_valid.set(false); // Simulate returning from TerminateWindow.
    renderer.suspend().unwrap(); // No old-window access on an empty renderer.
    assert!(renderer.rendering_notifier.borrow().is_some());
    probe.window_valid.set(true);
    associate(&renderer, 2).unwrap();
    setup(&renderer);
    drop(renderer);
    assert_eq!(
        &probe.events.borrow()[5..],
        &[
            ("create", 2),
            ("setup", 0),
            ("activate", 2),
            ("teardown", 0),
            ("drop", 2),
        ]
    );
}

#[test]
fn failed_reassociation_drops_old_surface_and_can_retry() {
    let (renderer, probe) = renderer();
    associate(&renderer, 1).unwrap();
    setup(&renderer);
    probe.fail_creation.set(true);
    assert!(associate(&renderer, 2).is_err());
    assert!(renderer.surface.borrow().is_none());
    assert_eq!(
        &*probe.events.borrow(),
        &[
            ("create", 1),
            ("setup", 0),
            ("activate", 1),
            ("teardown", 0),
            ("drop", 1),
            ("create", 2),
        ]
    );
    probe.fail_creation.set(false);
    associate(&renderer, 3).unwrap();
    setup(&renderer);
    drop(renderer);
    assert_eq!(
        &probe.events.borrow()[6..],
        &[
            ("create", 3),
            ("setup", 0),
            ("activate", 3),
            ("teardown", 0),
            ("drop", 3),
        ]
    );
}

#[test]
fn lost_activation_still_drops_surface_and_unrendered_surface_needs_no_teardown() {
    let (renderer, probe) = renderer();
    associate(&renderer, 1).unwrap();
    setup(&renderer);
    probe.fail_activation.set(true);
    renderer.suspend().unwrap();
    assert!(renderer.surface.borrow().is_none());
    assert_eq!(
        &*probe.events.borrow(),
        &[("create", 1), ("setup", 0), ("activate", 1), ("drop", 1),]
    );
    probe.fail_activation.set(false);
    associate(&renderer, 2).unwrap();
    renderer.suspend().unwrap();
    assert_eq!(&probe.events.borrow()[4..], &[("create", 2), ("drop", 2)]);
    probe.window_valid.set(false);
    drop(renderer); // Already suspended: no further callbacks or surface access.
}

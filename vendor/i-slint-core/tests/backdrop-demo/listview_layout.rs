//! Exercise the actual generated virtualized ListView against the patched core.
use slint::platform::{
    Platform, WindowAdapter,
    software_renderer::{MinimalSoftwareWindow, RepaintBufferType},
};
use slint::{ComponentHandle, Model};
use std::rc::Rc;

slint::slint! {
    component ListView inherits Flickable {
        out property <length> visible-height: self.height;
        out property <length> visible-width: self.width;
        @children
    }
    export component LayoutProbe inherits Window {
        width: 200px; height: 100px; background: black;
        in property <[int]> heights;
        in-out property <length> row-width: 200px;
        in-out property <length> scroll-y <=> list.viewport-y;
        out property <length> extent: list.viewport-height;
        list := ListView {
            x: 0px; y: 0px;
            width: root.row-width; height: 100%;
            for h[i] in root.heights: Rectangle {
                width: list.width; height: h * 1px;
                background: mod(i, 2) == 0 ? red : blue;
            }
        }
    }
    export component SnapshotProbe inherits Window {
        width: 200px; height: 100px;
        in property <[int]> values;
        in property <bool> open;
        callback child-init(int);
        for value in root.values: Rectangle {
            if root.open: Rectangle {
                init => { root.child-init(value); }
            }
        }
    }
}

struct Headless(Rc<MinimalSoftwareWindow>);
impl Platform for Headless {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        Ok(self.0.clone())
    }
}

#[test]
fn generated_list_updates_after_clean_pass_scroll_resize_edit_and_model_changes() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(Headless(adapter.clone()))).unwrap();
    let window = LayoutProbe::new().unwrap();
    let rows = Rc::new(slint::VecModel::from(vec![20; 20]));
    window.set_heights(rows.clone().into());
    window.show().unwrap();
    adapter.set_size(slint::PhysicalSize::new(200, 100));
    let pass = std::cell::Cell::new(0);
    let render = || {
        pass.set(pass.get() + 1);
        slint::platform::update_timers_and_animations();
        let mut pixels = vec![slint::Rgb8Pixel::default(); 200 * 100];
        let mut painted = false;
        adapter.draw_if_needed(|renderer| {
            painted = true;
            renderer.render(&mut pixels, 200);
        });
        assert!(painted, "model/geometry changes must request redraw themselves, pass {}", pass.get());
        pixels
    };
    let red = slint::Rgb8Pixel::new(255, 0, 0);
    let blue = slint::Rgb8Pixel::new(0, 0, 255);
    let black = slint::Rgb8Pixel::new(0, 0, 0);
    assert_eq!(render()[10 * 200 + 10], red);
    // Exercise a clean update once, then rely on automatic invalidation for
    // each subsequent mutation instead of masking missing redraw dependencies.
    window.window().request_redraw();
    assert_eq!(render()[30 * 200 + 10], blue);
    window.set_scroll_y(-20.);
    assert_eq!(render()[10 * 200 + 10], blue);
    window.set_row_width(100.);
    let pixels = render();
    assert_eq!(pixels[10 * 200 + 10], blue);
    assert_eq!(pixels[10 * 200 + 150], black);
    rows.set_row_data(1, 60);
    assert_eq!(render()[35 * 200 + 10], blue);
    let previous_extent = window.get_extent();
    rows.push(20); // Beyond the visible delegates; still changes scroll extent.
    // There is no scrollbar in this probe: an offscreen append need not paint
    // unchanged visible pixels. Its next layout must nevertheless update extent.
    window.window().request_redraw();
    render();
    assert!(window.get_extent() > previous_extent);
    rows.set_vec(Vec::new());
    assert_eq!(render()[10 * 200 + 10], black);
    rows.push(40);
    window.set_scroll_y(0.);
    assert_eq!(render()[10 * 200 + 10], red);
}

#[test]
fn generated_child_init_can_reset_model_during_complete_snapshot_traversal() {
    let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(Headless(adapter.clone()))).unwrap();
    for len in [1, 4, 5, 12] {
        let window = SnapshotProbe::new().unwrap();
        let rows = Rc::new(slint::VecModel::from((0..len).collect::<Vec<_>>()));
        window.set_values(rows.clone().into());
        let visited = Rc::new(std::cell::RefCell::new(Vec::new()));
        window.on_child_init({
            let rows = rows.clone();
            let visited = visited.clone();
            move |value| {
                visited.borrow_mut().push(value);
                rows.set_vec(Vec::new());
            }
        });
        window.show().unwrap();
        adapter.set_size(slint::PhysicalSize::new(200, 100));
        let draw = || {
            window.window().request_redraw();
            adapter.draw_if_needed(|renderer| {
                renderer.render(&mut vec![slint::Rgb8Pixel::default(); 200 * 100], 200);
            });
        };
        draw(); // Materialize outer delegates while their children are absent.
        assert!(visited.borrow().is_empty());
        window.set_open(true);
        draw();
        assert_eq!(*visited.borrow(), (0..len).collect::<Vec<_>>());
        assert_eq!(rows.row_count(), 0);
        draw();
        assert_eq!(
            visited.borrow().len(),
            len as usize,
            "init must run only once"
        );
        // Refill after the callback-driven reset to check the next fixed point.
        rows.set_vec(vec![42]);
        draw();
        assert_eq!(visited.borrow().last(), Some(&42));
        assert_eq!(rows.row_count(), 0);
        window.hide().unwrap();
    }
}

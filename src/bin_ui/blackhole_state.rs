//! Black hole animation state.
//!
//! Manages the animated black hole background for login/register pages.
//! Uses wgpu for GPU-accelerated rendering and slint::Timer for animation.

use std::cell::RefCell;
use std::time::Instant;

use slint::{Timer, TimerMode, Weak};

use chatter_linux::media::blackhole::{BlackHoleRenderer, RENDER_HEIGHT, RENDER_WIDTH};

use crate::MainWindow;

thread_local! {
    /// Renderer instance (must be on UI thread due to wgpu requirements).
    static RENDERER: RefCell<Option<BlackHoleRenderer>> = const { RefCell::new(None) };
    /// Animation timer.
    static TIMER: RefCell<Option<Timer>> = const { RefCell::new(None) };
    /// Animation start time for consistent timing.
    static START_TIME: RefCell<Option<Instant>> = const { RefCell::new(None) };
}

/// Start the black hole animation.
///
/// Initializes the wgpu renderer and starts a timer to update frames at 24fps.
/// The animation is displayed via the `auth-background-image` property.
pub fn start_animation(window: &Weak<MainWindow>) {
    // Don't restart if already running
    if is_running() {
        return;
    }

    // Initialize renderer if needed
    let renderer_ok = RENDERER.with(|cell| {
        if cell.borrow().is_some() {
            return true;
        }

        match BlackHoleRenderer::new() {
            Ok(renderer) => {
                *cell.borrow_mut() = Some(renderer);
                tracing::info!("Black hole renderer initialized");
                true
            }
            Err(e) => {
                tracing::warn!("Failed to initialize black hole renderer: {}", e);
                false
            }
        }
    });

    if !renderer_ok {
        return;
    }

    // Set start time
    START_TIME.with(|cell| {
        *cell.borrow_mut() = Some(Instant::now());
    });

    // Stop existing timer if any
    TIMER.with(|cell| {
        if let Some(timer) = cell.borrow().as_ref() {
            timer.stop();
        }
    });

    // Render first frame immediately
    render_frame(window);

    // Start animation timer at 24fps (~42ms per frame)
    let weak = window.clone();
    let timer = Timer::default();
    timer.start(TimerMode::Repeated, std::time::Duration::from_millis(42), move || {
        render_frame(&weak);
    });

    TIMER.with(|cell| {
        *cell.borrow_mut() = Some(timer);
    });

    tracing::debug!("Black hole animation started");
}

/// Stop the black hole animation.
///
/// Stops the animation timer but keeps the renderer for potential reuse.
pub fn stop_animation() {
    TIMER.with(|cell| {
        if let Some(timer) = cell.borrow().as_ref() {
            timer.stop();
        }
        *cell.borrow_mut() = None;
    });

    START_TIME.with(|cell| {
        *cell.borrow_mut() = None;
    });

    tracing::debug!("Black hole animation stopped");
}

/// Cleanup the black hole renderer entirely.
///
/// Call this when the renderer is no longer needed to free GPU resources.
pub fn cleanup() {
    stop_animation();

    RENDERER.with(|cell| {
        *cell.borrow_mut() = None;
    });

    tracing::debug!("Black hole renderer cleaned up");
}

/// Render a single frame and update the window.
fn render_frame(window: &Weak<MainWindow>) {
    let Some(w) = window.upgrade() else {
        return;
    };

    // Calculate time since animation start
    let time = START_TIME.with(|cell| {
        cell.borrow()
            .as_ref()
            .map(|start| start.elapsed().as_secs_f32())
            .unwrap_or(0.0)
    });

    // Render frame
    let pixels = RENDERER.with(|cell| {
        cell.borrow()
            .as_ref()
            .map(|renderer| renderer.render_frame(time))
    });

    let Some(pixels) = pixels else {
        return;
    };

    // Convert to Slint image
    let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
        &pixels,
        RENDER_WIDTH,
        RENDER_HEIGHT,
    );
    let image = slint::Image::from_rgba8(buffer);

    // Update window
    w.set_auth_background_image(image);
}

/// Check if animation is currently running.
pub fn is_running() -> bool {
    TIMER.with(|cell| cell.borrow().is_some())
}

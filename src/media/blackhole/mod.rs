//! Black hole visual effect renderer.
//!
//! Renders an Interstellar-style black hole animation using wgpu.
//! Used as the background for login/register pages.

mod renderer;

pub use renderer::{BlackHoleRenderer, RENDER_HEIGHT, RENDER_WIDTH};

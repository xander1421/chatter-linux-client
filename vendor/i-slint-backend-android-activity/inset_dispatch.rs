// Copyright © Chatter contributors
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0
//! Queue fence shared by JNI publication and host recovery regressions.
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Default)]
pub struct InsetDispatchFence(AtomicU64);

impl InsetDispatchFence {
    pub const fn new() -> Self {
        Self(AtomicU64::new(0))
    }
    pub fn receive(&self, sequence: u64) {
        self.0.fetch_max(sequence, Ordering::SeqCst);
    }

    pub fn apply(&self, sequence: u64, publish: impl FnOnce()) -> bool {
        if self.0.load(Ordering::SeqCst) != sequence {
            return false;
        }
        publish();
        true
    }
}

/// The canonical full window bounds are independent of an adjustResize surface.
/// Insets are physical; the returned IME rectangle is logical, as required by Slint.
pub fn keyboard_rect(
    origin: [f32; 2],
    size: [f32; 2],
    edges: [f32; 4],
    scale: f32,
) -> ([f32; 2], [f32; 2]) {
    let [x, y] = origin.map(|v| v / scale);
    let [width, height] = size.map(|v| v / scale);
    let [top, bottom, left, right] = edges.map(|v| v / scale);
    if bottom > 0.0 {
        ([x, y + height - bottom], [width, bottom])
    } else if top > 0.0 {
        ([x, y], [width, top])
    } else if left > 0.0 {
        ([x, y], [left, height])
    } else if right > 0.0 {
        ([x + width - right, y], [right, height])
    } else {
        ([0.0, 0.0], [0.0, 0.0])
    }
}

/// The same publication path is used by JNI and the phone recovery fixture.
/// Only API30+ WindowMetrics snapshots override native surface dimensions.
pub fn publish_window_insets(
    window: &i_slint_core::api::Window,
    authoritative_geometry: bool,
    origin: i_slint_core::api::PhysicalPosition,
    size: i_slint_core::api::PhysicalSize,
    safe_area: i_slint_core::lengths::PhysicalEdges,
    keyboard: i_slint_core::lengths::PhysicalEdges,
) {
    use i_slint_core::api::{LogicalPosition, LogicalSize};
    let scale = window.scale_factor();
    if authoritative_geometry {
        let _ = window.dispatch_event_with_result(i_slint_core::platform::WindowEvent::Resized {
            size: size.to_logical(scale),
        });
    }
    i_slint_core::window::WindowInner::from_pub(window)
        .set_window_item_safe_area(safe_area.to_logical(scale));
    let (position, extent) = keyboard_rect(
        [origin.x as f32, origin.y as f32],
        [size.width as f32, size.height as f32],
        [
            keyboard.top as f32,
            keyboard.bottom as f32,
            keyboard.left as f32,
            keyboard.right as f32,
        ],
        scale,
    );
    window.set_virtual_keyboard(
        LogicalPosition::new(position[0], position[1]),
        LogicalSize::new(extent[0], extent[1]),
        i_slint_core::InternalToken,
    );
}

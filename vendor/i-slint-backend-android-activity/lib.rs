// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

// cSpell: ignore androidwindowadapter javahelper
#![doc = include_str!("README.md")]
#![doc(html_logo_url = "https://slint.dev/logo/slint-logo-square-light.svg")]
#![cfg_attr(not(target_os = "android"), allow(rustdoc::broken_intra_doc_links))]
#![cfg(target_os = "android")]
#![cfg_attr(slint_nightly_test, feature(non_exhaustive_omitted_patterns_lint))]
#![cfg_attr(slint_nightly_test, warn(non_exhaustive_omitted_patterns))]

mod androidwindowadapter;
mod javahelper;
mod inset_dispatch;
mod text_input_offsets;

#[cfg(all(not(feature = "aa-06"), feature = "aa-05"))]
pub use android_activity_05 as android_activity;
#[cfg(feature = "aa-06")]
pub use android_activity_06 as android_activity;

pub use android_activity::AndroidApp;
use android_activity::PollEvent;
use androidwindowadapter::AndroidWindowAdapter;
pub use androidwindowadapter::{InboundTextInput, TextInputBridge, TextInputPush};
use core::ops::ControlFlow;
use core::time::Duration;
use i_slint_core::api::{EventLoopError, PlatformError};
use i_slint_core::platform::{Clipboard, WindowAdapter};
use i_slint_renderer_skia::SkiaRendererExt;
use std::cell::RefCell;
use std::rc::{Rc, Weak};
use std::sync::{Arc, Mutex};

/// Chatter patch: shortest time one iteration of the render loop may take
/// (see `run_event_loop`), used until the panel's refresh rate is known.
const FRAME_FLOOR: Duration = Duration::from_millis(4);

/// Chatter patch: how long the UI must be at rest before the refresh-rate
/// vote is cleared (see `AndroidPlatform::with_refresh_rate_vote`).
const MOTION_IDLE: Duration = Duration::from_millis(400);

thread_local! {
    static CURRENT_WINDOW: RefCell<Weak<AndroidWindowAdapter>> = RefCell::new(Default::default());
}

pub struct AndroidPlatform {
    app: AndroidApp,
    window: Rc<AndroidWindowAdapter>,
    event_listener: Option<Box<dyn Fn(&PollEvent<'_>)>>,
    /// Chatter patch: see `with_refresh_rate_vote`.
    refresh_rate_vote: Option<Box<dyn Fn(bool) -> Option<f32>>>,
    /// Whether the last vote that landed was "moving".
    voting: core::cell::Cell<bool>,
    /// Half a vsync at the panel's top rate, once known; see `run_event_loop`.
    frame_floor: core::cell::Cell<Duration>,
}

impl AndroidPlatform {
    /// Instantiate a new Android backend given the [`android_activity::AndroidApp`]
    ///
    /// Pass the returned value to [`slint::platform::set_platform()`](`i_slint_core::platform::set_platform()`)
    ///
    /// # Example
    /// ```
    /// #[cfg(target_os = "android")]
    /// #[unsafe(no_mangle)]
    /// fn android_main(app: i_slint_backend_android_activity::AndroidApp) {
    ///     slint::platform::set_platform(Box::new(
    ///         i_slint_backend_android_activity::AndroidPlatform::new(app),
    ///     ))
    ///     .unwrap();
    ///     // ... your slint application ...
    /// }
    /// ```
    pub fn new(app: AndroidApp) -> Self {
        let window = AndroidWindowAdapter::new(app.clone());
        CURRENT_WINDOW.set(Rc::downgrade(&window));
        Self {
            app,
            window,
            event_listener: None,
            refresh_rate_vote: None,
            voting: Default::default(),
            frame_floor: core::cell::Cell::new(FRAME_FLOOR),
        }
    }

    /// Chatter patch: vote for the panel's top refresh rate only while the UI
    /// moves.
    ///
    /// `vote(true)` is called when an animation (a fling, a transition) or a
    /// touch drag starts, and `vote(false)` once neither has happened for
    /// `MOTION_IDLE`; only on those transitions, never per frame. It returns
    /// the panel's top rate in Hz (0 if unknown), which sets the frame floor,
    /// or `None` if the vote did not land, in which case it is retried.
    pub fn with_refresh_rate_vote(mut self, vote: impl Fn(bool) -> Option<f32> + 'static) -> Self {
        self.refresh_rate_vote = Some(Box::new(vote));
        self
    }

    /// Chatter patch: route the IME's copy of the focused field through the
    /// app's producer gate (see [`TextInputBridge`]). With a bridge bound,
    /// every outbound text state and every accepted-send consumption go
    /// through it, and inbound states arrive through
    /// [`deliver_text_input_state`]; android-activity's own text events are
    /// then only a wake-up.
    pub fn with_text_input_bridge(self, bridge: impl TextInputBridge + 'static) -> Self {
        self.window.set_text_input_bridge(Rc::new(bridge));
        self
    }

    /// Chatter patch: edge-triggered refresh-rate vote; see
    /// `with_refresh_rate_vote`.
    fn update_refresh_rate_vote(&self, animating: bool) {
        let Some(vote) = &self.refresh_rate_vote else { return };
        let now = std::time::Instant::now();
        if animating {
            self.window.last_motion.set(Some(now));
        }
        let moving = self.window.last_motion.get().is_some_and(|t| now - t < MOTION_IDLE);
        if moving == self.voting.get() {
            return;
        }
        let Some(hz) = vote(moving) else { return };
        self.voting.set(moving);
        if moving {
            self.frame_floor.set(if hz > 0. {
                Duration::from_secs_f32(0.5 / hz)
            } else {
                FRAME_FLOOR
            });
        }
    }

    /// Instantiate a new Android backend given the [`android_activity::AndroidApp`]
    /// and a function to process the events.
    ///
    /// This is the same as [`AndroidPlatform::new()`], but it allow you to get notified
    /// of events.
    ///
    /// Pass the returned value to [`slint::platform::set_platform()`](`i_slint_core::platform::set_platform()`)
    ///
    /// # Example
    /// ```
    /// #[cfg(target_os = "android")]
    /// #[unsafe(no_mangle)]
    /// fn android_main(app: i_slint_backend_android_activity::AndroidApp) {
    ///     slint::platform::set_platform(Box::new(
    ///         i_slint_backend_android_activity::AndroidPlatform::new_with_event_listener(
    ///             app,
    ///             |event| { eprintln!("got event {event:?}") }
    ///         ),
    ///     ))
    ///     .unwrap();
    ///     // ... your slint application ...
    /// }
    /// ```
    pub fn new_with_event_listener(
        app: AndroidApp,
        listener: impl Fn(&PollEvent<'_>) + 'static,
    ) -> Self {
        let mut this = Self::new(app);
        this.event_listener = Some(Box::new(listener));
        this
    }
}

impl i_slint_core::platform::Platform for AndroidPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        Ok(self.window.clone())
    }
    fn run_event_loop(&self) -> Result<(), PlatformError> {
        loop {
            let mut timeout = i_slint_core::platform::duration_until_next_timer_update();
            if self.window.window.has_active_animations() {
                // Chatter patch: upstream hardcoded a 10 ms frame here, capping
                // a fling at ~100 fps — below a 120 Hz panel, and out of phase
                // with any vsync. With a window, don't wait at all: the redraw
                // forced below blocks in the vsync'd eglSwapBuffers (swap
                // interval 1), which paces the loop at the display's rate.
                // Without one there is no swap to block on, so fall back to a
                // 60 Hz tick rather than spin.
                let frame_duration = if self.app.native_window().is_some() {
                    Duration::ZERO
                } else {
                    Duration::from_millis(16)
                };
                timeout = Some(match timeout {
                    Some(x) => x.min(frame_duration),
                    None => frame_duration,
                })
            }
            // Chatter patch: wake when the UI has been at rest for MOTION_IDLE,
            // so the refresh-rate vote is cleared without waiting for an event.
            if self.voting.get()
                && let Some(t) = self.window.last_motion.get()
            {
                let left = (t + MOTION_IDLE).saturating_duration_since(std::time::Instant::now());
                timeout = Some(timeout.map_or(left, |x| x.min(left)));
            }
            let mut r = Ok(ControlFlow::Continue(()));
            self.app.poll_events(timeout, |e| {
                i_slint_core::platform::update_timers_and_animations();
                r = self.window.process_event(&e);
                if let Some(event_listener) = &self.event_listener {
                    event_listener(&e)
                }
            });
            if r?.is_break() {
                break;
            }
            // Chatter patch: while animating, draw every iteration so the swap
            // (not a timer) sets the frame rate; see the timeout above.
            let animating = self.window.window.has_active_animations()
                && self.app.native_window().is_some();
            if self.window.pending_redraw.take() || animating {
                let started = std::time::Instant::now();
                let presented = self.window.do_render()?;
                // Chatter patch: a frame floor under the vsync pacing. If the
                // swap does not block (swap interval refused, a driver that
                // queues ahead) or nothing was presented, the zero timeout
                // above would spin. A presented frame takes at least half a
                // vsync of the panel's top rate (4 ms until that is known); a
                // skipped one waits twice that.
                let floor = self.frame_floor.get() * if presented { 1 } else { 2 };
                if let Some(rest) = floor.checked_sub(started.elapsed()) {
                    std::thread::sleep(rest);
                }
            }
            self.update_refresh_rate_vote(animating);
        }
        Ok(())
    }

    fn new_event_loop_proxy(&self) -> Option<Box<dyn i_slint_core::platform::EventLoopProxy>> {
        Some(Box::new(AndroidEventLoopProxy {
            event_queue: self.window.event_queue.clone(),
            waker: self.app.create_waker(),
        }))
    }

    fn set_clipboard_text(&self, text: &str, clipboard: Clipboard) {
        if clipboard == Clipboard::DefaultClipboard {
            self.window
                .java_helper
                .set_clipboard(text)
                .unwrap_or_else(|e| javahelper::print_jni_error(&self.app, e));
        }
    }

    fn clipboard_text(&self, clipboard: Clipboard) -> Option<String> {
        if clipboard == Clipboard::DefaultClipboard {
            Some(
                self.window
                    .java_helper
                    .get_clipboard()
                    .unwrap_or_else(|e| javahelper::print_jni_error(&self.app, e)),
            )
        } else {
            None
        }
    }

    fn bind_context(&self, ctx: i_slint_core::SlintContextWeak, _: i_slint_core::InternalToken) {
        let ctx = ctx.upgrade().expect("bind_context called while the SlintContext is still alive");
        let color_scheme = match self
            .window
            .java_helper
            .color_scheme()
            .unwrap_or_else(|e| javahelper::print_jni_error(&self.app, e))
        {
            0x10 => i_slint_core::items::ColorScheme::Light, // UI_MODE_NIGHT_NO
            0x20 => i_slint_core::items::ColorScheme::Dark,  // UI_MODE_NIGHT_YES
            _ => i_slint_core::items::ColorScheme::Unknown,
        };
        ctx.set_color_scheme(color_scheme);
        if let Ok(accent) = self.window.java_helper.accent_color() {
            ctx.set_accent_color(accent);
        }
        if let Ok(scale) = self.window.java_helper.font_scale()
            && let Some(size) = javahelper::font_scale_to_logical_length(scale)
        {
            ctx.set_platform_default_font_size(Some(size));
        }
    }

    fn long_press_interval(&self, _: i_slint_core::InternalToken) -> Duration {
        self.window.java_helper.long_press_timeout().unwrap_or(Duration::from_millis(500))
    }
}

enum Event {
    Quit,
    Other(Box<dyn FnOnce() + Send + 'static>),
}

type EventQueue = Arc<Mutex<Vec<Event>>>;

struct AndroidEventLoopProxy {
    event_queue: EventQueue,
    waker: android_activity::AndroidAppWaker,
}

impl i_slint_core::platform::EventLoopProxy for AndroidEventLoopProxy {
    fn quit_event_loop(&self) -> Result<(), EventLoopError> {
        self.event_queue.lock().unwrap().push(Event::Quit);
        self.waker.wake();
        Ok(())
    }

    fn invoke_from_event_loop(
        &self,
        event: Box<dyn FnOnce() + Send>,
    ) -> Result<(), EventLoopError> {
        self.event_queue.lock().unwrap().push(Event::Other(event));
        self.waker.wake();
        Ok(())
    }
}

/// Chatter patch: an accepted send took the focused field's text (the app
/// has already cleared it). The IME's copy is consumed as of now: the gate
/// is told, and every state the IME produced before that reset was applied
/// on the UI thread is dropped by its generation when it arrives.
/// A no-op unless the IME serves a field whose text is now empty.
pub fn consume_text_input() {
    if let Some(adapter) = CURRENT_WINDOW.with_borrow(|x| x.upgrade()) {
        adapter.consume_text_input();
    }
}

/// Chatter patch: the app put text back into the focused field after
/// `consume_text_input` (a restored draft). The IME's copy is replaced as of
/// a new generation, so a snapshot of the emptied field that the consumption
/// left behind cannot overwrite the restored text when it arrives.
/// A no-op unless the IME serves a multi-line field that now holds text.
pub fn restore_text_input() {
    if let Some(adapter) = CURRENT_WINDOW.with_borrow(|x| x.upgrade()) {
        adapter.restore_text_input();
    }
}

/// Chatter patch: a state the IME produced, tagged by the gate. Any thread;
/// delivered on the event loop in arrival order.
pub fn deliver_text_input_state(state: InboundTextInput) {
    let _ = i_slint_core::api::invoke_from_event_loop(move || {
        if let Some(adapter) = CURRENT_WINDOW.with_borrow(|x| x.upgrade()) {
            adapter.handle_text_input_state(state);
        }
    });
}

/// Chatter patch: the gate applied the reset of `consume_text_input` for
/// `(owner, generation)` on the UI thread. No text travels with it and
/// nothing is cleared: it only ends the pending state of that consumption.
pub fn deliver_text_input_ack(owner: u64, generation: u64) {
    let _ = i_slint_core::api::invoke_from_event_loop(move || {
        if let Some(adapter) = CURRENT_WINDOW.with_borrow(|x| x.upgrade()) {
            adapter.handle_text_input_ack(owner, generation);
        }
    });
}

pub fn set_requested_graphics_api(
    requested_graphics_api: Option<i_slint_core::graphics::RequestedGraphicsAPI>,
) -> Result<(), PlatformError> {
    let Some(adapter) = CURRENT_WINDOW.with_borrow(|x| x.upgrade()) else {
        return Err(format!("On Android a graphics API for Slint can only be requested after calling slint::android::init()").into());
    };
    adapter.set_requested_graphics_api(requested_graphics_api);
    Ok(())
}

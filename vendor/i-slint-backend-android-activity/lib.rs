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
mod accessibility;
mod javahelper;
mod inset_dispatch;
mod text_input_offsets;
mod text_input_publication;
mod vsync;
mod native_pacing;
mod admission_drain;

#[link(name = "log")]
unsafe extern "C" {
    fn __android_log_write(
        priority: i32,
        tag: *const std::ffi::c_char,
        text: *const std::ffi::c_char,
    ) -> i32;
}

// Android-only backend. ATrace requires API 23+; Chatter uses minSdk 26.
// Static names and the enabled check keep disabled tracing allocation-free.
#[link(name = "android")]
unsafe extern "C" {
    fn ATrace_isEnabled() -> bool;
    fn ATrace_beginSection(name: *const std::ffi::c_char);
    fn ATrace_endSection();
}

struct AndroidTraceSection {
    enabled: bool,
    // ATrace sections must end on the thread where they began.
    _thread_bound: core::marker::PhantomData<Rc<()>>,
}

// Cached once by this helper crate, independent of trace session boundaries.
// Only exact "1" opts in; changing the property requires a process restart.
fn compact_trace() -> bool {
    static COMPACT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *COMPACT.get_or_init(|| {
        unsafe extern "C" {
            fn __system_property_get(name: *const std::ffi::c_char, value: *mut std::ffi::c_char) -> i32;
        }
        let mut value = [0u8; 92]; // bionic PROP_VALUE_MAX, including NUL
        // SAFETY: fixed property name and full-size writable buffer.
        let length = unsafe {
            __system_property_get(c"debug.chatter.compact_trace".as_ptr(), value.as_mut_ptr().cast())
        };
        length == 1 && value[0] == b'1'
    })
}

// Experiment gate: `setprop debug.chatter.front_buffer 1` switches the EGL surface
// to single-buffer (front-buffer) rendering during an active drag to cut compositor
// latency (tearing is the trade-off). Read once; default off.
fn front_buffer_experiment() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        unsafe extern "C" {
            fn __system_property_get(name: *const std::ffi::c_char, value: *mut std::ffi::c_char) -> i32;
        }
        let mut value = [0u8; 92];
        // SAFETY: fixed property name and full-size writable buffer.
        let length = unsafe {
            __system_property_get(c"debug.chatter.front_buffer".as_ptr(), value.as_mut_ptr().cast())
        };
        length == 1 && value[0] == b'1'
    })
}

impl AndroidTraceSection {
    fn new(name: &'static std::ffi::CStr) -> Self {
        let enabled = compact_scope_allowed(compact_trace(), name) && unsafe { ATrace_isEnabled() };
        if enabled {
            unsafe { ATrace_beginSection(name.as_ptr()) };
        }
        Self { enabled, _thread_bound: core::marker::PhantomData }
    }

    fn motion(name: &'static std::ffi::CStr, event_time_ns: i64) -> Self {
        let section = Self::new(name);
        if section.enabled {
            trace_motion_age(event_time_ns);
        }
        section
    }
}

fn compact_scope_allowed(compact: bool, name: &std::ffi::CStr) -> bool {
    !compact || matches!(name.to_bytes(),
        b"Chatter Android render"
        | b"Chatter Android Down dispatch"
        | b"Chatter Android Move dispatch"
        | b"Chatter Android Up dispatch"
        | b"Chatter Android Cancel dispatch")
}

impl Drop for AndroidTraceSection {
    fn drop(&mut self) {
        if self.enabled {
            unsafe { ATrace_endSection() };
        }
    }
}

// API 29 counters are optional: do not create a loader dependency on them
// for API 26-28 devices. Match Android's LP32/LP64 RTLD_DEFAULT definitions.
#[link(name = "dl")]
unsafe extern "C" {
    fn dlsym(
        handle: *mut std::ffi::c_void,
        symbol: *const std::ffi::c_char,
    ) -> *mut std::ffi::c_void;
}

// Android bionic time_t and timespec.tv_nsec are both C long, on LP32
// as well as LP64. Use clock_gettime (not clock_gettime64) with this ABI.
#[repr(C)]
struct AndroidTimespec {
    tv_sec: std::ffi::c_long,
    tv_nsec: std::ffi::c_long,
}

unsafe extern "C" {
    fn clock_gettime(clock_id: std::ffi::c_int, result: *mut AndroidTimespec) -> std::ffi::c_int;
}

// Called only inside an enabled motion trace. No Rust Instant epoch or
// animation clock participates; Android motion times use CLOCK_MONOTONIC.
fn trace_motion_age(event_time_ns: i64) {
    type SetCounter = unsafe extern "C" fn(*const std::ffi::c_char, i64);
    static SET_COUNTER: std::sync::OnceLock<Option<SetCounter>> = std::sync::OnceLock::new();
    let set_counter = SET_COUNTER.get_or_init(|| {
        #[cfg(target_pointer_width = "64")]
        let default_handle = std::ptr::null_mut();
        #[cfg(target_pointer_width = "32")]
        let default_handle = 0xffff_ffffusize as *mut std::ffi::c_void;
        // SAFETY: platform RTLD_DEFAULT and static NUL-terminated name.
        let symbol = unsafe { dlsym(default_handle, c"ATrace_setCounter".as_ptr()) };
        if symbol.is_null() {
            None
        } else {
            // SAFETY: exact Android function ABI; linked libandroid remains
            // loaded for the process lifetime.
            Some(unsafe { std::mem::transmute::<*mut std::ffi::c_void, SetCounter>(symbol) })
        }
    });
    let Some(set_counter) = set_counter else { return };
    if event_time_ns <= 0 {
        return;
    }
    let mut now = AndroidTimespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: CLOCK_MONOTONIC is 1 on Android; writable bionic timespec ABI.
    if unsafe { clock_gettime(1, &mut now) } != 0
        || now.tv_sec < 0
        || !(0..1_000_000_000).contains(&now.tv_nsec)
    {
        return;
    }
    let age_ns = i128::from(now.tv_sec) * 1_000_000_000 + i128::from(now.tv_nsec)
        - i128::from(event_time_ns);
    // Invalid/future samples are omitted rather than disguised as zero lag.
    if age_ns < 0 {
        return;
    }
    if let Ok(age_us) = i64::try_from(age_ns / 1_000) {
        // SAFETY: static name and resolved ABI; microseconds since this
        // motion sample, measured at native dispatch entry (not scanout).
        unsafe { set_counter(c"Chatter Android motion age us".as_ptr(), age_us) };
        // Exact GameActivity sample ms, after the original age probe so its
        // sampling point/order is preserved. No content or coordinates.
        unsafe { set_counter(c"Chatter Android motion event ms".as_ptr(), event_time_ns / 1_000_000) };
    }
}

/// Chatter patch: rare accessibility-bridge diagnostics (logcat tag SlintA11y).
pub(crate) fn a11y_log(message: &str) {
    if let Ok(message) = std::ffi::CString::new(message) {
        unsafe { __android_log_write(5, c"SlintA11y".as_ptr(), message.as_ptr()); }
    }
}

// Opt-in native probe for the Android 10 full-screen page touch cutoff. Build
// with CHATTER_PROFILE_TAP_PROBE=1 and read the ProfileTap logcat tag. Keep it
// compile-time gated so normal pointer/inset dispatch allocates nothing.
pub(crate) fn profile_tap_probe(message: &str) {
    if option_env!("CHATTER_PROFILE_TAP_PROBE").is_none() {
        return;
    }
    if let Ok(message) = std::ffi::CString::new(message) {
        unsafe { __android_log_write(4, c"ProfileTap".as_ptr(), message.as_ptr()); }
    }
}

#[cfg(all(not(feature = "aa-06"), feature = "aa-05"))]
pub use android_activity_05 as android_activity;
#[cfg(feature = "aa-06")]
pub use android_activity_06 as android_activity;

#[cfg(all(not(feature = "aa-06"), feature = "aa-05"))]
use ndk_08 as ndk;
#[cfg(feature = "aa-06")]
use ndk_09 as ndk;

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
    context: core::cell::OnceCell<i_slint_core::SlintContextWeak>,
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
            context: Default::default(),
            refresh_rate_vote: None,
            voting: Default::default(),
        }
    }

    /// Chatter patch: vote for the panel's top refresh rate only while the UI
    /// moves.
    ///
    /// `vote(true)` is called when an animation (a fling, a transition) or a
    /// touch drag starts, and `vote(false)` once neither has happened for
    /// `MOTION_IDLE`; only on those transitions, never per frame. It returns
    /// the panel's top rate in Hz (0 if unknown), or `None` if the vote did
    /// not land, in which case it is retried. Native Choreographer callbacks
    /// supply frame wakeups at the actual display rate.
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
        if vote(moving).is_some() {
            self.voting.set(moving);
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
        let ctx = self
            .context
            .get()
            .and_then(|ctx| ctx.upgrade())
            .expect("the event loop runs inside the context that owns this platform");
        let vsync = vsync::VsyncDriver::new(self.app.create_waker());
        let mut render_retry_at = None::<std::time::Instant>;
        let pacing_debug_supported = self.window.java_helper.debug_trace_supported();
        // admission_drain stays default-inert here: its `observed` flag requires the
        // retired native_mode==0 diagnostic path, so every admission_drain call in this
        // loop early-returns under the promoted pacing path. Kept (uncofigured) per the
        // migration brief, pending a separate cleanup decision.
        let mut pacing = native_pacing::Policy::new();
        let front_buffer_on = front_buffer_experiment();
        let mut front_active = false;
        loop {
            let mut timeout = ctx.duration_until_next_timer_update();
            let has_surface = self.app.native_window().is_some();
            let animating = self.window.window.has_active_animations();
            let now = std::time::Instant::now();
            let gate_wait = {
                let watchdog = vsync.watchdog_wait(now);
                pacing.observe(vsync.is_driving());
                if has_surface && (self.window.pending_redraw.get() || animating) {
                    vsync.request_frame();
                }
                watchdog.or_else(|| vsync.watchdog_wait(now))
            };
            vsync.set_animating(animating && has_surface);
            if front_buffer_on {
                // Front-buffer (single-buffer) only while a finger is actively
                // dragging (the motion vote); revert to double-buffer for flings,
                // animations and idle. Toggle on change only, never per frame.
                let want = has_surface && self.voting.get();
                if want != front_active {
                    self.window.renderer.set_front_buffer(want);
                    front_active = want;
                }
            }
            if let Some(zero) = pacing.interval_change() {
                let applied = self.window.renderer.set_android_swap_interval_zero(zero)?;
                if !zero && !applied { return Err("cannot restore Android swap interval".into()); }
                pacing.interval_applied(zero, applied);
            }
            let retry_wait = render_retry_at.map(|t| t.saturating_duration_since(now));
            if animating && has_surface && !vsync.is_driving() {
                let wait = Duration::from_millis(10);
                timeout = Some(timeout.map_or(wait, |x| x.min(wait)));
            } else if animating && !has_surface {
                let wait = Duration::from_millis(16);
                timeout = Some(timeout.map_or(wait, |x| x.min(wait)));
            }
            if has_surface && self.window.pending_redraw.get() {
                let wait = {
                    let drain = self.window.admission_drain.borrow();
                    if drain.observed { drain.wait(admission_drain::now_ns(), retry_wait) }
                    else { retry_wait.unwrap_or_default() }
                };
                let wait = if pacing.gating() && !pacing.first_frame() {
                    // A delivered credit needs only one normal nonblocking input
                    // poll. Failed submissions still wait for their retry deadline.
                    if vsync.has_permit() { wait }
                    else { wait.max(gate_wait.unwrap_or(Duration::from_millis(100))) }
                } else { wait };
                timeout = Some(timeout.map_or(wait, |x| x.min(wait)));
            }
            if let Some(wait) = gate_wait {
                timeout = Some(timeout.map_or(wait, |x| x.min(wait)));
            }
            // Wake independently when the motion refresh-rate vote expires.
            if self.voting.get()
                && let Some(t) = self.window.last_motion.get()
            {
                let left = (t + MOTION_IDLE).saturating_duration_since(std::time::Instant::now());
                timeout = Some(timeout.map_or(left, |x| x.min(left)));
            }
            // android-activity truncates timeout to integer milliseconds. Round
            // positive tails up; cap at its signed poll timeout representation.
            timeout = timeout.map(|wait| Duration::from_millis(
                (wait.as_millis() + u128::from(wait.subsec_nanos() % 1_000_000 != 0))
                    .min(i32::MAX as u128) as u64,
            ));
            let mut r = Ok(ControlFlow::Continue(()));
            let mut synchronous_render = false;
            {
                let _poll_trace = AndroidTraceSection::new(c"Chatter Android poll");
                self.app.poll_events(timeout, |e| {
                    // A poll can deliver multiple events. Keep the FIRST terminal
                    // outcome while still notifying the existing event listener.
                    if matches!(&r, Ok(ControlFlow::Continue(()))) {
                        // Updating the clock clears Slint's active-animation bit.
                        // Preserve an owed frame before advancing, including its
                        // final invalidation when no animation remains afterward.
                        if self.window.window.has_active_animations() {
                            self.window.pending_redraw.set(true);
                        }
                        ctx.update_timers_and_animations();
                        match &e {
                            PollEvent::Main(android_activity::MainEvent::InitWindow { .. }) => self.window.admission_drain.borrow_mut().cancel(7),
                            PollEvent::Main(android_activity::MainEvent::TerminateWindow { .. }) => self.window.admission_drain.borrow_mut().cancel(4),
                            PollEvent::Main(android_activity::MainEvent::Pause) => self.window.admission_drain.borrow_mut().cancel(3),
                            PollEvent::Main(android_activity::MainEvent::Destroy) => self.window.admission_drain.borrow_mut().cancel(8),
                            PollEvent::Main(android_activity::MainEvent::RedrawNeeded { .. }) => self.window.admission_drain.borrow_mut().cancel(5),
                            _ => (),
                        }
                        match &e {
                            PollEvent::Main(android_activity::MainEvent::InitWindow { .. }) => {
                                pacing.surface_changed(true);
                                vsync.surface_changed(true);
                                // A replacement surface gets its first frame immediately.
                                render_retry_at = None;
                                self.window.pending_redraw.set(true);
                            }
                            PollEvent::Main(android_activity::MainEvent::RedrawNeeded { .. }) => {
                                // Android requires synchronous rendering here.
                                synchronous_render = true;
                                render_retry_at = None;
                            }
                            PollEvent::Main(android_activity::MainEvent::TerminateWindow { .. }) => {
                                pacing.surface_changed(false);
                                vsync.surface_changed(false);
                            }
                            _ => (),
                        }
                        let _event_trace = match &e {
                            PollEvent::Main(android_activity::MainEvent::InputAvailable) => {
                                Some(AndroidTraceSection::new(c"Chatter Android input"))
                            }
                            PollEvent::Main(android_activity::MainEvent::RedrawNeeded { .. }) => {
                                Some(AndroidTraceSection::new(c"Chatter Android sync render"))
                            }
                            _ => None,
                        };
                        if matches!(&e, PollEvent::Main(android_activity::MainEvent::RedrawNeeded { .. })) {
                            // This OS-required draw happens inside process_event. Restore
                            // before it too, including when the poll crossed the deadline.
                            let _ = vsync.watchdog_wait(std::time::Instant::now());
                            pacing.observe(vsync.is_driving());
                            if pacing.interval_change() == Some(false) {
                                match self.window.renderer.set_android_swap_interval_zero(false) {
                                    Ok(false) => r = Err("cannot restore Android swap interval".into()),
                                    Ok(true) => pacing.interval_applied(false, true),
                                    Err(error) => r = Err(error),
                                }
                            }
                        }
                        if matches!(&r, Ok(ControlFlow::Continue(()))) {
                            r = self.window.process_event(&e);
                        }
                        if matches!(&r, Ok(ControlFlow::Continue(()))) {
                            // process_event(InitWindow) has now created the surface.
                            pacing.observe(vsync.is_driving());
                            if let Some(zero) = pacing.interval_change() {
                                let applied = self.window.renderer.set_android_swap_interval_zero(zero);
                                match applied {
                                    Ok(false) if !zero => r = Err("cannot restore Android swap interval".into()),
                                    Ok(applied) => pacing.interval_applied(zero, applied),
                                    Err(error) => r = Err(error),
                                }
                            }
                        }
                    }
                    if let Some(event_listener) = &self.event_listener {
                        event_listener(&e)
                    }
                });
            }
            if r?.is_break() {
                break;
            }
            {
                // The poll may have reached the watchdog deadline. Restore Wait1
                // before admitting an ordinary fallback frame on this iteration.
                let _ = vsync.watchdog_wait(std::time::Instant::now());
                pacing.observe(vsync.is_driving());
                if let Some(zero) = pacing.interval_change() {
                    let applied = self.window.renderer.set_android_swap_interval_zero(zero)?;
                    if !zero && !applied { return Err("cannot restore Android swap interval".into()); }
                    pacing.interval_applied(zero, applied);
                }
                if synchronous_render { pacing.render_started(); }
            }
            let animating = self.window.window.has_active_animations()
                && self.app.native_window().is_some();
            let retry_ready = render_retry_at
                .is_none_or(|deadline| std::time::Instant::now() >= deadline);
            let render_ready = self.app.native_window().is_some()
                && self.window.pending_redraw.get() && retry_ready && !synchronous_render;
            let admitted = {
                // Gated credit belongs to one draw in this surface epoch. Keep an
                // unused delivery across idle wakes and retry-ineligible polls;
                // synchronous rendering consumes it through this bookkeeping too.
                let permit = (!pacing.gating() || render_ready || synchronous_render)
                    && vsync.take_permit();
                !pacing.gating() || pacing.first_frame() || permit
            };
            let admitted = admitted && (!render_ready || {
                let mut drain = self.window.admission_drain.borrow_mut();
                !drain.observed || drain.admit(admission_drain::now_ns())
            });
            if render_ready && admitted {
                pacing.render_started();
                // Reserve the next callback while this ordinary frame renders.
                // An idle callback retains one credit without reserving another.
                if pacing.gating() { vsync.request_frame(); }
                if pacing_debug_supported {
                    native_pacing::trace(&pacing, vsync.watchdog_failed());
                }
                // Clear before rendering so observer redraws survive submission.
                self.window.pending_redraw.set(false);
                let presented = {
                    let _render_trace = AndroidTraceSection::new(c"Chatter Android render");
                    self.window.do_render()?
                };
                render_retry_at = if presented {
                    None
                } else {
                    // Preserve work without spinning if the renderer cannot
                    // submit. This is a retry backoff, not normal frame pacing.
                    self.window.pending_redraw.set(true);
                    Some(std::time::Instant::now() + Duration::from_millis(16))
                };
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
        let _ = self.context.set(ctx.clone());
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

/// Observe successful frame submissions on the Android event-loop thread.
///
/// This replaces the observer for the current Android window. Call it on the
/// event-loop thread after the platform has been created. Unlike a rendering
/// notifier, this callback has no graphics API access and does not disable
/// partial rendering or force an extra GPU flush. It runs after rendering and
/// surface submission succeed, not after physical display scanout.
///
/// Missing surfaces and skipped/failed frames do not invoke the callback; it
/// remains registered across surface recreation until replaced or the window
/// adapter is dropped. Registration does not itself request a redraw.
pub fn set_frame_presented_callback(callback: impl Fn() + 'static) -> Result<(), PlatformError> {
    let adapter = CURRENT_WINDOW.with_borrow(|window| window.upgrade())
        .ok_or_else(|| PlatformError::from("Android window is not available"))?;
    *adapter.frame_presented_callback.borrow_mut() = Some(Rc::new(callback));
    Ok(())
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

/// Chatter patch: the app replaced the focused field's text (a draft put
/// back after `consume_text_input`, the message text an edit opens on). The
/// IME's copy is replaced as of a new generation, so a snapshot of the field
/// as it was before (emptied by the consumption, or still the old draft)
/// cannot overwrite the new text when it arrives. The caret goes to the end
/// of the new text on both sides.
/// A no-op unless the IME serves a multi-line field.
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

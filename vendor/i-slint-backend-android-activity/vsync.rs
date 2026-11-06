// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

// cSpell: ignore ALOOPER Condvar wakeups

//! Drives frames from the display's vsync using the Android Choreographer.
//!
//! The Choreographer delivers its callback through the `ALooper` it is fetched on, and
//! `ALooper_pollOnce` reports that as `ALOOPER_POLL_CALLBACK`, which the android-activity
//! `poll_events` wrapper logs as a spurious error. To keep the Choreographer off the main
//! looper, a dedicated helper thread owns its own looper, waits for each vsync, and wakes
//! the main event loop through its `AndroidAppWaker`. The main loop then advances the
//! animations and renders. Pacing therefore follows the actual refresh rate instead of a
//! fixed poll timeout.
//!
//! Demand is `surface && !failed && (animating || request)`: an active Slint animation,
//! OR an explicit per-frame `request` raised by the main loop while a drag has a pending
//! redraw. Each granted vsync hands the main loop ONE `permit`, so the producer renders
//! exactly one frame per Choreographer vsync during a manual drag. With no animation and
//! no pending redraw the demand drops and the helper parks — zero wakes at idle.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use crate::android_activity::AndroidAppWaker;
use crate::ndk::looper::{ForeignLooper, ThreadLooper};

/// The Choreographer is not wrapped by the `ndk` crate. Available since API 24; Slint's
/// Android minimum is API 26, so the symbols are always present.
mod ffi {
    use core::ffi::{c_long, c_void};

    #[repr(C)]
    pub struct AChoreographer {
        _unused: [u8; 0],
    }
    pub type FrameCallback = unsafe extern "C" fn(frame_time_nanos: c_long, data: *mut c_void);

    #[link(name = "android")]
    unsafe extern "C" {
        pub fn AChoreographer_getInstance() -> *mut AChoreographer;
        pub fn AChoreographer_postFrameCallback(
            choreographer: *mut AChoreographer,
            callback: FrameCallback,
            data: *mut c_void,
        );
    }
}

struct Shared {
    state: Mutex<State>,
    cond: Condvar,
    /// Set once the helper thread has a working Choreographer and can wake the main
    /// loop at every vsync. Until then (and if the thread never starts) the main loop
    /// keeps its own periodic wakeup so animations still advance.
    driving: AtomicBool,
    /// The helper thread's looper, published so the main thread can wake it on shutdown.
    looper: Mutex<Option<ForeignLooper>>,
    /// Wakes the main event loop when a vsync arrives.
    main_waker: AndroidAppWaker,
}

#[derive(Default)]
struct State {
    pacing: PacingState,
    animating: bool,
    quit: bool,
}

/// Wakes the main event loop at each display vsync while frames are demanded.
pub struct VsyncDriver {
    shared: Arc<Shared>,
    handle: Option<JoinHandle<()>>,
}

#[derive(Default)]
struct PacingState {
    surface: bool,
    epoch: u64,
    request: bool,
    permit: bool,
    requested_at: Option<std::time::Instant>,
    failed: bool,
}

thread_local! {
    // The callback never dereferences FFI data, including during shutdown.
    static CALLBACK_FIRED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

impl VsyncDriver {
    pub fn surface_changed(&self, present: bool) {
        let mut state = self.shared.state.lock().unwrap();
        state.pacing.epoch = state.pacing.epoch.wrapping_add(1);
        state.pacing.permit = false;
        state.pacing.request = false;
        state.pacing.surface = present;
        // An old callback remains outstanding, but suspension owes no frame.
        // Current demand after resume arms a fresh deadline, even while awaiting it.
        state.pacing.requested_at = None;
        self.shared.cond.notify_one();
    }

    pub fn request_frame(&self) {
        let mut state = self.shared.state.lock().unwrap();
        if state.pacing.surface && !state.pacing.failed && !state.pacing.permit {
            state.pacing.request = true;
            state.pacing.requested_at.get_or_insert_with(std::time::Instant::now);
            self.shared.cond.notify_one();
        }
    }

    // One scheduling credit only: no UI content or EGL buffer ownership.
    pub fn has_permit(&self) -> bool {
        self.shared.state.lock().unwrap().pacing.permit
    }

    pub fn take_permit(&self) -> bool {
        std::mem::take(&mut self.shared.state.lock().unwrap().pacing.permit)
    }

    pub fn watchdog_failed(&self) -> bool {
        self.shared.state.lock().unwrap().pacing.failed
    }

    pub fn watchdog_wait(&self, now: std::time::Instant) -> Option<std::time::Duration> {
        let mut state = self.shared.state.lock().unwrap();
        if !state.pacing.surface { return None; }
        let deadline = state.pacing.requested_at? + std::time::Duration::from_millis(100);
        if now >= deadline {
            state.pacing.failed = true;
            state.pacing.permit = false;
            state.pacing.request = false;
            state.pacing.requested_at = None;
            self.shared.driving.store(false, Ordering::Release);
            None
        } else {
            Some(deadline.saturating_duration_since(now))
        }
    }

    pub fn new(main_waker: AndroidAppWaker) -> Self {
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            cond: Condvar::new(),
            driving: AtomicBool::new(false),
            looper: Mutex::new(None),
            main_waker,
        });
        let handle = std::thread::Builder::new()
            .name("slint vsync".into())
            .spawn({
                let shared = shared.clone();
                move || vsync_thread(shared)
            })
            .ok();
        Self { shared, handle }
    }

    /// Whether the helper thread is driving frames from vsync. When it is not, the
    /// caller must keep waking itself so that animations still advance.
    pub fn is_driving(&self) -> bool {
        if self.shared.state.lock().unwrap().pacing.failed { return false; }
        self.shared.driving.load(Ordering::Acquire)
    }

    /// Publishes whether the main loop currently has active animations. Turning it on
    /// starts the per-vsync wakeups; turning it off parks the helper after the next frame.
    pub fn set_animating(&self, animating: bool) {
        let mut state = self.shared.state.lock().unwrap();
        if state.animating != animating {
            state.animating = animating;
            drop(state);
            self.shared.cond.notify_one();
        }
    }
}

impl Drop for VsyncDriver {
    fn drop(&mut self) {
        self.shared.state.lock().unwrap().quit = true;
        self.shared.cond.notify_one();
        // Interrupt a poll that may otherwise block until the next vsync, or forever
        // while the app is in the background.
        if let Some(looper) = self.shared.looper.lock().unwrap().as_ref() {
            looper.wake();
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn vsync_thread(shared: Arc<Shared>) {
    let looper = ThreadLooper::prepare();
    *shared.looper.lock().unwrap() = Some(looper.as_foreign().clone());

    // SAFETY: `AChoreographer_getInstance` needs a looper on the calling thread, which
    // `ThreadLooper::prepare` just established.
    let choreographer = unsafe { ffi::AChoreographer_getInstance() };
    if choreographer.is_null() {
        return;
    }
    shared.driving.store(true, Ordering::Release);

    loop {
        {
            let mut state = shared.state.lock().unwrap();
            while !state.quit {
                let demand = state.pacing.surface && !state.pacing.failed
                    && (state.animating || state.pacing.request);
                if demand { break; }
                state = shared.cond.wait(state).unwrap();
            }
            if state.quit {
                break;
            }
        }

        let epoch = {
            let mut state = shared.state.lock().unwrap();
            if state.quit { return; }
            // Recheck after the parked wait: surface/failed state may have changed.
            if !state.pacing.surface || state.pacing.failed
                || !(state.animating || state.pacing.request) { continue; }
            state.pacing.request = false;
            state.pacing.requested_at.get_or_insert_with(std::time::Instant::now);
            CALLBACK_FIRED.set(false);
            // Serialize posting with surface invalidation. The helper retains
            // this single callback across suspension until it is delivered.
            // SAFETY: thread-owned Choreographer; callback touches only its TLS flag.
            unsafe {
                ffi::AChoreographer_postFrameCallback(
                    choreographer, pacing_frame_callback, std::ptr::null_mut(),
                );
            }
            state.pacing.epoch
        };
        loop {
            // Unrelated looper wakes do not grant a permit or post another callback.
            let result = {
                let _trace = crate::AndroidTraceSection::new(c"Chatter Android vsync wait");
                looper.poll_once()
            };
            let mut state = shared.state.lock().unwrap();
            if state.quit { return; }
            // Ok(Poll::Callback) is a normal return; only an actual poll error
            // disables the driver. Null callback data remains safe on exit.
            if result.is_err() {
                state.pacing.failed = true;
                state.pacing.permit = false;
                state.pacing.request = false;
                state.pacing.requested_at = None;
                shared.driving.store(false, Ordering::Release);
                drop(state);
                let _trace = crate::AndroidTraceSection::new(c"Chatter Android vsync wake");
                shared.main_waker.wake();
                return;
            }
            drop(state);
            if CALLBACK_FIRED.replace(false) { break; }
        }
        let mut state = shared.state.lock().unwrap();
        // A stale callback must not clear the resumed demand's deadline.
        if state.pacing.surface && !state.pacing.failed && state.pacing.epoch == epoch {
            state.pacing.requested_at = None;
            state.pacing.permit = true;
            state.pacing.request = false;
            let _trace = crate::AndroidTraceSection::new(c"Chatter Android vsync wake");
            shared.main_waker.wake();
        }
    }
}

extern "C" fn pacing_frame_callback(
    _frame_time_nanos: core::ffi::c_long,
    _data: *mut core::ffi::c_void,
) {
    CALLBACK_FIRED.set(true);
}

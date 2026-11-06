// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

//! DEBUG EXPERIMENT only. ABI verified against games-frame-pacing:2.1.3 AAR
//! prefab/modules/swappy/include/swappy/{swappyGL.h,swappyGL_extra.h}.
//! No pacing policy for production; no content data or readback.
use std::sync::atomic::{AtomicBool, Ordering};

static OWNED: AtomicBool = AtomicBool::new(false);

struct Lease;
impl Lease {
    fn acquire(flag: &AtomicBool) -> bool {
        flag.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
    fn new() -> Option<Self> {
        Self::acquire(&OWNED).then(|| Self)
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        OWNED.store(false, Ordering::Release);
    }
}

const INTERVAL_NS: u64 = 16_666_667;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Fix0,
    Fix8,
}
impl Mode {
    fn parse(property: &[u8]) -> Option<Self> {
        match property {
            b"fix0" => Some(Self::Fix0),
            b"fix8" => Some(Self::Fix8),
            _ => None,
        }
    }
    fn wait(self) -> i32 {
        match self {
            Self::Fix0 => 0,
            Self::Fix8 => 8,
        }
    }
    fn trace(self) -> i64 {
        match self {
            Self::Fix0 => 1,
            Self::Fix8 => 2,
        }
    }
}

fn requested(debuggable: bool, api: i32, property: &[u8]) -> bool {
    debuggable && api >= 29 && Mode::parse(property).is_some()
}

// Read only after the actual Java debuggable/API guard. Fail closed on typos.
fn auto_requested(property: &[u8]) -> Option<bool> {
    match property {
        b"" | b"0" => Some(false),
        b"1" => Some(true),
        _ => None,
    }
}

// Exact public SwappyStats layout: 25 uint64_t values, 200 bytes, alignment 8.
#[repr(C)]
#[derive(Default)]
struct Stats {
    total: u64,
    idle: [u64; 6],
    late: [u64; 6],
    offset: [u64; 6],
    latency: [u64; 6],
}

// The pinned SDK filters PENDING (-2) but not INVALID (-1). These are
// independent EGL validity observations, never a fabricated SDK sample count.
#[derive(Debug, PartialEq, Eq)]
enum Sample {
    Pending,
    Invalid,
    Valid,
}
fn sample(values: [i64; 4]) -> Sample {
    if values.iter().any(|v| *v < 0 && *v != -2) {
        Sample::Invalid
    } else if values.contains(&-2) {
        Sample::Pending
    } else if values[1..].contains(&0) {
        Sample::Invalid
    } else {
        Sample::Valid
    }
}

// Fixed capacity; no allocation, images, timer, thread, wait, or new frame.
// Only successfully submitted frame IDs enter this queue.
#[derive(Default)]
struct Pending {
    ids: [Option<u64>; 11],
    head: usize,
    len: usize,
}
impl Pending {
    fn push(&mut self, id: u64) -> bool {
        if self.len == self.ids.len() {
            return false;
        }
        self.ids[(self.head + self.len) % self.ids.len()] = Some(id);
        self.len += 1;
        true
    }
    fn front(&self) -> Option<u64> {
        self.ids[self.head]
    }
    fn pop(&mut self) {
        if self.len > 0 {
            self.ids[self.head] = None;
            self.head = (self.head + 1) % self.ids.len();
            self.len -= 1;
        }
    }
}

fn application_is_debuggable(flags: i32) -> bool {
    flags & 2 != 0 // Android ApplicationInfo.FLAG_DEBUGGABLE
}

// Weak registration never extends a backend lifetime on its own. Each SDK
// owner upgrades it and retains AndroidApp until after SwappyGL_destroy.
thread_local! {
    static ACTIVITY: std::cell::RefCell<std::rc::Weak<ActivityContext>> = Default::default();
}

struct ActivityContext {
    vm: *mut std::ffi::c_void,
    activity: *mut std::ffi::c_void,
    _owner: Box<dyn std::any::Any>,
}

/// Lifetime guard for the debug experiment's Activity registration.
/// Keep this alive while constructing/recreating renderer surfaces.
#[must_use = "keep the Activity registration alive while constructing renderer surfaces"]
pub struct AndroidSwappyActivity(std::rc::Rc<ActivityContext>);

impl AndroidSwappyActivity {
    /// Register the backend's actual Activity on the rendering thread.
    ///
    /// # Safety
    /// Non-null `vm` must be a live JavaVM and non-null `activity` a global Activity reference
    /// belonging to it. `owner` must keep both valid until it is dropped.
    /// Null pointers are accepted and cause initialization to fall back.
    /// Registration and all surface creation must occur on this thread.
    pub unsafe fn new(
        vm: *mut std::ffi::c_void,
        activity: *mut std::ffi::c_void,
        owner: impl std::any::Any,
    ) -> Self {
        let context = std::rc::Rc::new(ActivityContext {
            vm,
            activity,
            _owner: Box::new(owner),
        });
        ACTIVITY.with(|slot| *slot.borrow_mut() = std::rc::Rc::downgrade(&context));
        Self(context)
    }
}

impl Drop for AndroidSwappyActivity {
    fn drop(&mut self) {
        ACTIVITY.with(|slot| {
            let mut slot = slot.borrow_mut();
            if slot.ptr_eq(&std::rc::Rc::downgrade(&self.0)) {
                *slot = Default::default();
            }
        });
    }
}

#[cfg(target_os = "android")]
mod android {
    use super::*;
    use glutin::{
        display::{AsRawDisplay, RawDisplay},
        surface::{AsRawSurface, RawSurface, Surface, WindowSurface},
    };
    use std::ffi::{c_char, c_void};
    type Handle = *mut c_void;
    // C++ bool is one byte in the Android ABI. EGL handles and ANativeWindow*
    // are opaque pointers; JNIEnv* and jobject use jni's exact JNI types.
    type Init = unsafe extern "C" fn(*mut jni::sys::JNIEnv, jni::sys::jobject) -> bool;
    type Toggle = unsafe extern "C" fn(bool);
    struct Api {
        init: Init,
        enabled: unsafe extern "C" fn() -> bool,
        destroy: unsafe extern "C" fn(),
        window: unsafe extern "C" fn(Handle) -> bool,
        swap: unsafe extern "C" fn(Handle, Handle) -> bool,
        interval: unsafe extern "C" fn(u64),
        auto_interval: Toggle,
        auto_pipeline: Toggle,
        stats: Toggle,
        get_interval: unsafe extern "C" fn() -> u64,
        get_refresh: unsafe extern "C" fn() -> u64,
        fix_wait: unsafe extern "C" fn(i32),
        record: unsafe extern "C" fn(Handle, Handle),
        get_stats: unsafe extern "C" fn(*mut Stats),
        clear_stats: unsafe extern "C" fn(),
    }
    #[link(name = "dl")]
    unsafe extern "C" {
        fn dlopen(name: *const c_char, flags: i32) -> Handle;
        fn dlsym(library: Handle, name: *const c_char) -> Handle;
        fn dlclose(library: Handle) -> i32;
    }
    unsafe extern "C" {
        fn __system_property_get(name: *const c_char, value: *mut c_char) -> i32;
    }

    impl Api {
        fn load() -> Option<&'static Self> {
            static API: std::sync::OnceLock<Option<Api>> = std::sync::OnceLock::new();
            API.get_or_init(|| unsafe {
                // Bare soname only: Android's app linker namespace/package native
                // library search path. Never probe /data/local/tmp or other paths.
                stage(80); // next: dlopen
                let library = dlopen(c"libswappy.so".as_ptr(), 2); // RTLD_NOW | RTLD_LOCAL
                stage(81); // dlopen returned
                if library.is_null() {
                    return None;
                }
                macro_rules! symbol {
                    ($name:literal, $ty:ty) => {{
                        let p = dlsym(library, $name.as_ptr());
                        if p.is_null() {
                            dlclose(library);
                            return None;
                        }
                        core::mem::transmute::<Handle, $ty>(p)
                    }};
                }
                let api = Api {
                    init: symbol!(c"SwappyGL_init", Init),
                    enabled: symbol!(c"SwappyGL_isEnabled", unsafe extern "C" fn() -> bool),
                    destroy: symbol!(c"SwappyGL_destroy", unsafe extern "C" fn()),
                    window: symbol!(c"SwappyGL_setWindow", unsafe extern "C" fn(Handle) -> bool),
                    swap: symbol!(
                        c"SwappyGL_swap",
                        unsafe extern "C" fn(Handle, Handle) -> bool
                    ),
                    interval: symbol!(c"SwappyGL_setSwapIntervalNS", unsafe extern "C" fn(u64)),
                    auto_interval: symbol!(c"SwappyGL_setAutoSwapInterval", Toggle),
                    auto_pipeline: symbol!(c"SwappyGL_setAutoPipelineMode", Toggle),
                    stats: symbol!(c"SwappyGL_enableStats", Toggle),
                    get_interval: symbol!(
                        c"SwappyGL_getSwapIntervalNS",
                        unsafe extern "C" fn() -> u64
                    ),
                    get_refresh: symbol!(
                        c"SwappyGL_getRefreshPeriodNanos",
                        unsafe extern "C" fn() -> u64
                    ),
                    fix_wait: symbol!(
                        c"SwappyGL_setBufferStuffingFixWait",
                        unsafe extern "C" fn(i32)
                    ),
                    record: symbol!(
                        c"SwappyGL_recordFrameStart",
                        unsafe extern "C" fn(Handle, Handle)
                    ),
                    get_stats: symbol!(c"SwappyGL_getStats", unsafe extern "C" fn(*mut Stats)),
                    clear_stats: symbol!(c"SwappyGL_clearStats", unsafe extern "C" fn()),
                };
                stage(82); // all symbols resolved
                // Pin code for the process: JNI native registrations can outlive
                // SDK destruction. SDK resources/threads still die with the owner.
                Some(api)
            })
            .as_ref()
        }
    }

    impl Api {
        fn configure(&self, mode: Mode, auto: bool) -> bool {
            // Public setters return void: this is intention to treat, not
            // acceptance readback or an assertion of actual nonpipeline mode.
            crate::renderer_trace::counter(c"Chatter Swappy auto requested", i64::from(auto));
            unsafe {
                (self.auto_interval)(auto);
                (self.auto_pipeline)(auto);
                (self.interval)(INTERVAL_NS);
                (self.stats)(true);
                (self.clear_stats)();
                (self.fix_wait)(mode.wait());
            }
            self.verify_timing()
        }

        fn verify_timing(&self) -> bool {
            let refresh = unsafe { (self.get_refresh)() };
            let interval = unsafe { (self.get_interval)() };
            // Emit both public readbacks before any rejection, including init.
            for (name, value) in [
                (c"Chatter Swappy refresh ns", refresh),
                (c"Chatter Swappy interval ns", interval),
            ] {
                crate::renderer_trace::counter(name, value.min(i64::MAX as u64) as i64);
            }
            // S9-only nominal-60 trial: floor/ceil(1e9 / 60), not a rate band.
            // The SDK returns refresh * integer swap count, not the request.
            // Require exactly ONE actual period; never tolerate a mismatch.
            (16_666_666..=INTERVAL_NS).contains(&refresh) && interval == refresh
        }
    }

    // Only the four fields queried by this pinned SDK. Sample one natural
    // render per mixed block of eight; poll each retained ID once per render.
    // Equal instrumentation in both SDK arms. This replaces the
    // app's general seven-field EGL observer while Swappy owns this surface.
    const FIELDS: [i32; 4] = [0x3434, 0x3435, 0x3436, 0x343A];
    type Next = unsafe extern "C" fn(Handle, Handle, *mut u64) -> u32;
    type Get = unsafe extern "C" fn(Handle, Handle, u64, i32, *const i32, *mut i64) -> u32;
    type Attrib = unsafe extern "C" fn(Handle, Handle, i32, i32) -> u32;
    type Supported = unsafe extern "C" fn(Handle, Handle, i32) -> u32;
    type GetError = unsafe extern "C" fn() -> i32;

    // Closed numeric diagnostic vocabulary; never infer an unconsumed EGL error.
    #[derive(Clone, Copy, PartialEq, Eq)]
    #[repr(i64)]
    enum Reason {
        None = 0,
        HistoryExpired = 1,
        CollectionDisabled = 2,
        Unsupported = 3,
        Context = 4,
        Unexplained = 5,
        Invalid = 6,
        NextId = 7,
        Interrupted = 8,
        Overflow = 9,
        SwapFailed = 10,
        IdentityRange = 11,
    }
    fn egl_reason(error: i32) -> Reason {
        match error {
            0x3002 => Reason::HistoryExpired, // EGL_BAD_ACCESS: limited history
            0x300D => Reason::CollectionDisabled, // EGL_BAD_SURFACE
            0x300C => Reason::Unsupported,    // EGL_BAD_PARAMETER
            0x3001 | 0x3006 | 0x3007 | 0x3008 | 0x300E => Reason::Context,
            _ => Reason::Unexplained, // includes FALSE with EGL_SUCCESS
        }
    }

    struct Probe {
        display: Handle,
        surface: Handle,
        next: Next,
        get: Get,
        attrib: Attrib,
        get_error: GetError,
        sampling: crate::android_frame_timestamps::Sampling,
        sampled: bool,
        expired: i64,
        disabled: i64,
        unsupported: i64,
        context_errors: i64,
        unexplained: i64,
        next_errors: i64,
        interrupted: i64,
        egl_reason: Reason,
        failure_reason: Reason,
        pending: Pending,
        started: Option<u64>,
        valid: i64,
        invalid: i64,
        errors: i64,
        overflow: i64,
        usable: bool,
    }
    impl Probe {
        fn new(
            display: &glutin::display::Display,
            surface: &Surface<WindowSurface>,
        ) -> Option<Self> {
            use glutin::display::GlDisplay;
            let (d, s) = handles(display, surface)?;
            unsafe {
                macro_rules! load {
                    ($name:literal, $ty:ty) => {{
                        let ptr = display.get_proc_address($name);
                        if ptr.is_null() {
                            return None;
                        }
                        core::mem::transmute::<*const c_void, $ty>(ptr)
                    }};
                }
                let query = load!(
                    c"eglQueryString",
                    unsafe extern "C" fn(Handle, i32) -> *const c_char
                );
                let extensions = query(d, 0x3055); // EGL_EXTENSIONS
                if extensions.is_null()
                    || !std::ffi::CStr::from_ptr(extensions)
                        .to_bytes()
                        .split(|b| *b == b' ')
                        .any(|e| e == b"EGL_ANDROID_get_frame_timestamps")
                {
                    return None;
                }
                let get_error = load!(c"eglGetError", GetError);
                let supported = load!(c"eglGetFrameTimestampSupportedANDROID", Supported);
                let mask = FIELDS.iter().enumerate().fold(0i64, |m, (i, f)| {
                    m | (i64::from(supported(d, s, *f) != 0) << i)
                });
                crate::renderer_trace::counter(c"Chatter Swappy timestamp support mask", mask);
                if mask != 15 {
                    crate::renderer_trace::counter(
                        c"Chatter Swappy failure reason",
                        Reason::Unsupported as i64,
                    );
                    return None;
                }
                let next = load!(c"eglGetNextFrameIdANDROID", Next);
                let get = load!(c"eglGetFrameTimestampsANDROID", Get);
                let attrib = load!(c"eglSurfaceAttrib", Attrib);
                if attrib(d, s, 0x3430, 1) == 0 {
                    // Consume only this failed diagnostic call, before any other EGL call.
                    let reason = egl_reason(get_error());
                    crate::renderer_trace::counter(c"Chatter Swappy EGL reason", reason as i64);
                    crate::renderer_trace::counter(c"Chatter Swappy failure reason", reason as i64);
                    return None;
                } // EGL_TIMESTAMPS_ANDROID
                Some(Self {
                    display: d,
                    surface: s,
                    next,
                    get,
                    attrib,
                    get_error,
                    sampling: Default::default(),
                    sampled: false,
                    expired: 0,
                    disabled: 0,
                    unsupported: 0,
                    context_errors: 0,
                    unexplained: 0,
                    next_errors: 0,
                    interrupted: 0,
                    egl_reason: Reason::None,
                    failure_reason: Reason::None,
                    pending: Pending::default(),
                    started: None,
                    valid: 0,
                    invalid: 0,
                    errors: 0,
                    overflow: 0,
                    usable: true,
                })
            }
        }
        fn fail(&mut self, reason: Reason) {
            self.usable = false;
            if self.failure_reason == Reason::None {
                self.failure_reason = reason;
            }
        }
        fn query_failed(&mut self, sampled_history: bool) {
            // EGL errors are thread-local. Do not pre-drain or read after successful
            // calls, SDK swaps, or SDK capture; only our immediately failed query.
            let reason = egl_reason(unsafe { (self.get_error)() });
            self.egl_reason = reason;
            match reason {
                Reason::HistoryExpired if sampled_history => {
                    self.expired = self.expired.saturating_add(1);
                    return; // expected retirement, no timestamp emitted
                }
                Reason::CollectionDisabled => self.disabled = self.disabled.saturating_add(1),
                Reason::Unsupported => self.unsupported = self.unsupported.saturating_add(1),
                Reason::Context => self.context_errors = self.context_errors.saturating_add(1),
                _ => self.unexplained = self.unexplained.saturating_add(1),
            }
            self.errors = self.errors.saturating_add(1);
            self.fail(reason);
        }
        fn poll(&mut self) {
            if self.started.take().is_some() {
                self.errors = self.errors.saturating_add(1);
                self.interrupted = self.interrupted.saturating_add(1);
                self.fail(Reason::Interrupted);
            }
            // Snapshot length; each of at most 11 retained samples is queried once.
            // Requeue only PENDING. A pending head cannot starve younger ready IDs.
            for _ in 0..self.pending.len {
                let id = self.pending.front().unwrap();
                self.pending.pop();
                let mut values = [-2; 4];
                if unsafe {
                    (self.get)(
                        self.display,
                        self.surface,
                        id,
                        4,
                        FIELDS.as_ptr(),
                        values.as_mut_ptr(),
                    )
                } == 0
                {
                    self.query_failed(true);
                    continue;
                }
                match sample(values) {
                    Sample::Pending => {
                        assert!(self.pending.push(id));
                    }
                    Sample::Invalid => {
                        self.invalid = self.invalid.saturating_add(1);
                        self.fail(Reason::Invalid);
                    }
                    Sample::Valid => {
                        self.valid = self.valid.saturating_add(1);
                        if let Ok(id) = i64::try_from(id) {
                            crate::renderer_trace::counter(c"Chatter Swappy sampled ID", id);
                            crate::renderer_trace::counter(
                                c"Chatter Swappy actual present ns",
                                values[3],
                            );
                        } else {
                            self.fail(Reason::IdentityRange);
                        }
                    }
                }
            }
        }
        fn start(&mut self) {
            self.sampled = self.sampling.next(true);
            let mut id = 0;
            if unsafe { (self.next)(self.display, self.surface, &mut id) } == 0 {
                self.fail(Reason::NextId);
                self.query_failed(false);
                self.next_errors = self.next_errors.saturating_add(1);
            } else {
                self.started = Some(id);
            }
        }
        fn submitted(&mut self) {
            if let Some(id) = self.started.take() {
                // This same ID was obtained before the successful SDK swap. Query
                // ONLY requested time: other fields may still be PENDING. Pairs
                // are adjacent closed numeric counters inside nativeRender.
                let mut requested = -2;
                if unsafe {
                    (self.get)(
                        self.display,
                        self.surface,
                        id,
                        1,
                        FIELDS.as_ptr(),
                        &mut requested,
                    )
                } == 0
                {
                    self.query_failed(false);
                } else if requested >= 0 {
                    if let Ok(id) = i64::try_from(id) {
                        crate::renderer_trace::counter(c"Chatter Swappy submitted ID", id);
                        crate::renderer_trace::counter(c"Chatter Swappy requested ns", requested);
                    } else {
                        self.fail(Reason::IdentityRange);
                    }
                } else if requested != -2 {
                    self.invalid = self.invalid.saturating_add(1);
                    self.fail(Reason::Invalid);
                }
                if self.sampled && !self.pending.push(id) {
                    self.overflow = self.overflow.saturating_add(1);
                    self.fail(Reason::Overflow);
                }
            }
        }
        fn trace(&self) {
            for (name, value) in [
                (c"Chatter Swappy timestamp valid", self.valid),
                (c"Chatter Swappy timestamp invalid", self.invalid),
                (c"Chatter Swappy timestamp errors", self.errors),
                (c"Chatter Swappy timestamp overflow", self.overflow),
                (c"Chatter Swappy timestamp expired", self.expired),
                (
                    c"Chatter Swappy timestamp collection disabled",
                    self.disabled,
                ),
                (c"Chatter Swappy timestamp unsupported", self.unsupported),
                (
                    c"Chatter Swappy timestamp context errors",
                    self.context_errors,
                ),
                (c"Chatter Swappy timestamp unexplained", self.unexplained),
                (c"Chatter Swappy timestamp next ID errors", self.next_errors),
                (c"Chatter Swappy timestamp interrupted", self.interrupted),
                (c"Chatter Swappy EGL reason", self.egl_reason as i64),
                (c"Chatter Swappy failure reason", self.failure_reason as i64),
                (c"Chatter Swappy timestamp pending", self.pending.len as i64),
                (c"Chatter Swappy observation usable", i64::from(self.usable)),
            ] {
                crate::renderer_trace::counter(name, value);
            }
        }
    }
    impl Drop for Probe {
        fn drop(&mut self) {
            // Borrowed handles remain alive: the owner is dropped before glutin.
            unsafe {
                if (self.attrib)(self.display, self.surface, 0x3430, 0) == 0 {
                    self.query_failed(false);
                    self.trace();
                }
            }
        }
    }
    fn handles(
        display: &glutin::display::Display,
        surface: &Surface<WindowSurface>,
    ) -> Option<(Handle, Handle)> {
        #[allow(irrefutable_let_patterns)]
        if let (RawDisplay::Egl(d), RawSurface::Egl(s)) =
            (display.raw_display(), surface.raw_surface())
        {
            Some((d.cast_mut(), s.cast_mut()))
        } else {
            None
        }
    }

    fn status(value: i64) {
        crate::renderer_trace::counter(c"Chatter Swappy status", value);
    }

    fn stage(value: i64) {
        crate::renderer_trace::counter(c"Chatter Swappy stage", value);
    }

    /// Borrows the owning surface's native window. !Send/!Sync, like glutin.
    pub(crate) struct Swappy {
        api: &'static Api,
        mode: Mode,
        auto: bool,
        probe: std::cell::RefCell<Option<Probe>>,
        renders: std::cell::Cell<u64>,
        generation: i64,
        _activity: std::rc::Rc<ActivityContext>,
        _lease: Lease,
        _thread: std::marker::PhantomData<std::rc::Rc<()>>,
    }
    impl Swappy {
        pub(crate) fn new(
            window: raw_window_handle::RawWindowHandle,
            display: &glutin::display::Display,
            surface: &Surface<WindowSurface>,
        ) -> Option<Self> {
            let mut property = [0u8; 92]; // Android PROP_VALUE_MAX
            let length = unsafe {
                __system_property_get(
                    c"debug.chatter.swappy".as_ptr(),
                    property.as_mut_ptr().cast(),
                )
            };
            let property = &property[..(length.max(0) as usize).min(property.len())];
            let Some(mode) = Mode::parse(property) else {
                return None; // No dlopen, Activity lookup, JavaVM or JNI attachment.
            };
            status(0);
            stage(0); // property opt-in accepted; next: handle/lease checks
            let raw_window_handle::RawWindowHandle::AndroidNdk(window) = window else {
                status(2);
                return None;
            };
            let Some(lease) = Lease::new() else {
                status(3);
                return None;
            };
            stage(1); // about to acquire backend Activity/VM
            let Some(context) = ACTIVITY.with(|slot| slot.borrow().upgrade()) else {
                status(13); // backend did not register an Activity on this thread
                return None;
            };
            if context.vm.is_null() || context.activity.is_null() {
                status(5);
                return None;
            }
            stage(2); // backend owner retained, pointers checked
            // AttachGuard detaches only threads attached by this call.
            let Ok(vm) = (unsafe { jni::JavaVM::from_raw(context.vm.cast()) }) else {
                status(5);
                return None;
            };
            stage(3); // JavaVM wrapper constructed; next is attach
            let Ok(mut env) = vm.attach_current_thread() else {
                status(5);
                return None;
            };
            stage(4); // JNI attached; next is pending-exception check
            if env.exception_check().unwrap_or(true) {
                status(5);
                return None;
            }
            // Borrow AndroidApp's global reference; context retains its owner.
            let activity = unsafe { jni::objects::JObject::from_raw(context.activity.cast()) };
            stage(5); // next: bounded Activity type/debuggable probe
            let debuggable: jni::errors::Result<Option<(bool, i32)>> =
                env.with_local_frame(8, |env| {
                    let activity_class = env.find_class("android/app/Activity")?;
                    if !env.is_instance_of(&activity, activity_class)? {
                        return Ok(None);
                    }
                    stage(6); // confirmed actual Activity
                    let info = env
                        .call_method(
                            &activity,
                            "getApplicationInfo",
                            "()Landroid/content/pm/ApplicationInfo;",
                            &[],
                        )?
                        .l()?;
                    let flags = env.get_field(&info, "flags", "I")?.i()?;
                    let api = env
                        .get_static_field("android/os/Build$VERSION", "SDK_INT", "I")?
                        .i()?;
                    Ok(Some((application_is_debuggable(flags), api)))
                });
            let (debuggable, api_level) = match debuggable {
                Ok(Some(value)) => value,
                Ok(None) => {
                    status(14); // registered object is not an Activity
                    return None;
                }
                Err(_) => {
                    // No exception was pending before our probe; only clear an
                    // exception introduced by it, without content/error logging.
                    let _ = env.exception_clear();
                    status(12);
                    return None;
                }
            };
            stage(7); // Activity/ApplicationInfo probe completed
            if !requested(debuggable, api_level, property) {
                status(11);
                return None;
            }
            // Only a debuggable Android application may load/initialize Swappy.
            // This deliberately does not depend on Rust debug_assertions.
            let mut auto_property = [0u8; 92];
            let auto_length = unsafe {
                __system_property_get(
                    c"debug.chatter.swappy_auto".as_ptr(),
                    auto_property.as_mut_ptr().cast(),
                )
            };
            let Some(auto) = auto_requested(
                &auto_property[..(auto_length.max(0) as usize).min(auto_property.len())],
            ) else {
                status(19); // invalid diagnostic policy: ordinary EGL
                return None;
            };
            stage(8); // next: dlopen/symbol resolution
            let Some(api) = Api::load() else {
                status(4);
                return None;
            };
            stage(9); // next: SwappyGL_init
            let initialized = unsafe { (api.init)(env.get_native_interface(), activity.as_raw()) };
            stage(10); // SwappyGL_init returned
            // Once init has been attempted, always destroy even partial/disabled
            // state on fallback; retain the lease throughout cleanup.
            let owner = Self {
                api,
                mode,
                auto,
                probe: std::cell::RefCell::new(None),
                renders: std::cell::Cell::new(0),
                generation: {
                    static NEXT: std::sync::atomic::AtomicI64 =
                        std::sync::atomic::AtomicI64::new(1);
                    NEXT.fetch_add(1, Ordering::Relaxed)
                },
                _activity: context,
                _lease: lease,
                _thread: std::marker::PhantomData,
            };
            if env.exception_check().unwrap_or(true) {
                let _ = env.exception_clear();
                status(6);
                return None;
            }
            owner.finish_init(initialized, window.a_native_window.as_ptr(), || {
                Probe::new(display, surface)
            })
        }

        fn finish_init(
            self,
            initialized: bool,
            window: Handle,
            make_probe: impl FnOnce() -> Option<Probe>,
        ) -> Option<Self> {
            let owner = self;
            let api = owner.api;
            stage(11); // init exception check complete; next: isEnabled
            if !initialized || !unsafe { (api.enabled)() } {
                status(6);
                return None;
            }
            stage(12); // next: setWindow
            if !unsafe { (api.window)(window) } {
                status(7);
                return None;
            }
            stage(13); // next: fixed experiment configuration
            let Some(probe) = make_probe() else {
                status(17); // timestamp API/fields/collection unavailable; SDK owner cleans up
                return None;
            };
            *owner.probe.borrow_mut() = Some(probe);
            if !api.configure(owner.mode, owner.auto) {
                status(16); // public getters did not confirm one nominal-60 refresh
                return None;
            }
            stage(14); // initialization complete
            status(1);
            Some(owner)
        }

        /// Called immediately after making the context current, before resize,
        /// canvas acquisition, scene CPU work, GPU queries, or render callbacks.
        pub(crate) fn record_frame_start(&self) {
            let mut probe = self.probe.borrow_mut();
            let Some(probe) = probe.as_mut() else {
                return;
            };
            // Establish this render's generation before any delayed sample pair.
            crate::renderer_trace::counter(c"Chatter Swappy generation", self.generation);
            crate::renderer_trace::counter(c"Chatter Swappy mode", self.mode.trace());
            probe.poll();
            if !probe.usable {
                // Never interpret or continue recovery from invalid observations.
                // Retain SDK/EGL ownership; do not switch swap policies mid-frame.
                unsafe {
                    (self.api.fix_wait)(0);
                }
                status(18);
            }
            unsafe {
                (self.api.record)(probe.display, probe.surface);
            }
            probe.start();
            if !probe.usable {
                unsafe {
                    (self.api.fix_wait)(0);
                }
                status(18);
            }
            let renders = self.renders.get().saturating_add(1);
            self.renders.set(renders);
            crate::renderer_trace::counter(
                c"Chatter Swappy capture calls",
                renders.min(i64::MAX as u64) as i64,
            );
            if !self.api.verify_timing() {
                probe.usable = false;
                unsafe {
                    (self.api.fix_wait)(0);
                }
                status(16);
            }
            probe.trace();
            // Explicit closed labels; no bucket number or handle formatted at runtime.
            if renders == 1 || renders % 30 == 0 {
                self.trace_stats();
            }
        }

        fn trace_stats(&self) {
            let mut stats = Stats::default(); // public getter returns void; zero if unsupported
            unsafe {
                (self.api.get_stats)(&mut stats);
            }
            for (name, value) in [
                (c"Chatter Swappy SDK samples", stats.total),
                (c"Chatter Swappy SDK latency 0", stats.latency[0]),
                (c"Chatter Swappy SDK latency 1", stats.latency[1]),
                (c"Chatter Swappy SDK latency 2", stats.latency[2]),
                (c"Chatter Swappy SDK latency 3", stats.latency[3]),
                (c"Chatter Swappy SDK latency 4", stats.latency[4]),
                (c"Chatter Swappy SDK latency 5", stats.latency[5]),
            ] {
                crate::renderer_trace::counter(name, value.min(i64::MAX as u64) as i64);
            }
        }

        pub(crate) fn swap(
            &self,
            display: &glutin::display::Display,
            surface: &Surface<WindowSurface>,
        ) -> Result<(), &'static str> {
            #[allow(irrefutable_let_patterns)]
            let (RawDisplay::Egl(d), RawSurface::Egl(s)) =
                (display.raw_display(), surface.raw_surface())
            else {
                status(8);
                return Err("Swappy requires EGL");
            };
            self.swap_egl(d.cast_mut(), s.cast_mut())
        }

        fn swap_egl(&self, d: Handle, s: Handle) -> Result<(), &'static str> {
            if unsafe { (self.api.swap)(d, s) } {
                if let Some(probe) = self.probe.borrow_mut().as_mut() {
                    probe.submitted();
                    probe.trace();
                    if !probe.usable {
                        unsafe {
                            (self.api.fix_wait)(0);
                        }
                        status(18);
                    }
                }
                Ok(())
            } else {
                // EGL may already have submitted this frame: NEVER retry it with
                // glutin. Keep ownership until destruction, report render error.
                if let Some(probe) = self.probe.borrow_mut().as_mut() {
                    probe.started = None;
                    probe.fail(Reason::SwapFailed);
                    probe.trace();
                    unsafe {
                        (self.api.fix_wait)(0);
                    }
                }
                status(9);
                Err("Swappy swap failed")
            }
        }
    }
    impl Drop for Swappy {
        fn drop(&mut self) {
            stage(15); // next: SDK teardown, Activity owner still retained
            self.trace_stats();
            unsafe { (self.api.destroy)() };
            drop(self.probe.get_mut().take());
            stage(16); // SDK teardown returned
            status(10);
            crate::renderer_trace::counter(c"Chatter Swappy enabled", 0);
            // _lease releases only after SDK teardown; EGL/window still live.
        }
    }
}
#[cfg(target_os = "android")]
pub(crate) use android::Swappy;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn activity_registration_owns_lifetime_and_does_not_clear_replacement() {
        use std::rc::Rc;
        let owner = Rc::new(());
        // Null pointers are never used by this host lifetime-only test.
        let first = unsafe {
            AndroidSwappyActivity::new(std::ptr::null_mut(), std::ptr::null_mut(), owner.clone())
        };
        let retained = ACTIVITY.with(|slot| slot.borrow().upgrade()).unwrap();
        assert_eq!(Rc::strong_count(&owner), 2);
        let replacement =
            unsafe { AndroidSwappyActivity::new(std::ptr::null_mut(), std::ptr::null_mut(), ()) };
        drop(first);
        assert!(ACTIVITY.with(|slot| slot.borrow().upgrade()).is_some());
        assert_eq!(Rc::strong_count(&owner), 2);
        drop(retained);
        assert_eq!(Rc::strong_count(&owner), 1);
        drop(replacement);
        assert!(ACTIVITY.with(|slot| slot.borrow().upgrade()).is_none());
    }
    #[test]
    fn exact_modes_actual_flag_and_api_required() {
        for value in [
            b"".as_slice(),
            b"0",
            b"off",
            b"1",
            b"true",
            b"fix00",
            b"fix8 ",
            b"fix8\0",
        ] {
            assert!(!requested(true, 29, value));
        }
        for property in [b"fix0".as_slice(), b"fix8"] {
            assert!(requested(true, 29, property));
            assert!(requested(true, 36, property));
            assert!(!requested(true, 28, property));
            assert!(!requested(false, 36, property));
            for flags in [0, 1, 4, 0x1000] {
                assert!(!requested(application_is_debuggable(flags), 29, property));
            }
            for flags in [2, 3, 6, 0x1002] {
                assert!(requested(application_is_debuggable(flags), 29, property));
            }
        }
        assert_eq!(Mode::parse(b"fix0").unwrap().wait(), 0);
        assert_eq!(Mode::parse(b"fix8").unwrap().wait(), 8);
    }
    #[test]
    fn public_stats_layout_is_exact() {
        assert_eq!(std::mem::size_of::<Stats>(), 200);
        assert_eq!(std::mem::align_of::<Stats>(), 8);
        assert_eq!(std::mem::offset_of!(Stats, total), 0);
        assert_eq!(std::mem::offset_of!(Stats, idle), 8);
        assert_eq!(std::mem::offset_of!(Stats, late), 56);
        assert_eq!(std::mem::offset_of!(Stats, offset), 104);
        assert_eq!(std::mem::offset_of!(Stats, latency), 152);
    }
    #[test]
    fn pending_and_invalid_are_not_valid_observations() {
        assert_eq!(sample([0, 3, 4, 5]), Sample::Valid);
        assert_eq!(sample([1, 3, 4, -2]), Sample::Pending);
        assert_eq!(sample([1, -1, 4, -2]), Sample::Invalid);
        assert_eq!(sample([1, 0, 4, 5]), Sample::Invalid);
        assert_eq!(sample([1, 3, 4, -1]), Sample::Invalid);
        assert_eq!(sample([1, 3, 4, 0]), Sample::Invalid);
    }
    #[test]
    fn fixed_queue_preserves_fifo_wraps_and_bounds() {
        let mut queue = Pending::default();
        for id in 0..11 {
            assert!(queue.push(id));
        }
        assert!(!queue.push(11));
        for id in 0..5 {
            assert_eq!(queue.front(), Some(id));
            queue.pop();
        }
        for id in 11..16 {
            assert!(queue.push(id));
        }
        for id in (5..11).chain(11..16) {
            assert_eq!(queue.front(), Some(id));
            queue.pop();
        }
        assert_eq!(queue.front(), None);
        queue.pop();
    }
    #[test]
    fn singleton_lease_excludes_second_owner_and_releases_on_drop() {
        let owner = Lease::new().unwrap();
        assert!(Lease::new().is_none());
        assert!(Lease::new().is_none()); // failed acquisition must not drop a temporary lease
        assert!(OWNED.load(Ordering::Acquire));
        drop(owner);
        let replacement = Lease::new().unwrap();
        assert!(Lease::new().is_none());
        drop(replacement);
    }
}

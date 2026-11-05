//! Content-free native Android performance probes. Labels must be fixed C literals.
//! Async cookies are process-local monotonic job serials, never content IDs.
use std::{
    ffi::CStr,
    marker::PhantomData,
    rc::Rc,
    sync::atomic::{AtomicI32, Ordering},
};

static NEXT_JOB: AtomicI32 = AtomicI32::new(1);
thread_local! {
    static CURRENT: std::cell::Cell<Job> = const { std::cell::Cell::new(Job(0)) };
}

/// Copyable correlation token; retains no media, identifiers, or owner state.
#[derive(Clone, Copy, Default)]
pub struct Job(i32);

impl Job {
    pub fn new() -> Self {
        if !native::async_enabled() {
            return Self::default();
        }
        // Stop issuing cookies at exhaustion instead of reusing a live cookie.
        Self(
            NEXT_JOB
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
                .unwrap_or(0),
        )
    }

    /// Safe across await, task migration, queue ownership and callback drops.
    pub fn span(self, label: &'static CStr) -> AsyncSection {
        let active = self.0 != 0 && native::async_enabled();
        if active {
            native::begin_async(label, self.0);
        }
        AsyncSection {
            label,
            cookie: if active { self.0 } else { 0 },
        }
    }

    /// A paired outcome marker, including cancellation paths.
    pub fn event(self, label: &'static CStr) {
        if self.0 == 0 {
            drop(section(label));
        } else {
            drop(self.span(label));
        }
    }

    /// Install only for synchronous work; the guard cannot move across threads.
    pub fn enter(self) -> CurrentJob {
        CurrentJob {
            previous: (self.0 != 0).then(|| CURRENT.with(|current| current.replace(self))),
            _thread: PhantomData,
        }
    }
}

pub fn current_job() -> Job {
    CURRENT.with(std::cell::Cell::get)
}

pub struct CurrentJob {
    previous: Option<Job>,
    _thread: PhantomData<Rc<()>>,
}
impl Drop for CurrentJob {
    fn drop(&mut self) {
        if let Some(previous) = self.previous {
            CURRENT.with(|current| current.set(previous));
        }
    }
}

/// Restore thread-local correlation after *each poll*, including Pending.
/// No thread-bound context or synchronous ATrace section spans an await.
pub async fn scope<F: std::future::Future>(job: Job, future: F) -> F::Output {
    if job.0 == 0 {
        return future.await;
    }
    let mut future = std::pin::pin!(future);
    std::future::poll_fn(|cx| {
        let _current = job.enter();
        future.as_mut().poll(cx)
    })
    .await
}

pub struct AsyncSection {
    label: &'static CStr,
    cookie: i32,
}
impl Drop for AsyncSection {
    fn drop(&mut self) {
        if self.cookie != 0 {
            native::end_async(self.label, self.cookie);
        }
    }
}

/// Thread-bound by construction: never carry this guard across an await.
#[must_use]
pub struct Section {
    active: bool,
    _thread: PhantomData<Rc<()>>,
}
pub fn section(label: &'static CStr) -> Section {
    let active = native::enabled();
    if active {
        native::begin(label);
    }
    Section {
        active,
        _thread: PhantomData,
    }
}
impl Drop for Section {
    fn drop(&mut self) {
        if self.active {
            native::end();
        }
    }
}

pub fn counter(label: &'static CStr, value: i64) {
    if native::enabled() {
        native::counter(label, value);
    }
}

/// Keep any instrumentation-only scans or sums behind this check.
pub fn enabled() -> bool {
    native::enabled()
}

// The real ATrace harness compiles only under the `perf-trace` feature (default
// off). A normal build — including every shipped Android APK — compiles the no-op
// `native` stub below, so `enabled()`/`async_enabled()` are always false and every
// `span`/`section`/`counter` call site folds away at compile time. Flip it back on
// with `--features chatter-ui/perf-trace` (or `TRACE=1 ./build-android.sh`) when a
// profiling pass needs the probes again.
#[cfg(all(target_os = "android", feature = "perf-trace"))]
mod native {
    use std::{
        ffi::{CStr, c_char},
        sync::OnceLock,
    };

    #[link(name = "android")]
    unsafe extern "C" {
        fn ATrace_isEnabled() -> bool;
        fn ATrace_beginSection(label: *const c_char);
        fn ATrace_endSection();
    }
    type AsyncFn = unsafe extern "C" fn(*const c_char, i32);
    type CounterFn = unsafe extern "C" fn(*const c_char, i64);
    struct Optional {
        begin: Option<AsyncFn>,
        end: Option<AsyncFn>,
        counter: Option<CounterFn>,
    }
    fn optional() -> &'static Optional {
        static API: OnceLock<Optional> = OnceLock::new();
        API.get_or_init(|| {
            // API 29 symbols must never become hard imports: minSDK stays 26.
            // libandroid is already loaded by the API 23 functions above.
            unsafe {
                let begin = libc::dlsym(libc::RTLD_DEFAULT, c"ATrace_beginAsyncSection".as_ptr());
                let end = libc::dlsym(libc::RTLD_DEFAULT, c"ATrace_endAsyncSection".as_ptr());
                let counter = libc::dlsym(libc::RTLD_DEFAULT, c"ATrace_setCounter".as_ptr());
                Optional {
                    begin: if begin.is_null() {
                        None
                    } else {
                        Some(std::mem::transmute::<*mut libc::c_void, AsyncFn>(begin))
                    },
                    end: if end.is_null() {
                        None
                    } else {
                        Some(std::mem::transmute::<*mut libc::c_void, AsyncFn>(end))
                    },
                    counter: if counter.is_null() {
                        None
                    } else {
                        Some(std::mem::transmute::<*mut libc::c_void, CounterFn>(counter))
                    },
                }
            }
        })
    }
    pub(super) fn enabled() -> bool {
        unsafe { ATrace_isEnabled() }
    }
    pub(super) fn async_enabled() -> bool {
        enabled() && optional().begin.is_some() && optional().end.is_some()
    }
    pub(super) fn begin(label: &CStr) {
        unsafe { ATrace_beginSection(label.as_ptr()) }
    }
    pub(super) fn end() {
        unsafe { ATrace_endSection() }
    }
    pub(super) fn begin_async(label: &CStr, cookie: i32) {
        if let Some(begin) = optional().begin {
            unsafe { begin(label.as_ptr(), cookie) }
        }
    }
    pub(super) fn end_async(label: &CStr, cookie: i32) {
        if let Some(end) = optional().end {
            unsafe { end(label.as_ptr(), cookie) }
        }
    }
    pub(super) fn counter(label: &CStr, value: i64) {
        if let Some(counter) = optional().counter {
            unsafe { counter(label.as_ptr(), value) }
        }
    }
}

#[cfg(not(all(target_os = "android", feature = "perf-trace")))]
mod native {
    use std::ffi::CStr;
    pub(super) fn enabled() -> bool {
        false
    }
    pub(super) fn async_enabled() -> bool {
        false
    }
    pub(super) fn begin(_: &CStr) {}
    pub(super) fn end() {}
    pub(super) fn begin_async(_: &CStr, _: i32) {}
    pub(super) fn end_async(_: &CStr, _: i32) {}
    pub(super) fn counter(_: &CStr, _: i64) {}
}

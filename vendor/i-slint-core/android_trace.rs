// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

//! Private, allocation-free Android tracing for repeater lifecycle diagnostics.
//! Labels are static and contain no model data. Counters advance only while
//! tracing is enabled; use deltas because counts persist across trace sessions
//! and omit disabled intervals. No synchronous section may cross an await.

use core::ffi::CStr;

/// An anonymous repeater serial, assigned only when tracing is enabled. No
/// pointer, model identifier, or row data participates in its value.
#[derive(Clone, Default)]
pub(crate) struct Identity {
    #[cfg(all(target_os = "android", feature = "std"))]
    value: core::cell::Cell<usize>,
}

/// An observational cumulative count, shared by all repeaters in this process.
pub(crate) struct Counter {
    #[cfg(all(target_os = "android", feature = "std"))]
    name: &'static CStr,
    #[cfg(all(target_os = "android", feature = "std"))]
    value: core::sync::atomic::AtomicI64,
}

impl Counter {
    pub(crate) const fn new(_name: &'static CStr) -> Self {
        Self {
            #[cfg(all(target_os = "android", feature = "std"))]
            name: _name,
            #[cfg(all(target_os = "android", feature = "std"))]
            value: core::sync::atomic::AtomicI64::new(0),
        }
    }
}

/// A thread-bound synchronous section. Static C strings prevent allocations
/// and accidental interpolation of message text, identifiers, or file paths.
#[must_use]
pub(crate) struct Section {
    enabled: bool,
    // Rc is !Send + !Sync even when this module is a no-op on other targets.
    _thread_bound: core::marker::PhantomData<alloc::rc::Rc<()>>,
}

impl Section {
    #[inline]
    pub(crate) fn new(_name: &'static CStr) -> Self {
        #[cfg(all(target_os = "android", feature = "std"))]
        let enabled = android::begin(_name);
        #[cfg(not(all(target_os = "android", feature = "std")))]
        let enabled = false;
        Self {
            enabled,
            _thread_bound: core::marker::PhantomData,
        }
    }

    #[inline]
    pub(crate) fn enabled(&self) -> bool {
        self.enabled
    }

    #[inline]
    pub(crate) fn identify(&self, _identity: &Identity) {
        #[cfg(all(target_os = "android", feature = "std"))]
        if self.enabled {
            use core::sync::atomic::{AtomicUsize, Ordering};
            static NEXT: AtomicUsize = AtomicUsize::new(1);
            let mut value = _identity.value.get();
            if value == 0 {
                value = NEXT.fetch_add(1, Ordering::Relaxed);
                _identity.value.set(value);
            }
            android::counter(c"Slint repeater instance", value as i64);
        }
    }

    #[inline]
    pub(crate) fn counter(&self, _name: &'static CStr, _value: i64) {
        #[cfg(all(target_os = "android", feature = "std"))]
        if self.enabled {
            android::counter(_name, _value);
        }
    }

    #[inline]
    pub(crate) fn count(&self, _counter: &Counter, _delta: usize) {
        #[cfg(all(target_os = "android", feature = "std"))]
        if self.enabled {
            use core::sync::atomic::Ordering;
            let delta = _delta.min(i64::MAX as usize) as i64;
            let value = _counter
                .value
                .fetch_add(delta, Ordering::Relaxed)
                .wrapping_add(delta);
            android::counter(_counter.name, value);
        }
    }
}

impl Drop for Section {
    #[inline]
    fn drop(&mut self) {
        #[cfg(all(target_os = "android", feature = "std"))]
        if self.enabled {
            android::end();
        }
    }
}

#[cfg(all(target_os = "android", feature = "std"))]
mod android {
    #![allow(unsafe_code)]

    use core::ffi::{CStr, c_char, c_void};

    // Cached once by each trace helper crate, not once per section/frame.
    // Exact opt-in only; property changes require a process restart.
    fn compact() -> bool {
        static COMPACT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *COMPACT.get_or_init(|| {
            unsafe extern "C" {
                fn __system_property_get(name: *const c_char, value: *mut c_char) -> i32;
            }
            let mut value = [0u8; 92]; // bionic PROP_VALUE_MAX, including NUL
            // SAFETY: fixed property name and full-size writable buffer.
            let length = unsafe {
                __system_property_get(c"debug.chatter.compact_trace".as_ptr(), value.as_mut_ptr().cast())
            };
            length == 1 && value[0] == b'1'
        })
    }

    // The scoped API is available from Android API 23.
    #[link(name = "android")]
    unsafe extern "C" {
        fn ATrace_isEnabled() -> bool;
        fn ATrace_beginSection(name: *const c_char);
        fn ATrace_endSection();
    }

    #[link(name = "dl")]
    unsafe extern "C" {
        fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    }

    pub(super) fn begin(name: &'static CStr) -> bool {
        // SAFETY: Android API 23 ABI; caller provides a static C string.
        let enabled = !compact() && unsafe { ATrace_isEnabled() };
        if enabled {
            unsafe { ATrace_beginSection(name.as_ptr()) };
        }
        enabled
    }

    pub(super) fn end() {
        // SAFETY: Section is !Send and balances each enabled begin on this thread.
        unsafe { ATrace_endSection() };
    }

    pub(super) fn counter(name: &'static CStr, value: i64) {
        type SetCounter = unsafe extern "C" fn(*const c_char, i64);
        static SET_COUNTER: std::sync::OnceLock<Option<SetCounter>> = std::sync::OnceLock::new();
        // Called only from an enabled section. Resolve the optional API 29
        // symbol once so API 23-28 never acquire a loader dependency on it.
        let set_counter = SET_COUNTER.get_or_init(|| {
            #[cfg(target_pointer_width = "64")]
            let default_handle = core::ptr::null_mut();
            #[cfg(target_pointer_width = "32")]
            let default_handle = 0xffff_ffffusize as *mut c_void;
            // SAFETY: bionic RTLD_DEFAULT differs on LP32/LP64; static symbol.
            let symbol = unsafe { dlsym(default_handle, c"ATrace_setCounter".as_ptr()) };
            if symbol.is_null() {
                None
            } else {
                // SAFETY: exact NDK ABI, libandroid remains linked for life.
                Some(unsafe { core::mem::transmute::<*mut c_void, SetCounter>(symbol) })
            }
        });
        if let Some(set_counter) = set_counter {
            // SAFETY: resolved ABI and a static NUL-terminated label.
            unsafe { set_counter(name.as_ptr(), value) };
        }
    }
}

// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

//! Android-only, allocation-free CPU-side renderer scopes (API 23+, minSdk 26).
//! Fixed labels only; these intervals do not measure GPU completion.

#[link(name = "android")]
unsafe extern "C" {
    fn ATrace_isEnabled() -> bool;
    fn ATrace_beginSection(name: *const std::ffi::c_char);
    fn ATrace_endSection();
}

/// Cached once by this helper crate, independent of trace sessions. Exact "1"
/// opts in; changing the property requires restarting the process.
pub(crate) fn compact() -> bool {
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

#[must_use]
pub(crate) struct Section {
    enabled: bool,
    // ATrace sections must end on the thread where they began; no await/migration.
    _thread_bound: core::marker::PhantomData<std::rc::Rc<()>>,
}

impl Section {
    pub(crate) fn new(name: &'static std::ffi::CStr) -> Self {
        let enabled = !compact() && unsafe { ATrace_isEnabled() };
        if enabled {
            unsafe { ATrace_beginSection(name.as_ptr()) };
        }
        Self {
            enabled,
            _thread_bound: core::marker::PhantomData,
        }
    }
}

impl Drop for Section {
    fn drop(&mut self) {
        if self.enabled {
            unsafe { ATrace_endSection() };
        }
    }
}

// ATrace counters were added in API 29. Resolve dynamically for minSdk 26;
// never introduce a hard loader dependency on ATrace_setCounter.
type SetCounter = unsafe extern "C" fn(*const std::ffi::c_char, i64);

fn set_counter() -> Option<SetCounter> {
    use std::ffi::c_void;
    #[link(name = "dl")]
    unsafe extern "C" {
        fn dlsym(handle: *mut c_void, symbol: *const std::ffi::c_char) -> *mut c_void;
    }
    static COUNTER: std::sync::OnceLock<Option<SetCounter>> = std::sync::OnceLock::new();
    *COUNTER.get_or_init(|| {
        #[cfg(target_pointer_width = "64")]
        let default_handle = core::ptr::null_mut();
        #[cfg(target_pointer_width = "32")]
        let default_handle = 0xffff_ffffusize as *mut c_void;
        // SAFETY: bionic RTLD_DEFAULT ABI; static NUL-terminated name.
        let p = unsafe { dlsym(default_handle, c"ATrace_setCounter".as_ptr()) };
        if p.is_null() {
            None
        } else {
            // SAFETY: exact NDK signature; linked libandroid lives for the process.
            Some(unsafe { core::mem::transmute::<*mut c_void, SetCounter>(p) })
        }
    })
}

pub(crate) fn enabled() -> bool {
    !compact() && observation_enabled()
}

/// Numeric observation availability, independent of verbose scope policy.
/// EGL collection and the prefix experiment must use this gate.
pub(crate) fn observation_enabled() -> bool {
    (unsafe { ATrace_isEnabled() }) && set_counter().is_some()
}

fn compact_counter(name: &std::ffi::CStr) -> bool {
    #[cfg(feature = "painted-scroll-probe")]
    if i_slint_core::painted_scroll_probe::LABELS.contains(&name)
        || i_slint_core::painted_scroll_probe::AVAILABILITY_LABELS.contains(&name) { return true; }
    #[cfg(feature = "experimental-swappy")]
    if matches!(name.to_bytes(),
        b"Chatter Swappy generation" | b"Chatter Swappy mode" | b"Chatter Swappy interval ns"
        | b"Chatter Swappy auto requested"
        | b"Chatter Swappy refresh ns"
        | b"Chatter Swappy submitted ID" | b"Chatter Swappy requested ns"
        | b"Chatter Swappy sampled ID" | b"Chatter Swappy actual present ns"
        | b"Chatter Swappy timestamp expired" | b"Chatter Swappy timestamp collection disabled"
        | b"Chatter Swappy timestamp unsupported" | b"Chatter Swappy timestamp context errors"
        | b"Chatter Swappy timestamp unexplained" | b"Chatter Swappy timestamp next ID errors"
        | b"Chatter Swappy timestamp interrupted" | b"Chatter Swappy EGL reason"
        | b"Chatter Swappy failure reason"
        | b"Chatter Swappy capture calls" | b"Chatter Swappy app observer suppressed"
        | b"Chatter Swappy timestamp support mask" | b"Chatter Swappy timestamp valid"
        | b"Chatter Swappy timestamp invalid" | b"Chatter Swappy timestamp errors"
        | b"Chatter Swappy timestamp overflow" | b"Chatter Swappy timestamp pending"
        | b"Chatter Swappy observation usable" | b"Chatter Swappy SDK samples"
        | b"Chatter Swappy SDK latency 0" | b"Chatter Swappy SDK latency 1"
        | b"Chatter Swappy SDK latency 2" | b"Chatter Swappy SDK latency 3"
        | b"Chatter Swappy SDK latency 4" | b"Chatter Swappy SDK latency 5") {
        return true;
    }
    #[cfg(feature = "experimental-native-pacing")]
    if name.to_bytes() == b"Chatter EGL observer selected" {
        return true;
    }
    matches!(name.to_bytes(),
        b"Chatter EGL generation"
        | b"Chatter EGL submitted frame id"
        | b"Chatter EGL timestamp frame id"
        | b"Chatter EGL timestamp API available"
        | b"Chatter EGL timestamp support mask"
        | b"Chatter EGL timestamp collection enabled"
        | b"Chatter EGL requested or queue ns"
        | b"Chatter EGL rendering complete ns"
        | b"Chatter EGL latch ns"
        | b"Chatter EGL composition start ns"
        | b"Chatter EGL display present ns"
        | b"Chatter EGL dequeue ready ns"
        | b"Chatter EGL reads done ns"
        | b"Chatter Skia prefix flush mode"
        | b"Chatter Skia prefix flush us"
        | b"Chatter Swappy enabled"
        | b"Chatter Swappy status")
}

pub(crate) fn counter(name: &'static std::ffi::CStr, value: i64) {
    if compact() && !compact_counter(name) {
        return;
    }
    if unsafe { ATrace_isEnabled() } {
        if let Some(counter) = set_counter() {
            // SAFETY: checked optional API and static C string.
            unsafe { counter(name.as_ptr(), value) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::compact_counter;

    #[cfg(feature = "painted-scroll-probe")]
    #[test]
    fn painted_compact_labels_are_exact_and_closed() {
        for label in i_slint_core::painted_scroll_probe::LABELS.into_iter()
            .chain(i_slint_core::painted_scroll_probe::AVAILABILITY_LABELS) {
            assert!(compact_counter(label));
            let mut extra = label.to_bytes().to_vec(); extra.extend_from_slice(b" extra");
            assert!(!compact_counter(&std::ffi::CString::new(extra).unwrap()));
        }
        assert!(!compact_counter(c"Chatter painted"));
        assert!(!compact_counter(c"Chatter painted unknown"));
    }

    #[test]
    fn compact_counter_labels_are_exact_and_closed() {
        assert_eq!(compact_counter(c"Chatter Swappy auto requested"), cfg!(feature = "experimental-swappy"));
        assert!(!compact_counter(c"Chatter Swappy auto requested extra"));
        assert_eq!(compact_counter(c"Chatter EGL observer selected"), cfg!(feature = "experimental-native-pacing"));
        assert!(!compact_counter(c"Chatter EGL observer selected extra"));
        for label in [
            c"Chatter EGL generation", c"Chatter EGL submitted frame id",
            c"Chatter EGL timestamp frame id", c"Chatter EGL timestamp API available",
            c"Chatter EGL timestamp support mask", c"Chatter EGL timestamp collection enabled",
            c"Chatter EGL requested or queue ns", c"Chatter EGL rendering complete ns",
            c"Chatter EGL latch ns", c"Chatter EGL composition start ns",
            c"Chatter EGL display present ns", c"Chatter EGL dequeue ready ns",
            c"Chatter EGL reads done ns", c"Chatter Skia prefix flush mode",
            c"Chatter Skia prefix flush us", c"Chatter Swappy enabled", c"Chatter Swappy status",
        ] {
            assert!(compact_counter(label), "{label:?}");
        }
        for label in [
            c"Chatter image current device clip bbox px2", c"Chatter Swappy stage",
            c"Chatter EGL dequeue ready ns extra", c"Chatter EGL reads done ns extra",
            c"Chatter EGL generation extra", c"Chatter EGL GPU elapsed ns", c"",
        ] {
            assert!(!compact_counter(label), "{label:?}");
        }
    }
}

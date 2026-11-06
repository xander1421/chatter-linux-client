// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

//! Matched same-APK diagnostic, not a buffer-acquisition API or shipping fix.
//! No extra drawing, GPU completion wait, input dispatch, submission or swap.

fn permitted(
    debuggable: bool,
    property: &[u8],
    numeric_trace: bool,
    notifier: bool,
    background: bool,
    context: bool,
) -> bool {
    debuggable && property == b"1" && numeric_trace && !notifier && background && context
}

#[cfg(target_os = "android")]
pub(crate) fn flush_existing_background(
    debuggable: bool,
    notifier: bool,
    background: bool,
    context: Option<&mut skia_safe::gpu::DirectContext>,
) {
    use crate::renderer_trace;
    // Fail closed if API 29 numeric counters or tracing are unavailable.
    if !renderer_trace::observation_enabled() {
        return;
    }
    let mut property = [0u8; 92]; // bionic PROP_VALUE_MAX includes terminator
    let length = if debuggable {
        unsafe extern "C" {
            fn __system_property_get(
                name: *const std::ffi::c_char,
                value: *mut std::ffi::c_char,
            ) -> i32;
        }
        // SAFETY: fixed name, full documented writable property buffer.
        unsafe {
            __system_property_get(
                c"debug.chatter.prefix_flush".as_ptr(),
                property.as_mut_ptr().cast(),
            )
        }
    } else {
        0
    };
    let property = &property[..usize::try_from(length).unwrap_or(0).min(property.len())];
    let enabled = permitted(
        debuggable,
        property,
        true,
        notifier,
        background,
        context.is_some(),
    );
    renderer_trace::counter(c"Chatter Skia prefix flush mode", i64::from(enabled));
    let duration_us = if enabled {
        let start = std::time::Instant::now();
        {
            let _trace = renderer_trace::Section::new(c"Chatter Skia prefix flush");
            if let Some(context) = context {
                // Same safe operation as the existing notifier/final flush.
                context.flush(None);
            }
        }
        start.elapsed().as_micros().min(i64::MAX as u128) as i64
    } else {
        0
    };
    // CPU-side duration only: neither GPU completion nor native buffer ownership.
    renderer_trace::counter(c"Chatter Skia prefix flush us", duration_us);
}

#[cfg(test)]
mod tests {
    use super::permitted;

    #[test]
    fn exact_property_required() {
        assert!(permitted(true, b"1", true, false, true, true));
        for property in [
            b"".as_slice(),
            b"0",
            b"01",
            b"true",
            b"1\n",
            b"1 ",
            b"11",
            b"\x001",
        ] {
            assert!(!permitted(true, property, true, false, true, true));
        }
    }

    #[test]
    fn every_runtime_gate_fails_closed() {
        // Each missing prerequisite independently suppresses the extra flush.
        assert!(!permitted(false, b"1", true, false, true, true));
        assert!(!permitted(true, b"1", false, false, true, true));
        assert!(!permitted(true, b"1", true, true, true, true));
        assert!(!permitted(true, b"1", true, false, false, true));
        assert!(!permitted(true, b"1", true, false, true, false));
    }
}

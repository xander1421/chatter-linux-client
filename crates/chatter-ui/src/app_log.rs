//! One tracing destination for Activity and Service startup.
//! GameActivity redirects stderr only after its native entry runs, so a
//! Service-first process must write directly to Android's log buffer.

use std::io::{self, Write};

pub(super) fn writer() -> AppLogWriter {
    AppLogWriter {
        #[cfg(target_os = "android")]
        bytes: Vec::new(),
    }
}

pub(super) struct AppLogWriter {
    #[cfg(target_os = "android")]
    bytes: Vec<u8>,
}

impl Write for AppLogWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        #[cfg(target_os = "android")]
        {
            // Bound one event even if a dependency emits a huge debug value.
            const MAX_EVENT: usize = 16 * 1024;
            let remaining = MAX_EVENT.saturating_sub(self.bytes.len());
            self.bytes
                .extend_from_slice(&bytes[..bytes.len().min(remaining)]);
            Ok(bytes.len())
        }
        #[cfg(not(target_os = "android"))]
        {
            io::stderr().write(bytes)
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        #[cfg(target_os = "android")]
        {
            self.write_android();
            Ok(())
        }
        #[cfg(not(target_os = "android"))]
        {
            io::stderr().flush()
        }
    }
}

#[cfg(target_os = "android")]
#[link(name = "log")]
unsafe extern "C" {
    fn __android_log_write(
        priority: std::ffi::c_int,
        tag: *const std::ffi::c_char,
        text: *const std::ffi::c_char,
    ) -> std::ffi::c_int;
}

#[cfg(target_os = "android")]
impl AppLogWriter {
    fn write_android(&mut self) {
        // logcat has a per-entry limit. Split long events and replace NULs so
        // every chunk is a valid C string. Release formatting already removes
        // event fields before this writer receives them.
        for chunk in self.bytes.chunks(3000) {
            let line = String::from_utf8_lossy(chunk).replace('\0', "\u{fffd}");
            if let Ok(text) = std::ffi::CString::new(line.trim_end_matches('\n')) {
                unsafe {
                    __android_log_write(4, c"ChatterRust".as_ptr(), text.as_ptr());
                }
            }
        }
        self.bytes.clear();
    }
}

#[cfg(target_os = "android")]
impl Drop for AppLogWriter {
    fn drop(&mut self) {
        self.write_android();
    }
}

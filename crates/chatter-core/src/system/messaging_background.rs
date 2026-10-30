//! Platform policy hook; the existing core inbox loop owns all messaging and Tor.
use std::sync::{
    OnceLock,
    atomic::{AtomicBool, Ordering},
};
static SIGNED_IN: AtomicBool = AtomicBool::new(false);
// The default false is not an account decision. Activity registration can run
// during a boot prelaunch before the saved session has been restored.
static SIGNED_IN_KNOWN: AtomicBool = AtomicBool::new(false);
static PLATFORM: OnceLock<fn(bool)> = OnceLock::new();

pub fn register_platform(hook: fn(bool)) {
    let _ = PLATFORM.set(hook);
    if SIGNED_IN_KNOWN.load(Ordering::Acquire) {
        hook(SIGNED_IN.load(Ordering::Acquire));
    }
}
pub(crate) fn set_signed_in(signed_in: bool) {
    SIGNED_IN.store(signed_in, Ordering::Release);
    SIGNED_IN_KNOWN.store(true, Ordering::Release);
    if let Some(hook) = PLATFORM.get() {
        hook(signed_in);
    }
}

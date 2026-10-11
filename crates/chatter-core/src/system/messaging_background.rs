//! Platform policy hook; the existing core inbox loop owns all messaging and Tor.
use std::sync::{
    OnceLock,
    atomic::{AtomicBool, Ordering},
};
static SIGNED_IN: AtomicBool = AtomicBool::new(false);
static PLATFORM: OnceLock<fn(bool)> = OnceLock::new();

pub fn register_platform(hook: fn(bool)) {
    let _ = PLATFORM.set(hook);
    hook(SIGNED_IN.load(Ordering::Acquire));
}
pub(crate) fn set_signed_in(signed_in: bool) {
    SIGNED_IN.store(signed_in, Ordering::Release);
    if let Some(hook) = PLATFORM.get() {
        hook(signed_in);
    }
}

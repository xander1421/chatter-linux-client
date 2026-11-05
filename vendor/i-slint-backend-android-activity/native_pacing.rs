// Android frame-pacing policy (single production configuration).
//
// During a manual drag the producer is paced to exactly one frame per
// Choreographer vsync by a permit gate (see `vsync::VsyncDriver`), and the EGL
// swap is `DontWait` while that gate is driving. When the helper is not driving
// (startup, fallback, suspension) the swap is restored to `Wait(1)` so the
// swap fence keeps 60Hz without the gate. This file owns the Wait(1)<->DontWait
// transition state machine; the gate itself lives in the vsync helper.
//
// This was previously feature-gated ("mode 3") behind device-property selection
// alongside unselected alternatives (fixed intervals, a UI-thread frame gate).
// Those alternatives were removed; this is the one configuration that ships.
#[derive(Debug)]
pub(crate) struct Policy {
    surface: bool,
    first: bool,
    driving: bool,
    zero_applied: bool,
    zero_attempted: bool,
}
impl Policy {
    pub fn new() -> Self {
        Self {
            surface: false,
            first: false,
            driving: false,
            zero_applied: false,
            zero_attempted: false,
        }
    }
    pub fn observe(&mut self, driving: bool) {
        self.driving = driving;
    }
    // The permit gate is active whenever a surface exists and the vsync helper
    // is driving frames.
    pub fn gating(&self) -> bool {
        self.surface && self.driving
    }
    pub fn first_frame(&self) -> bool {
        self.first
    }
    pub fn render_started(&mut self) {
        self.first = false;
    }
    pub fn surface_changed(&mut self, present: bool) {
        self.surface = present;
        self.first = present;
        self.zero_applied = false;
        self.zero_attempted = false;
    }
    // DontWait is desired exactly while driving. Returns Some(true) to request a
    // switch to DontWait, Some(false) to restore Wait(1), None if already there.
    pub fn interval_change(&self) -> Option<bool> {
        if !self.surface {
            return None;
        }
        let desired = self.driving;
        if self.zero_applied && !desired {
            Some(false)
        } else if desired && !self.zero_attempted {
            Some(true)
        } else {
            None
        }
    }
    pub fn interval_applied(&mut self, zero: bool, applied: bool) {
        if zero {
            self.zero_attempted = true;
        }
        if applied {
            self.zero_applied = zero;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn startup_requires_surface_and_helper() {
        let mut p = Policy::new();
        p.observe(true);
        assert!(!p.gating());
        assert_eq!(p.interval_change(), None);
        p.surface_changed(true);
        p.observe(false);
        assert!(!p.gating());
        assert!(p.first_frame());
        assert_eq!(p.interval_change(), None);
        p.observe(true);
        assert!(p.gating());
        assert_eq!(p.interval_change(), Some(true));
        p.interval_applied(true, true);
        assert_eq!(p.interval_change(), None);
    }
    #[test]
    fn stale_helper_restores_interval_before_fallback() {
        let mut p = Policy::new();
        p.surface_changed(true);
        p.observe(true);
        p.interval_applied(true, true);
        p.render_started();
        p.observe(false);
        assert!(!p.gating());
        assert_eq!(p.interval_change(), Some(false));
        p.interval_applied(false, true);
        assert_eq!(p.interval_change(), None);
    }
    #[test]
    fn surface_epoch_resets_first_frame_and_interval_state() {
        let mut p = Policy::new();
        p.surface_changed(true);
        p.observe(true);
        p.interval_applied(true, true);
        p.render_started();
        p.surface_changed(false);
        assert!(!p.gating());
        assert!(!p.first_frame());
        assert_eq!(p.interval_change(), None);
        p.surface_changed(true);
        assert!(p.first_frame());
        assert_eq!(p.interval_change(), Some(true));
    }
    // A failed interval API (applied=false) must not latch, so a later attempt retries.
    #[test]
    fn rejected_interval_api_retries() {
        let mut p = Policy::new();
        p.surface_changed(true);
        p.observe(true);
        assert_eq!(p.interval_change(), Some(true));
        p.interval_applied(true, false);
        assert_eq!(p.interval_change(), None);
    }
}

// Numeric trace counters for the pacing gate. An accepted interval API request is
// not proof EGL honored zero: the implementation may clamp intervals.
#[cfg(target_os = "android")]
pub(crate) fn trace(policy: &Policy, watchdog_failed: bool) {
    if !unsafe { crate::ATrace_isEnabled() } { return; }
    type SetCounter = unsafe extern "C" fn(*const std::ffi::c_char, i64);
    static COUNTER: std::sync::OnceLock<Option<SetCounter>> = std::sync::OnceLock::new();
    let counter = COUNTER.get_or_init(|| {
        #[cfg(target_pointer_width = "64")]
        let handle = std::ptr::null_mut();
        #[cfg(target_pointer_width = "32")]
        let handle = 0xffff_ffffusize as *mut std::ffi::c_void;
        // SAFETY: platform RTLD_DEFAULT; static symbol; exact Android counter ABI.
        let symbol = unsafe { crate::dlsym(handle, c"ATrace_setCounter".as_ptr()) };
        if symbol.is_null() { None } else {
            Some(unsafe { std::mem::transmute::<*mut std::ffi::c_void, SetCounter>(symbol) })
        }
    });
    let Some(counter) = counter else { return; };
    let fallback = if watchdog_failed { 2 } else if !policy.driving { 1 } else { 0 };
    // SAFETY: process-lifetime libandroid symbol and static numeric-only names.
    unsafe {
        counter(c"Chatter native pacing gate".as_ptr(), i64::from(policy.gating()));
        counter(c"Chatter native interval0 accepted".as_ptr(), i64::from(policy.zero_applied));
        counter(c"Chatter native pacing fallback".as_ptr(), fallback);
    }
}

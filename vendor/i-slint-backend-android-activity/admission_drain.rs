// Diagnostic ordinary-EGL admission drain. Numeric anonymous state only.
use std::time::Duration;
pub(crate) const DELAY_NS: u64 = 16_666_667;
pub(crate) const ORDINAL: u64 = 12;

#[derive(Default, Debug)]
pub(crate) struct Drain {
    pub observed: bool,
    pub enabled: bool,
    pub gesture: u64,
    pub active: bool,
    pub ordinal: u64,
    pub requests: u64,
    pub fires: u64,
    pub completions: u64,
    pub cancels: u64,
    pub contamination: u64,
    pub surface: u64,
    pub lifecycle: u64,
    pub cancel_reason: u8,
    pub request_ns: u64,
    pub deadline_ns: u64,
    pub completion_ns: u64,
    pending: bool,
    spent: bool,
}
impl Drain {
    pub fn configure(&mut self, supported: bool, native_mode: u8, native: &[u8], value: &[u8]) {
        self.observed = supported && native_mode == 0 && (native.is_empty() || native == b"0");
        self.enabled = self.observed && value == b"1";
        self.trace();
    }
    pub fn down(&mut self) {
        if !self.observed { return; }
        self.cancel(6);
        self.gesture += 1;
        self.active = true;
        self.ordinal = 0;
        self.spent = false;
        self.cancel_reason = 0;
        self.trace();
    }
    // 1 Up, 2 Cancel/Outside, 3 Pause, 4 TermWindow, 5 sync redraw,
    // 6 replacement Down, 7 InitWindow, 8 Destroy.
    pub fn cancel(&mut self, reason: u8) {
        if !self.observed { return; }
        if self.pending { self.cancels += 1; }
        if reason == 5 && self.active { self.contamination += 1; self.spent = true; }
        self.pending = false;
        self.cancel_reason = reason;
        if reason != 5 { self.active = false; }
        if reason == 4 || reason == 7 { self.surface += 1; }
        if matches!(reason, 3 | 4 | 7 | 8) { self.lifecycle += 1; }
        self.trace();
    }
    // Only called after surface, pending work and failure-retry eligibility.
    // A held admission retains ordinal 12; input/timers never call this gate.
    pub fn admit(&mut self, now: u64) -> bool {
        if !self.observed { return true; }
        if self.pending {
            if now < self.deadline_ns { return false; }
            self.pending = false;
            self.completions += 1;
            self.completion_ns = now;
        } else if self.active {
            self.ordinal += 1;
            if self.ordinal == ORDINAL && !self.spent {
                self.spent = true;
                self.requests += 1;
                self.request_ns = now;
                if self.enabled {
                    self.fires += 1;
                    self.deadline_ns = now.saturating_add(DELAY_NS);
                    self.pending = true;
                    self.trace();
                    return false;
                }
            }
        }
        self.trace();
        true
    }
    // Compose independent restrictions with MAX. Timers/events may wake earlier.
    // No drain deadline is supplied while inactive or after consumption/cancel.
    pub fn wait(&self, now: u64, retry: Option<Duration>) -> Duration {
        let drain = if self.pending { Duration::from_nanos(self.deadline_ns.saturating_sub(now)) }
            else { Duration::ZERO };
        retry.unwrap_or_default().max(drain)
    }
    #[cfg(not(target_os = "android"))]
    fn trace(&self) {}
}

#[cfg(target_os = "android")]
pub(crate) fn now_ns() -> u64 {
    let mut ts = crate::AndroidTimespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: CLOCK_MONOTONIC and Android's C-long timespec ABI.
    if unsafe { crate::clock_gettime(1, &mut ts) } != 0 || ts.tv_sec < 0
        || !(0..1_000_000_000).contains(&ts.tv_nsec) { return 0; }
    (ts.tv_sec as u64).saturating_mul(1_000_000_000).saturating_add(ts.tv_nsec as u64)
}
#[cfg(target_os = "android")]
pub(crate) fn configure(drain: &mut Drain, supported: bool, native_mode: u8) {
    // Actual Java FLAG_DEBUGGABLE/API>=29 guard BEFORE property reads.
    if !supported || native_mode != 0 { return; }
    unsafe extern "C" {
        fn __system_property_get(name: *const std::ffi::c_char, value: *mut std::ffi::c_char) -> i32;
    }
    fn property(name: &std::ffi::CStr) -> Vec<u8> {
        let mut buf = [0u8; 92];
        // SAFETY: static key, PROP_VALUE_MAX writable bytes.
        let n = unsafe { __system_property_get(name.as_ptr(), buf.as_mut_ptr().cast()) };
        buf[..usize::try_from(n).unwrap_or(0).min(buf.len())].to_vec()
    }
    drain.configure(now_ns() != 0, native_mode, &property(c"debug.chatter.native_pacing"),
        &property(c"debug.chatter.admission_drain"));
}
#[cfg(target_os = "android")]
impl Drain {
    fn trace(&self) {
        if !self.observed || !unsafe { crate::ATrace_isEnabled() } { return; }
        type Counter = unsafe extern "C" fn(*const std::ffi::c_char, i64);
        static COUNTER: std::sync::OnceLock<Option<Counter>> = std::sync::OnceLock::new();
        let counter = COUNTER.get_or_init(|| {
            #[cfg(target_pointer_width = "64")]
            let handle = std::ptr::null_mut();
            #[cfg(target_pointer_width = "32")]
            let handle = 0xffff_ffffusize as *mut std::ffi::c_void;
            // SAFETY: RTLD_DEFAULT, process-lifetime libandroid, exact ABI.
            let ptr = unsafe { crate::dlsym(handle, c"ATrace_setCounter".as_ptr()) };
            if ptr.is_null() { None } else { Some(unsafe { std::mem::transmute::<*mut std::ffi::c_void, Counter>(ptr) }) }
        });
        let Some(counter) = counter else { return; };
        macro_rules! emit { ($name:literal, $value:expr) => {
            // SAFETY: static names and resolved numeric counter ABI.
            unsafe { counter(concat!("Chatter drain ", $name, "\0").as_ptr().cast(), $value as i64) };
        }; }
        emit!("mode", 0); emit!("enabled", self.enabled);
        emit!("gesture", self.gesture); emit!("active", self.active); emit!("ordinal", self.ordinal);
        emit!("request", self.requests); emit!("fire", self.fires); emit!("completion", self.completions);
        emit!("cancel", self.cancels); emit!("cancel reason", self.cancel_reason);
        emit!("contamination", self.contamination); emit!("pending", self.pending);
        emit!("request ns", self.request_ns); emit!("deadline ns", self.deadline_ns);
        emit!("completion ns", self.completion_ns); emit!("surface epoch", self.surface);
        emit!("lifecycle epoch", self.lifecycle);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn ready(on: bool) -> Drain {
        let mut d = Drain::default(); d.configure(true, 0, b"0", if on { b"1" } else { b"0" }); d.down(); d
    }
    fn arm(d: &mut Drain, now: u64) {
        for _ in 1..ORDINAL { assert!(d.admit(now)); }
        assert!(!d.admit(now));
    }
    #[test] fn one_shot_exact_deadline_not_periodic() {
        let mut d=ready(true); arm(&mut d, 1_000);
        assert!(!d.admit(1_000+DELAY_NS-1)); assert_eq!(d.ordinal,12);
        assert!(d.admit(1_000+DELAY_NS)); assert_eq!(d.completions,1);
        for _ in 0..100 { assert!(d.admit(1_000+DELAY_NS)); }
        assert_eq!((d.requests,d.fires),(1,1));
        d.cancel(1); d.down(); arm(&mut d, 2_000+DELAY_NS); assert_eq!(d.fires,2);
    }
    #[test] fn paired_control_observes_same_ordinal_without_hold() {
        let mut d=ready(false); for _ in 0..30 { assert!(d.admit(100)); }
        assert_eq!((d.requests,d.fires,d.completions),(1,0,0)); assert_eq!(d.wait(0,None),Duration::ZERO);
    }
    #[test] fn exact_opt_in_and_all_pacing_modes_excluded() {
        for mode in [1,2,3,6,7] { let mut d=Drain::default();d.configure(true,mode,b"0",b"1");assert!(!d.enabled); }
        for val in [b"".as_slice(),b"01",b"1 ",b"true",b"1\n",b"0"] { let mut d=Drain::default();d.configure(true,0,b"0",val);assert!(!d.enabled); }
        let mut d=Drain::default();d.configure(false,0,b"0",b"1");assert!(!d.observed);
        d.configure(true,0,b"8",b"1");assert!(!d.observed);
    }
    #[test] fn input_and_timer_wakes_do_not_advance_or_rearm() {
        let mut d=ready(true);arm(&mut d,100);
        for now in [101,500,10_000,1_000_000] {
            // An input/timer wake is allowed, but ordinary work remains pending.
            assert!(d.wait(now,None)>Duration::ZERO);assert!(!d.admit(now));
        }
        assert_eq!((d.ordinal,d.fires,d.requests),(12,1,1));
        d.cancel(1);assert!(d.admit(1_000_001));assert_eq!(d.completions,0);
    }
    #[test] fn cancellation_surface_lifecycle_and_sync_cannot_rearm() {
        for reason in [1,2,3,4,5,6,7,8] {
            let mut d=ready(true);arm(&mut d,100);d.cancel(reason);
            assert_eq!(d.wait(101,None),Duration::ZERO);assert_eq!(d.cancels,1);
            for _ in 0..20 { assert!(d.admit(101)); }
            assert_eq!(d.fires,1);assert_eq!(d.completions,0);
            if reason==5 {assert_eq!(d.contamination,1);assert!(d.active);}
            if matches!(reason,4|7) {assert_eq!(d.surface,1);}
        }
    }
    #[test] fn failure_retry_dominates_shorter_drain_and_survives_cancel() {
        let mut d=ready(true);arm(&mut d,100);
        let retry=Some(Duration::from_millis(40));
        assert_eq!(d.wait(101,retry),Duration::from_millis(40));
        d.cancel(3);assert_eq!(d.wait(101,retry),Duration::from_millis(40));
        assert_eq!(d.wait(101,None),Duration::ZERO);
    }
    #[test] fn no_spin_and_no_idle_deadline() {
        let mut d=ready(true);assert_eq!(d.wait(1,None),Duration::ZERO);arm(&mut d,100);
        assert_eq!(d.wait(101,None),Duration::from_nanos(DELAY_NS-1));
        assert_eq!(d.wait(100+DELAY_NS-1,None),Duration::from_nanos(1));
        assert!(d.admit(100+DELAY_NS));assert_eq!(d.wait(100+DELAY_NS,None),Duration::ZERO);
    }
}

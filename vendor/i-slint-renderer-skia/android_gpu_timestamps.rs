// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

//! Opt-in whole-frame GPU command interval, NOT active GPU time.
//! Timestamp markers can perturb tilers; CPU issuance gaps, scheduling and
//! preemption can contribute to the interval. No waits or additional flushes.
//! https://registry.khronos.org/OpenGL/extensions/EXT/EXT_disjoint_timer_query.txt

use std::time::{Duration, Instant};
const CAPACITY: usize = 8;
const MAX_AGE: Duration = Duration::from_secs(2);

#[derive(Clone, Copy)]
struct Frame {
    id: i64,
    born: Instant,
}

#[derive(Default)]
struct Pending {
    slots: [Option<Frame>; CAPACITY],
}

// Reject ambiguous long intervals; a retained sample must fit within both the
// wall-clock retention bound and half the hardware timestamp modulus.
fn interval(start: u64, end: u64, bits: u32) -> Option<i64> {
    if !(30..=64).contains(&bits) {
        return None;
    }
    let mask = u64::MAX >> (64 - bits);
    let delta = end.wrapping_sub(start) & mask;
    (delta <= MAX_AGE.as_nanos() as u64 && delta < (1u64 << (bits - 1))).then_some(delta as i64)
}

impl Pending {
    fn clear(&mut self) {
        self.slots.fill(None);
    }

    // Results are staged until the post-read disjoint check. Never emit inside
    // query(): a later disjoint invalidates every read in this batch.
    fn poll(
        &mut self,
        now: Instant,
        bits: u32,
        mut disjoint: impl FnMut() -> bool,
        mut query: impl FnMut(usize) -> Option<(u64, u64)>,
    ) -> ([Option<(i64, i64)>; CAPACITY], bool) {
        let mut samples = [None; CAPACITY];
        if disjoint() {
            self.clear();
            return (samples, true);
        }
        for (index, slot) in self.slots.iter_mut().enumerate() {
            if let Some(frame) = *slot {
                if now.duration_since(frame.born) >= MAX_AGE {
                    *slot = None;
                } else if let Some((start, end)) = query(index) {
                    samples[index] = interval(start, end, bits).map(|ns| (frame.id, ns));
                    *slot = None;
                }
            }
        }
        if disjoint() {
            self.clear();
            return ([None; CAPACITY], true);
        }
        (samples, false)
    }
}

#[cfg(target_os = "android")]
mod android {
    use super::*;
    use glutin::display::GlDisplay;
    use std::ffi::{CStr, c_char, c_void};
    const TIMESTAMP: u32 = 0x8E28;
    type Gen = unsafe extern "system" fn(i32, *mut u32);
    type Delete = unsafe extern "system" fn(i32, *const u32);
    type Counter = unsafe extern "system" fn(u32, u32);
    type Available = unsafe extern "system" fn(u32, u32, *mut u32);
    type Result64 = unsafe extern "system" fn(u32, u32, *mut u64);
    type Get = unsafe extern "system" fn(u32, *mut i32);
    type Query = unsafe extern "system" fn(u32, u32, *mut i32);
    type StringFn = unsafe extern "system" fn(u32) -> *const c_char;

    struct Api {
        delete: Delete,
        counter: Counter,
        available: Available,
        result: Result64,
        get: Get,
        ids: [[u32; 2]; CAPACITY],
        bits: u32,
    }

    impl Api {
        fn load(display: &glutin::display::Display) -> Option<Self> {
            // SAFETY: current owning GL context, exact GLES extension ABI, all
            // pointers checked before use. No glGetError: Skia owns that state.
            unsafe {
                macro_rules! load {
                    ($name:literal, $ty:ty) => {{
                        let p = display.get_proc_address($name);
                        if p.is_null() {
                            return None;
                        }
                        core::mem::transmute::<*const c_void, $ty>(p)
                    }};
                }
                let string = load!(c"glGetString", StringFn);
                let extensions = string(0x1F03);
                if extensions.is_null()
                    || !CStr::from_ptr(extensions)
                        .to_bytes()
                        .split(|b| *b == b' ')
                        .any(|e| e == b"GL_EXT_disjoint_timer_query")
                {
                    return None;
                }
                let query = load!(c"glGetQueryivEXT", Query);
                let generate = load!(c"glGenQueriesEXT", Gen);
                let mut api = Self {
                    delete: load!(c"glDeleteQueriesEXT", Delete),
                    counter: load!(c"glQueryCounterEXT", Counter),
                    available: load!(c"glGetQueryObjectuivEXT", Available),
                    result: load!(c"glGetQueryObjectui64vEXT", Result64),
                    get: load!(c"glGetIntegerv", Get),
                    ids: [[0; 2]; CAPACITY],
                    bits: 0,
                };
                let mut bits = 0;
                query(TIMESTAMP, 0x8864, &mut bits);
                if !(30..=64).contains(&bits) {
                    return None;
                }
                api.bits = bits as u32;
                generate((CAPACITY * 2) as i32, api.ids.as_mut_ptr().cast());
                if api.ids.iter().flatten().any(|id| *id == 0) {
                    (api.delete)((CAPACITY * 2) as i32, api.ids.as_ptr().cast());
                    return None;
                }
                Some(api)
            }
        }

        fn disjoint(&self) -> bool {
            let mut value = 0;
            unsafe { (self.get)(0x8FBB, &mut value) };
            value != 0
        }

        fn read(&self, index: usize) -> Option<(u64, u64)> {
            let ids = self.ids[index];
            // Check BOTH endpoints before reading either result. Only one pass.
            let mut ready = [0; 2];
            for i in 0..2 {
                unsafe { (self.available)(ids[i], 0x8867, &mut ready[i]) };
            }
            if ready.contains(&0) {
                return None;
            }
            let mut values = [0; 2];
            for i in 0..2 {
                unsafe { (self.result)(ids[i], 0x8866, &mut values[i]) };
            }
            Some((values[0], values[1]))
        }
    }

    pub(crate) struct Timestamps {
        initialized: bool,
        api: Option<Api>,
        pending: Pending,
        active: Option<usize>,
        tracing: bool,
        epoch: i64,
        frame: i64,
    }

    impl Timestamps {
        // Called exactly once at surface creation. Off means no collector and
        // no GL discovery, clocks, tracing checks or per-render method calls.
        pub(crate) fn from_property() -> Option<Self> {
            unsafe extern "C" {
                fn __system_property_get(name: *const c_char, value: *mut c_char) -> i32;
            }
            let mut value = [0 as c_char; 92];
            let len = unsafe {
                __system_property_get(c"debug.chatter.gpu_timing".as_ptr(), value.as_mut_ptr())
            };
            if len != 1 || value[0] != b'1' as c_char {
                return None;
            }
            Some(Self {
                initialized: false,
                api: None,
                pending: Pending::default(),
                active: None,
                tracing: false,
                epoch: 0,
                frame: 0,
            })
        }

        pub(crate) fn begin(&mut self, display: &glutin::display::Display) {
            self.active = None;
            if !crate::renderer_trace::enabled() {
                self.pending.clear();
                self.tracing = false;
                return;
            }
            if !self.tracing {
                static NEXT: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(1);
                self.epoch = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                self.frame = 0;
                self.tracing = true;
            }
            if !self.initialized {
                self.initialized = true;
                self.api = Api::load(display);
            }
            crate::renderer_trace::counter(
                c"Chatter GPU timestamp API available",
                i64::from(self.api.is_some()),
            );
            crate::renderer_trace::counter(
                c"Chatter GPU timestamp counter bits",
                self.api.as_ref().map_or(0, |a| a.bits as i64),
            );
            let Some(api) = &self.api else {
                return;
            };
            let now = Instant::now();
            let (samples, disjoint) =
                self.pending
                    .poll(now, api.bits, || api.disjoint(), |i| api.read(i));
            crate::renderer_trace::counter(c"Chatter GPU timestamp disjoint", i64::from(disjoint));
            for (id, ns) in samples.into_iter().flatten() {
                let _sample = crate::renderer_trace::Section::new(c"Chatter GPU timestamp sample");
                crate::renderer_trace::counter(c"Chatter GPU timestamp epoch", self.epoch);
                crate::renderer_trace::counter(c"Chatter GPU timestamp frame id", id);
                crate::renderer_trace::counter(c"Chatter GPU command interval ns", ns);
            }
            self.frame = self.frame.saturating_add(1);
            let slot = self.pending.slots.iter().position(Option::is_none);
            crate::renderer_trace::counter(
                c"Chatter GPU timestamp capacity drop",
                i64::from(slot.is_none()),
            );
            if let Some(index) = slot {
                self.pending.slots[index] = Some(Frame {
                    id: self.frame,
                    born: now,
                });
                self.active = Some(index);
                unsafe { (api.counter)(api.ids[index][0], TIMESTAMP) };
            }
        }

        pub(crate) fn end(&mut self) {
            if let (Some(index), Some(api)) = (self.active.take(), &self.api) {
                if !crate::renderer_trace::enabled() {
                    self.pending.clear();
                    self.tracing = false;
                    return;
                }
                unsafe { (api.counter)(api.ids[index][1], TIMESTAMP) };
                let _submission =
                    crate::renderer_trace::Section::new(c"Chatter GPU timestamp submission");
                crate::renderer_trace::counter(c"Chatter GPU timestamp epoch", self.epoch);
                crate::renderer_trace::counter(c"Chatter GPU submitted frame id", self.frame);
            }
        }

        // No Drop GL calls: only the owner knows if its context is current.
        pub(crate) fn teardown(&mut self, current: bool) {
            if let Some(api) = self.api.take() {
                if current {
                    unsafe { (api.delete)((CAPACITY * 2) as i32, api.ids.as_ptr().cast()) };
                }
            }
            self.pending.clear();
            self.active = None;
        }
    }
}
#[cfg(target_os = "android")]
pub(crate) use android::Timestamps;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wrap_and_bad_widths() {
        assert_eq!(interval((1 << 40) - 10, 20, 40), Some(30));
        assert_eq!(interval(u64::MAX - 5, 4, 64), Some(10));
        for bits in [0, 29, 65] {
            assert_eq!(interval(1, 2, bits), None);
        }
        assert_eq!(interval(100, 99, 40), None);
        assert_eq!(interval(0, 1 << 29, 30), None);
        assert_eq!(interval(0, 2_000_000_001, 40), None);
    }
    #[test]
    fn bounded_pending_and_expiry_do_not_query() {
        let now = Instant::now();
        let mut pending = Pending::default();
        for slot in &mut pending.slots {
            *slot = Some(Frame { id: 1, born: now });
        }
        assert!(pending.slots.iter().position(Option::is_none).is_none());
        let (samples, bad) = pending.poll(now + MAX_AGE, 40, || false, |_| panic!("expired query"));
        assert!(!bad);
        assert!(samples.iter().all(Option::is_none));
        assert!(pending.slots.iter().all(Option::is_none));
    }
    #[test]
    fn unavailable_retained_available_retired_once() {
        let now = Instant::now();
        let mut pending = Pending::default();
        pending.slots[0] = Some(Frame { id: 42, born: now });
        let mut calls = 0;
        pending.poll(
            now,
            40,
            || false,
            |_| {
                calls += 1;
                None
            },
        );
        assert_eq!(calls, 1);
        assert!(pending.slots[0].is_some());
        let (samples, _) = pending.poll(now, 40, || false, |_| Some((100, 120)));
        assert_eq!(samples[0], Some((42, 20)));
        pending.poll(now, 40, || false, |_| panic!("retired query"));
    }
    #[test]
    fn disjoint_before_or_after_discards_entire_batch_and_pending() {
        let now = Instant::now();
        for before in [true, false] {
            let mut pending = Pending::default();
            for slot in &mut pending.slots {
                *slot = Some(Frame { id: 1, born: now });
            }
            let mut check = 0;
            let (samples, bad) = pending.poll(
                now,
                40,
                || {
                    check += 1;
                    before || check == 2
                },
                |i| {
                    assert!(!before);
                    if i == 0 { Some((10, 20)) } else { None }
                },
            );
            assert!(bad);
            assert!(samples.iter().all(Option::is_none));
            assert!(pending.slots.iter().all(Option::is_none));
        }
    }
}

// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

//! Bounded observational EGL timestamps. No waits, render requests, or content data.
//! https://registry.khronos.org/EGL/extensions/ANDROID/EGL_ANDROID_get_frame_timestamps.txt

const CAPACITY: usize = 8;
const FIELDS: usize = 7;
const PENDING: i64 = -2;
const MAX_AGE: std::time::Duration = std::time::Duration::from_secs(2);

// Process-wide comparison policy, shared by old and newly created surfaces.
// Updated at backend loop startup; EGL state remains owned by each surface.
#[cfg(feature = "experimental-native-pacing")]
static OBSERVER_ENABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

#[cfg(feature = "experimental-native-pacing")]
pub(crate) fn set_observer_enabled(enabled: bool) {
    OBSERVER_ENABLED.store(enabled, std::sync::atomic::Ordering::Relaxed);
}

#[cfg(feature = "experimental-native-pacing")]
fn observer_enabled() -> bool {
    OBSERVER_ENABLED.load(std::sync::atomic::Ordering::Relaxed)
}

/// One natural render per eight-render block. Mix the numeric block index to
/// avoid locking observation to one parity of alternating frame costs. No
/// content seed, randomness dependency, timer or additional frame.
#[derive(Default)]
pub(crate) struct Sampling {
    block: u64,
    phase: u8,
}

impl Sampling {
    fn offset(block: u64) -> u8 {
        let mut mixed = block.wrapping_add(0x9e3779b97f4a7c15);
        mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94d049bb133111eb);
        ((mixed ^ (mixed >> 31)) & 7) as u8
    }

    pub(crate) fn next(&mut self, compact: bool) -> bool {
        if !compact {
            return true;
        }
        let sample = self.phase == Self::offset(self.block);
        self.phase = (self.phase + 1) % 8;
        if self.phase == 0 {
            self.block = self.block.wrapping_add(1);
        }
        sample
    }
}

struct Frame {
    id: u64,
    born: std::time::Instant,
    remaining: [bool; FIELDS],
}

#[derive(Default)]
struct PendingFrames {
    frames: std::collections::VecDeque<Frame>,
}

impl PendingFrames {
    fn clear(&mut self) {
        self.frames.clear();
    }

    fn push(&mut self, id: u64, supported: [bool; FIELDS], now: std::time::Instant) {
        if self.frames.len() == CAPACITY {
            self.frames.pop_front();
        }
        self.frames.push_back(Frame {
            id,
            born: now,
            remaining: supported,
        });
    }

    // One query per retained frame per natural render. False retires EGL errors
    // (including expired driver history). Invalid fields retire independently.
    fn poll(
        &mut self,
        now: std::time::Instant,
        mut query: impl FnMut(u64, &mut [i64; FIELDS]) -> bool,
        mut emit: impl FnMut(u64, usize, i64),
    ) {
        self.frames.retain_mut(|frame| {
            if now.duration_since(frame.born) >= MAX_AGE {
                return false;
            }
            let mut values = [PENDING; FIELDS];
            if !query(frame.id, &mut values) {
                return false;
            }
            for (i, value) in values.into_iter().enumerate() {
                if frame.remaining[i] && value != PENDING {
                    frame.remaining[i] = false;
                    if value >= 0 {
                        emit(frame.id, i, value);
                    }
                }
            }
            frame.remaining.iter().any(|pending| *pending)
        });
    }
}

#[cfg(target_os = "android")]
mod android {
    use super::*;
    use glutin::{
        display::{AsRawDisplay, GlDisplay, RawDisplay},
        surface::{AsRawSurface, RawSurface, Surface, WindowSurface},
    };
    use std::ffi::{CStr, c_char, c_void};

    type Handle = *mut c_void;
    type Next = unsafe extern "C" fn(Handle, Handle, *mut u64) -> u32;
    type Supported = unsafe extern "C" fn(Handle, Handle, i32) -> u32;
    type Get = unsafe extern "C" fn(Handle, Handle, u64, i32, *const i32, *mut i64) -> u32;
    type Attrib = unsafe extern "C" fn(Handle, Handle, i32, i32) -> u32;
    type QueryString = unsafe extern "C" fn(Handle, i32) -> *const c_char;
    const NAMES: [i32; FIELDS] = [0x3434, 0x3435, 0x3436, 0x3437, 0x343A, 0x343B, 0x343C];
    const LABELS: [&CStr; FIELDS] = [
        c"Chatter EGL requested or queue ns",
        c"Chatter EGL rendering complete ns",
        c"Chatter EGL latch ns",
        c"Chatter EGL composition start ns",
        c"Chatter EGL display present ns",
        // Reusable without CPU blocking; reads may still be in flight.
        c"Chatter EGL dequeue ready ns",
        // All display/composition reads have completed.
        c"Chatter EGL reads done ns",
    ];

    struct Api {
        next: Next,
        get: Get,
        attrib: Attrib,
        supported: Supported,
    }

    /// Owned by one OpenGLSurface; raw handles are borrowed only during calls.
    #[derive(Default)]
    pub(crate) struct Timestamps {
        initialized: bool,
        api: Option<Api>,
        supported: [bool; FIELDS],
        enabled: bool,
        generation: i64,
        pending: PendingFrames,
        sampling: Sampling,
    }

    fn handles(
        display: &glutin::display::Display,
        surface: &Surface<WindowSurface>,
    ) -> Option<(Handle, Handle)> {
        match (display.raw_display(), surface.raw_surface()) {
            (RawDisplay::Egl(d), RawSurface::Egl(s)) => Some((d.cast_mut(), s.cast_mut())),
            #[allow(unreachable_patterns)]
            _ => None,
        }
    }

    impl Timestamps {
        fn initialize(&mut self, display: &glutin::display::Display, d: Handle, s: Handle) {
            self.initialized = true;
            // SAFETY: EGL ABI, live owning display/surface, checked non-null symbols.
            unsafe {
                macro_rules! load {
                    ($name:literal, $ty:ty) => {{
                        let p = display.get_proc_address($name);
                        if p.is_null() {
                            return;
                        }
                        core::mem::transmute::<*const c_void, $ty>(p)
                    }};
                }
                let query = load!(c"eglQueryString", QueryString);
                let extensions = query(d, 0x3055); // EGL_EXTENSIONS
                if extensions.is_null()
                    || !CStr::from_ptr(extensions)
                        .to_bytes()
                        .split(|b| *b == b' ')
                        .any(|extension| extension == b"EGL_ANDROID_get_frame_timestamps")
                {
                    return;
                }
                let api = Api {
                    next: load!(c"eglGetNextFrameIdANDROID", Next),
                    get: load!(c"eglGetFrameTimestampsANDROID", Get),
                    attrib: load!(c"eglSurfaceAttrib", Attrib),
                    supported: load!(c"eglGetFrameTimestampSupportedANDROID", Supported),
                };
                self.supported = NAMES.map(|name| (api.supported)(d, s, name) != 0);
                if self.supported.iter().any(|v| *v) {
                    self.api = Some(api);
                }
            }
        }

        #[cfg(feature = "painted-scroll-probe")]
        pub(crate) fn painted_generation(&self) -> i64 { self.generation }

        pub(crate) fn before_swap(
            &mut self,
            display: &glutin::display::Display,
            surface: &Surface<WindowSurface>,
        ) -> Option<u64> {
            #[cfg(feature = "experimental-native-pacing")]
            {
                let selected = observer_enabled();
                #[cfg(feature = "painted-scroll-probe")]
                let selected = selected || i_slint_core::painted_scroll_probe::active();
                // Selected policy is distinct from unsupported API/collection.
                crate::renderer_trace::counter(c"Chatter EGL observer selected", i64::from(selected));
                if !selected {
                    self.disable(display, surface);
                    return None;
                }
            }
            let (d, s) = handles(display, surface)?;
            if !crate::renderer_trace::observation_enabled() {
                self.disable(display, surface);
                return None;
            }
            let compact = crate::renderer_trace::compact();
            #[cfg(feature = "painted-scroll-probe")]
            let compact = compact && !i_slint_core::painted_scroll_probe::active();
            if !self.initialized {
                self.initialize(display, d, s);
            }
            // A missing sample must be distinguishable from an unsupported API.
            crate::renderer_trace::counter(
                c"Chatter EGL timestamp API available",
                i64::from(self.api.is_some()),
            );
            crate::renderer_trace::counter(
                c"Chatter EGL timestamp support mask",
                self.supported.iter().enumerate().fold(0i64, |mask, (i, supported)| {
                    mask | (i64::from(*supported) << i)
                }),
            );
            let api = self.api.as_ref()?;
            if !self.enabled {
                // SAFETY: borrowed live surface; collection is observational.
                self.enabled = unsafe { (api.attrib)(d, s, 0x3430, 1) != 0 };
                crate::renderer_trace::counter(
                    c"Chatter EGL timestamp collection enabled",
                    i64::from(self.enabled),
                );
                if !self.enabled {
                    return None;
                }
                // Anonymous process-wide generation distinguishes surfaces and
                // observed trace sessions without logging a native handle.
                static NEXT: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(1);
                #[cfg(not(feature = "painted-scroll-probe"))]
                { self.generation = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed); }
                #[cfg(feature = "painted-scroll-probe")]
                {
                    let Ok(generation) = NEXT.fetch_update(std::sync::atomic::Ordering::Relaxed,
                        std::sync::atomic::Ordering::Relaxed, |n| n.checked_add(1)) else {
                        i_slint_core::painted_scroll_probe::cancel();
                        self.disable(display, surface);
                        return None;
                    };
                    self.generation = generation;
                }
            }
            let generation = self.generation;
            let supported = self.supported;
            self.pending.poll(
                std::time::Instant::now(),
                |id, values| {
                    let mut names = [0; FIELDS];
                    let mut packed = [PENDING; FIELDS];
                    let mut count = 0;
                    for i in 0..FIELDS {
                        if supported[i] {
                            names[count] = NAMES[i];
                            count += 1;
                        }
                    }
                    // SAFETY: arrays contain count supported names and writable outputs.
                    if unsafe {
                        (api.get)(d, s, id, count as i32, names.as_ptr(), packed.as_mut_ptr())
                    } == 0
                    {
                        return false;
                    }
                    let mut index = 0;
                    for i in 0..FIELDS {
                        if supported[i] {
                            values[i] = packed[index];
                            index += 1;
                        }
                    }
                    true
                },
                |id, field, value| {
                    // Each value is preceded by its frame ID: pending frames can resolve out of order.
                    if let Ok(id) = i64::try_from(id) {
                        let _sample =
                            crate::renderer_trace::Section::new(c"Chatter EGL timestamp sample");
                        crate::renderer_trace::counter(c"Chatter EGL generation", generation);
                        crate::renderer_trace::counter(c"Chatter EGL timestamp frame id", id);
                        crate::renderer_trace::counter(LABELS[field], value);
                    }
                },
            );
            // Poll retained records on every natural render, but request a new
            // ID only at the compact sampling cadence. Full mode is unchanged.
            if !self.sampling.next(compact) {
                return None;
            }
            let mut id = 0;
            // Last EGL operation before the caller's swap; retain only on success.
            if unsafe { (api.next)(d, s, &mut id) } != 0 {
                Some(id)
            } else {
                None
            }
        }

        pub(crate) fn swapped(&mut self, id: Option<u64>) {
            #[cfg(feature = "experimental-native-pacing")]
            if !observer_enabled() {
                #[cfg(feature = "painted-scroll-probe")]
                if !i_slint_core::painted_scroll_probe::active() { return; }
                #[cfg(not(feature = "painted-scroll-probe"))]
                return;
            }
            if let Some(id) = id {
                if let Ok(value) = i64::try_from(id) {
                    let _submission =
                        crate::renderer_trace::Section::new(c"Chatter EGL submission");
                    crate::renderer_trace::counter(c"Chatter EGL generation", self.generation);
                    crate::renderer_trace::counter(c"Chatter EGL submitted frame id", value);
                }
                self.pending
                    .push(id, self.supported, std::time::Instant::now());
            }
        }

        pub(crate) fn disable(
            &mut self,
            display: &glutin::display::Display,
            surface: &Surface<WindowSurface>,
        ) {
            self.pending.clear();
            self.sampling = Sampling::default();
            if self.enabled {
                if let (Some(api), Some((d, s))) = (&self.api, handles(display, surface)) {
                    // SAFETY: owner calls before destroying the live surface.
                    if unsafe { (api.attrib)(d, s, 0x3430, 0) } != 0 {
                        self.enabled = false;
                    }
                }
            }
        }
    }
}
#[cfg(target_os = "android")]
pub(crate) use android::Timestamps;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn compact_sampling_known_answer_and_session_reset() {
        let mut sampling = Sampling::default();
        let sampled: Vec<_> = (1..=40).filter(|_| sampling.next(true)).collect();
        assert_eq!(sampled, [8, 10, 23, 30, 35]);
        let mut sampling = Sampling::default();
        assert!((0..25).all(|_| sampling.next(false)));
        // A new observation session restarts the deterministic schedule.
        sampling = Sampling::default();
        assert_eq!((1..=8).filter(|_| sampling.next(true)).collect::<Vec<_>>(), [8]);
    }

    #[test]
    fn mixed_blocks_have_exact_budget_both_parities_and_bounded_gaps() {
        let mut sampling = Sampling::default();
        let selected: Vec<_> = (0..8192).filter(|_| sampling.next(true)).collect();
        assert_eq!(selected.len(), 1024);
        for (block, &render) in selected.iter().enumerate() {
            assert_eq!(render / 8, block);
            assert!(render % 8 < 8);
        }
        assert!(selected.iter().any(|render| render % 2 == 0));
        assert!(selected.iter().any(|render| render % 2 == 1));
        assert!(selected.windows(2).all(|pair| (1..=15).contains(&(pair[1] - pair[0]))));
        let mut sampling = Sampling { block: u64::MAX, phase: 7 };
        sampling.next(true);
        assert_eq!(sampling.block, 0);
        assert_eq!(sampling.phase, 0);
    }

    #[test]
    fn sampled_identity_survives_unsampled_polls_and_out_of_order_results() {
        let mut sampling = Sampling::default();
        let mut pending = PendingFrames::default();
        let now = std::time::Instant::now();
        let mut submissions = Vec::new();
        let mut emitted = Vec::new();
        for render in 1..=11 {
            pending.poll(now, |id, values| {
                // The newer sampled frame resolves before the older one.
                if render == 11 && id == 110 {
                    values[0] = 900;
                }
                true
            }, |id, field, value| emitted.push((id, field, value)));
            let id = sampling.next(true).then_some(100 + render);
            // Every render has fresh optional identity. The private parser
            // resets its submission keys at each outer Native render begin.
            submissions.push(id);
            if let Some(id) = id {
                pending.push(id, [true, false, false, false, false, false, false], now);
            }
        }
        assert_eq!(submissions, [None, None, None, None, None, None, None, Some(108), None, Some(110), None]);
        assert_eq!(emitted, [(110, 0, 900)]);
        assert_eq!(pending.frames.front().unwrap().id, 108);
        pending.poll(now, |_, values| { values[0] = 100; true },
            |id, field, value| emitted.push((id, field, value)));
        assert_eq!(emitted, [(110, 0, 900), (108, 0, 100)]);
    }

    #[test]
    fn each_natural_poll_queries_at_most_eight_pending_records() {
        let mut pending = PendingFrames::default();
        let now = std::time::Instant::now();
        for id in 1..=100 {
            pending.push(id, [true; FIELDS], now);
        }
        let mut queried = Vec::new();
        pending.poll(now, |id, _| { queried.push(id); true }, |_, _, _| panic!());
        assert_eq!(queried, [93, 94, 95, 96, 97, 98, 99, 100]);
    }

    #[test]
    fn bounded_queue_evicts_oldest() {
        let mut p = PendingFrames::default();
        let now = std::time::Instant::now();
        for id in 0..20 {
            p.push(id, [true; FIELDS], now);
        }
        assert_eq!(p.frames.len(), CAPACITY);
        assert_eq!(p.frames.front().unwrap().id, 12);
    }
    #[test]
    fn pending_partial_invalid_and_error_retirement() {
        let mut p = PendingFrames::default();
        let now = std::time::Instant::now();
        p.push(7, [true, true, true, false, false, false, false], now);
        let mut emitted = Vec::new();
        p.poll(
            now,
            |_, v| {
                *v = [10, PENDING, -1, PENDING, PENDING, PENDING, PENDING];
                true
            },
            |id, i, v| emitted.push((id, i, v)),
        );
        assert_eq!(emitted, [(7, 0, 10)]);
        assert_eq!(p.frames.len(), 1);
        p.poll(
            now,
            |_, v| {
                *v = [10, 20, -1, PENDING, PENDING, PENDING, PENDING];
                true
            },
            |id, i, v| emitted.push((id, i, v)),
        );
        assert_eq!(emitted, [(7, 0, 10), (7, 1, 20)]);
        assert!(p.frames.is_empty());
        p.push(8, [true; FIELDS], now);
        p.poll(now, |_, _| false, |_, _, _| panic!("error must not emit"));
        assert!(p.frames.is_empty());
    }
    #[test]
    fn expiry_and_disable_clear_without_queries() {
        let mut p = PendingFrames::default();
        let now = std::time::Instant::now();
        p.push(1, [true; FIELDS], now);
        p.poll(
            now + MAX_AGE,
            |_, _| panic!("expired frame queried"),
            |_, _, _| panic!(),
        );
        assert!(p.frames.is_empty());
        p.push(2, [true; FIELDS], now);
        p.clear();
        p.poll(
            now,
            |_, _| panic!("disabled frame queried"),
            |_, _, _| panic!(),
        );
    }
    #[test]
    fn late_reuse_fields_do_not_delay_original_fields_or_each_other() {
        let mut pending = PendingFrames::default();
        let now = std::time::Instant::now();
        pending.push(71, [true; FIELDS], now);
        let mut emitted = Vec::new();
        pending.poll(now, |_, v| {
            *v = [10, 20, 30, 40, 50, PENDING, PENDING];
            true
        }, |id, field, value| emitted.push((id, field, value)));
        assert_eq!(emitted, [(71, 0, 10), (71, 1, 20), (71, 2, 30), (71, 3, 40), (71, 4, 50)]);
        pending.poll(now, |_, v| {
            v[5] = 60;
            true
        }, |id, field, value| emitted.push((id, field, value)));
        assert_eq!(emitted.last(), Some(&(71, 5, 60)));
        assert_eq!(pending.frames.len(), 1);
        pending.poll(now, |_, v| {
            v[6] = 70;
            true
        }, |id, field, value| emitted.push((id, field, value)));
        assert_eq!(emitted.last(), Some(&(71, 6, 70)));
        assert_eq!(emitted.len(), FIELDS);
        assert!(pending.frames.is_empty());
    }

    #[test]
    fn reuse_fields_invalid_and_unsupported_retire_independently() {
        let mut pending = PendingFrames::default();
        let now = std::time::Instant::now();
        for supported in [
            [false, false, false, false, false, true, false],
            [false, false, false, false, false, false, true],
            [false, false, false, false, false, true, true],
        ] {
            pending.push(81, supported, now);
            pending.poll(now, |_, v| {
                v[5] = -1;
                v[6] = -1;
                true
            }, |_, _, _| panic!("invalid or unsupported field emitted"));
            assert!(pending.frames.is_empty());
        }
    }

}

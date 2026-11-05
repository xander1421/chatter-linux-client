//! Opt-in, numeric command-observed anchors. No content or persistent item cache.
//! A frame owns every weak handle; dropping its transaction always clears them.
use crate::item_tree::ItemWeak;
use core::cell::{Cell, RefCell};
use std::time::{Duration, Instant};

pub const CAPACITY: usize = 64;
/// Output budget, independent of the per-frame registration capacity. Retain
/// still-eligible previous identities, then fill by numeric identity order.
/// This chooses no owner/item type, content, row size or position heuristic.
pub const MAX_EMITTED_ANCHORS: usize = 4;
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct AnchorKey([u64; 8]);

#[path = "painted_scroll_probe_sources.rs"]
mod sources;
pub(crate) use sources::{ConditionalSource, VisitGuard};

const MAX_FRAMES: u64 = 4096;
const LIFETIME: Duration = Duration::from_secs(30);
std::thread_local! {
    static ENABLED: Cell<bool> = const { Cell::new(false) };
    static ACTIVE: Cell<bool> = const { Cell::new(false) };
    static STATE: RefCell<State> = RefCell::new(State::default());
    // One cancelled transaction's numeric summary, never item references.
    static CANCELLED: Cell<Option<(u64, [i64; 6])>> = const { Cell::new(None) };
}
fn serial() -> Option<u64> {
    use core::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let n = NEXT
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
            (n < i64::MAX as u64).then_some(n + 1)
        })
        .ok();
    if n.is_none() {
        cancel();
    }
    n
}
/// Called only with the backend's actual Java debug/API29 check AND exact
/// process-start property. Errors must arrive as false.
pub fn configure(allowed: bool) {
    if allowed {
        ENABLED.set(true);
    } else {
        cancel();
    }
}
#[inline]
pub fn active() -> bool {
    ACTIVE.get()
}
#[inline]
pub fn enabled() -> bool {
    ENABLED.get()
}
/// Numeric renderer transaction token; a stale renderer cannot enter a later frame.
pub fn frame_token() -> Option<(u64,u64)> {
    active().then(|| STATE.with_borrow(|s|(s.session,s.render)))
}
pub fn frame_matches(token: Option<(u64,u64)>) -> bool {
    active() && token.is_some_and(|key|STATE.with_borrow(|s|key==(s.session,s.render)))
}
pub fn cancel() {
    let was_active = active();
    if ENABLED.replace(false) || ACTIVE.replace(false) {
        if was_active {
            STATE.with_borrow(|s| CANCELLED.set(Some((s.render, registry_counts(s)))));
        }
        ACTIVE.set(false);
        STATE.with_borrow_mut(|s| *s = State::default());
    }
}

/// Persistent owner metadata consists ONLY of anonymous numbers. Rebinding is
/// deliberately conservative: virtualization ends the owner's binding interval.
#[derive(Default)]
pub struct Owner {
    id: Cell<u64>,
    epoch: Cell<u64>,
    binding: Cell<u64>,
}
impl Owner {
    pub fn mutated(&self) {
        if !enabled() {
            return;
        }
        let Some(epoch) = serial() else {
            return;
        };
        self.epoch.set(epoch);
        self.rebound();
    }
    pub fn rebound(&self) {
        if !enabled() {
            return;
        }
        if let Some(binding) = serial() {
            self.binding.set(binding);
        }
        self.invalidate();
    }
    fn invalidate(&self) {
        if active() {
            STATE.with_borrow_mut(|s| {
                for entry in s.entries.iter_mut().flatten() {
                    if entry.owner == self.id.get() {
                        entry.invalid = true;
                        s.reasons |= Reason::OwnerChanged as u32;
                    }
                }
                sources::invalidate_owner(s, self.id.get());
            });
        }
    }
    pub fn register(&self, weak: ItemWeak, index: usize) {
        if !active() {
            return;
        }
        if self.id.get() == 0 {
            let Some(id) = serial() else {
                return;
            };
            self.id.set(id);
        }
        if self.binding.get() == 0 {
            self.rebound();
        }
        if index > i64::MAX as usize {
            cancel();
            return;
        }
        STATE.with_borrow_mut(|s| {
            if let Some(e) = s.entries.iter_mut().flatten().find(|e| e.weak == weak) {
                if e.root.is_some() || e.witness.is_some() {
                    s.reasons |= Reason::ReregisterObserved as u32;
                }
                // A clean ensure preserves both evidence and prior rejection.
                if e.owner == self.id.get() && e.epoch == self.epoch.get()
                    && e.index == index as u64 && e.binding == self.binding.get()
                {
                    return;
                }
                e.owner = self.id.get();
                e.epoch = self.epoch.get();
                e.index = index as u64;
                e.binding = self.binding.get();
                e.invalid = false;
                e.root = None;
                e.witness = None;
                return;
            }
            let Some(slot) = s.entries.iter_mut().find(|e| e.is_none()) else {
                s.overflow = true;
                s.reasons |= Reason::RegistryOverflow as u32;
                return;
            };
            *slot = Some(Entry {
                weak,
                owner: self.id.get(),
                epoch: self.epoch.get(),
                index: index as u64,
                binding: self.binding.get(),
                invalid: false,
                root: None,
                witness: None,
            });
        });
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        self.invalidate();
    }
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Geometry {
    /// Row-major 3x3 matrix; physical geometry is already scaled by the renderer.
    pub canvas: [f32; 9],
    /// Logical [m11,m12,m21,m22,m31,m32]. Never add content_y.
    pub logical: [f32; 6],
}
impl Geometry {
    fn supported(self) -> bool {
        self.canvas
            .iter()
            .chain(self.logical.iter())
            .all(|n| n.is_finite())
            && self.logical[..4] == [1., 0., 0., 1.]
            && self.canvas[0] == 1.
            && self.canvas[1] == 0.
            && self.canvas[3] == 0.
            && self.canvas[4] == 1.
            && self.canvas[6..] == [0., 0., 1.]
    }
}
#[derive(Clone, Copy)]
struct Witness {
    matrix: [f32; 9],
    anchor: [f32; 2],
    kind: u8,
    primitive: u32,
    source: u64,
    source_epoch: u64,
    source_slot: Option<usize>,
}
struct Entry {
    weak: ItemWeak,
    owner: u64,
    epoch: u64,
    index: u64,
    binding: u64,
    invalid: bool,
    root: Option<Geometry>,
    witness: Option<Witness>,
}
impl Entry {
    fn key(&self, witness: Witness) -> AnchorKey {
        AnchorKey([
            self.owner,
            self.epoch,
            self.index,
            self.binding,
            witness.source,
            witness.source_epoch,
            u64::from(witness.primitive),
            u64::from(witness.kind),
        ])
    }
}
struct State {
    start: Option<Instant>,
    session: u64,
    frames: u64,
    render: u64,
    entries: [Option<Entry>; CAPACITY],
    overflow: bool,
    reasons: u32,
    selected: [Option<AnchorKey>; MAX_EMITTED_ANCHORS],
    sources: sources::SourceState,
}
impl Default for State {
    fn default() -> Self {
        Self {
            start: None,
            session: 0,
            frames: 0,
            render: 0,
            entries: [const { None }; CAPACITY],
            overflow: false,
            reasons: 0,
            selected: [None; MAX_EMITTED_ANCHORS],
            sources: Default::default(),
        }
    }
}
fn registry_counts(s: &State) -> [i64; 6] {
    let mut counts = [0, i64::from(s.overflow), 0, 0, 0, i64::from(s.reasons)];
    for e in s.entries.iter().flatten() {
        counts[0] += 1;
        counts[2] += i64::from(e.root.is_some());
        counts[3] += i64::from(e.witness.is_some());
        counts[4] += i64::from(e.invalid);
    }
    counts
}
pub fn clear_registry() {
    if active() {
        STATE.with_borrow_mut(|s| {
            s.entries = [const { None }; CAPACITY];
            s.overflow = false;
            s.reasons = 0;
            let token = s.sources.token;
            s.sources = Default::default();
            s.sources.token = token;
        });
    }
}
/// Entry hook, after translate, ONLY on the main renderer. The caller keeps the
/// returned mask in its ordinary save/restore state, so nested roots are bounded.
pub fn enter(weak: &ItemWeak, root: Geometry, mask: u64, supported: bool) -> u64 {
    if !active() {
        return mask;
    }
    STATE.with_borrow_mut(|s| {
        sources::observe_root(s, weak, root, mask, supported);
        let mut result = mask;
        for (i, e) in s.entries.iter_mut().enumerate() {
            if let Some(e) = e
                && e.weak == *weak
            {
                if !supported || !root.supported() {
                    e.invalid = true;
                    s.reasons |= Reason::RootUnsupported as u32;
                }
                match e.root {
                    None => e.root = Some(root),
                    Some(first) if first != root => {
                        // The first root matrix belongs to the first command.
                        // A later traversal may not silently overwrite its basis.
                        e.invalid = true;
                        s.reasons |= Reason::RootGeometryChanged as u32;
                    }
                    Some(_) => {}
                }
                result |= 1u64 << i;
            }
        }
        result
    })
}
/// Reject an ambiguous effect scope, INCLUDING earlier subtree witnesses.
pub fn invalidate(mask: u64) {
    if !active() {
        return;
    }
    STATE.with_borrow_mut(|s| {
        for (i, e) in s.entries.iter_mut().enumerate() {
            if mask & (1u64 << i) != 0
                && let Some(e) = e
            {
                e.invalid = true;
            }
        }
    });
}
/// A later descendant effect/draw cannot retroactively alter an already
/// accepted main-target command outside it. Keep prior rejection, and keep
/// rejecting every entry that has not yet witnessed a supported command.
/// Owner, liveness and root-geometry invalidation still use invalidate().
pub fn reject_unwitnessed(mask: u64) {
    if !active() { return; }
    STATE.with_borrow_mut(|s| {
        for (i, e) in s.entries.iter_mut().enumerate() {
            if mask & (1u64 << i) != 0 && let Some(e) = e && e.witness.is_none() {
                e.invalid = true;
            }
        }
    });
}
/// Called immediately before an EXISTING eligible Skia draw command, after
/// empty/paint checks. No geometry-only or traversal-only fallback exists.
pub fn witness(
    paint_root: &ItemWeak,
    primitive: u32,
    mask: u64,
    draw: Geometry,
    scale: f32,
    kind: u8,
    eligible: bool,
) {
    if !active() || mask == 0 {
        return;
    }
    if !eligible || !draw.supported() || !scale.is_finite() || scale <= 0. {
        note(if !eligible { Reason::PrimitiveEligibility } else { Reason::UnsupportedDraw });
        reject_unwitnessed(mask);
        return;
    }
    STATE.with_borrow_mut(|s| {
        for i in 0..CAPACITY {
            if mask & (1u64 << i) == 0 { continue; }
            let Some(e) = s.entries[i].as_ref() else { continue; };
            if e.invalid || e.witness.is_some() { continue; }
            let origin = if e.weak == *paint_root && !sources::source_scope_open(s) {
                Some((0, 0, None))
            } else {
                sources::witness_origin(s, i, paint_root)
            };
            let Some((source, source_epoch, source_slot)) = origin else {
                s.reasons |= Reason::ForeignTree as u32;
                continue;
            };
            let e = s.entries[i].as_mut().unwrap();
            let Some(root) = e.root else { continue; };
            let anchor = [
                draw.canvas[2] - (draw.logical[4] - root.logical[4]) * scale,
                draw.canvas[5] - (draw.logical[5] - root.logical[5]) * scale,
            ];
            e.witness = Some(Witness {
                matrix: draw.canvas, anchor, kind, primitive,
                source, source_epoch, source_slot,
            });
        }
    });
}

/// Numeric tuple labels are also the renderer's closed compact allowlist.
/// Every emitted tuple has begin/end with identical monotonically unique render
/// serial. Consumers must require ALL labels exactly once between those markers.
pub const LABELS: [&core::ffi::CStr; 41] = [
    c"Chatter painted v16 begin",
    c"Chatter painted v16 version",
    c"Chatter painted v16 session",
    c"Chatter painted v16 render",
    c"Chatter painted v16 EGL generation",
    c"Chatter painted v16 EGL ID",
    c"Chatter painted v16 owner",
    c"Chatter painted v16 epoch",
    c"Chatter painted v16 index",
    c"Chatter painted v16 binding",
    c"Chatter painted v16 source",
    c"Chatter painted v16 source epoch",
    c"Chatter painted v16 witness",
    c"Chatter painted v16 anchor x1024",
    c"Chatter painted v16 anchor y1024",
    c"Chatter painted v16 logical 0",
    c"Chatter painted v16 logical 1",
    c"Chatter painted v16 logical 2",
    c"Chatter painted v16 logical 3",
    c"Chatter painted v16 logical 4",
    c"Chatter painted v16 logical 5",
    c"Chatter painted v16 root canvas 0",
    c"Chatter painted v16 root canvas 1",
    c"Chatter painted v16 root canvas 2",
    c"Chatter painted v16 root canvas 3",
    c"Chatter painted v16 root canvas 4",
    c"Chatter painted v16 root canvas 5",
    c"Chatter painted v16 root canvas 6",
    c"Chatter painted v16 root canvas 7",
    c"Chatter painted v16 root canvas 8",
    c"Chatter painted v16 draw canvas 0",
    c"Chatter painted v16 draw canvas 1",
    c"Chatter painted v16 draw canvas 2",
    c"Chatter painted v16 draw canvas 3",
    c"Chatter painted v16 draw canvas 4",
    c"Chatter painted v16 draw canvas 5",
    c"Chatter painted v16 draw canvas 6",
    c"Chatter painted v16 draw canvas 7",
    c"Chatter painted v16 draw canvas 8",
    c"Chatter painted v16 primitive",
    c"Chatter painted v16 end",
];
/// Closed numeric schema. Counts describe the final frame-local registry,
/// including rejected witnesses; registered is capped at CAPACITY + overflow.
pub const AVAILABILITY_LABELS: [&core::ffi::CStr; 15] = [
    c"Chatter painted availability begin",
    c"Chatter painted availability session",
    c"Chatter painted availability render",
    c"Chatter painted availability active",
    c"Chatter painted availability EGL generation",
    c"Chatter painted availability EGL ID",
    c"Chatter painted availability swapped",
    c"Chatter painted availability registered",
    c"Chatter painted availability overflow",
    c"Chatter painted availability observed",
    c"Chatter painted availability witnesses",
    c"Chatter painted availability invalidated",
    c"Chatter painted availability emitted",
    c"Chatter painted availability reasons",
    c"Chatter painted availability end",
];
#[derive(Clone, Copy)]
#[repr(u32)]
pub enum Reason {
    RootUnsupported = 1 << 0,
    RootGeometryChanged = 1 << 1,
    Opacity = 1 << 2,
    CachedLayer = 1 << 3,
    Backdrop = 1 << 4,
    RoundedClip = 1 << 5,
    PrimitiveEligibility = 1 << 6,
    UnsupportedDraw = 1 << 7,
    ForeignTree = 1 << 8,
    ReregisterObserved = 1 << 9,
    WeakDead = 1 << 10,
    RegistryOverflow = 1 << 11,
    OwnerChanged = 1 << 12,
    NumericOverflow = 1 << 13,
    Cancelled = 1 << 14,
    NoSubmission = 1 << 15,
    SwapFailed = 1 << 16,
    SourceChanged = 1 << 17,
    SourceUnknownParent = 1 << 18,
    SourceMismatch = 1 << 19,
    SourceOverflow = 1 << 20,
    SourceScopeOrder = 1 << 21,
}
pub fn note(reason: Reason) {
    if active() { STATE.with_borrow_mut(|s| s.reasons |= reason as u32); }
}

type Emit = fn(&'static core::ffi::CStr, i64);
pub struct FrameGuard {
    live: bool,
    render: u64,
    session: u64,
    observer: Option<Emit>,
    reported: Cell<bool>,
    generation: Cell<i64>,
    egl_id: Cell<i64>,
    swapped: Cell<bool>,
    emitted: Cell<usize>,
}
impl FrameGuard {
    /// Construct on ordinary EGL entry. SDK/Swappy frames pass ordinary=false.
    /// Quiet warmup before tracing consumes neither time nor frame budget.
    pub fn begin(ordinary: bool, traced: bool) -> Self {
        Self::begin_with_observer(ordinary, traced, None)
    }
    /// Same gates and budget as begin; one fixed record is emitted on Drop,
    /// including zero-anchor, missing-ID, failed-swap and cancelled frames.
    pub fn begin_observed(ordinary: bool, traced: bool, emit: Emit) -> Self {
        Self::begin_with_observer(ordinary, traced, Some(emit))
    }
    fn inactive() -> Self {
        Self { live: false, render: 0, session: 0, observer: None,
            reported: Cell::new(false), generation: Cell::new(0), egl_id: Cell::new(0),
            swapped: Cell::new(false), emitted: Cell::new(0) }
    }
    fn begin_with_observer(ordinary: bool, traced: bool, observer: Option<Emit>) -> Self {
        // Any nested surface render is ambiguous, including an SDK/quiet one.
        // Reject it before the ordinary/traced gates so it cannot borrow the
        // outer transaction's renderer token or Conditional source scopes.
        if active() {
            cancel();
            return Self::inactive();
        }
        if !enabled() || !ordinary || !traced {
            return Self::inactive();
        }
        let now = Instant::now();
        let allowed = STATE.with_borrow_mut(|s| {
            if s.start
                .is_some_and(|start| now.duration_since(start) >= LIFETIME)
                || s.frames >= MAX_FRAMES
            {
                return false;
            }
            true
        });
        if !allowed {
            cancel();
            return Self::inactive();
        }
        let Some(render) = serial() else {
            return Self::inactive();
        };
        STATE.with_borrow_mut(|s| {
            if s.start.is_none() {
                s.start = Some(now);
                s.session = render;
            }
            s.frames += 1;
            s.render = render;
        });
        CANCELLED.set(None);
        ACTIVE.set(true);
        clear_registry();
        let session = STATE.with_borrow(|s| s.session);
        let mut guard = Self::inactive();
        guard.live = true;
        guard.render = render;
        guard.session = session;
        guard.observer = observer;
        guard
    }
    pub fn commit(
        &self,
        generation: i64,
        id: Option<u64>,
        swapped: bool,
        mut emit: impl FnMut(&'static core::ffi::CStr, i64),
    ) {
        self.generation.set(generation.max(0));
        self.egl_id.set(id.and_then(|n| i64::try_from(n).ok()).unwrap_or(0));
        self.swapped.set(swapped);
        self.emitted.set(0);
        let current = self.live && active() && STATE.with_borrow(|s| s.render == self.render && s.session == self.session);
        if current {
            if !swapped { note(Reason::SwapFailed); }
            if generation <= 0 || id.is_none() { note(Reason::NoSubmission); }
        }
        if !current || !swapped || generation <= 0 {
            return;
        }
        let Some(id) = id else {
            return;
        };
        let Ok(id) = i64::try_from(id) else {
            note(Reason::NumericOverflow);
            cancel();
            return;
        };
        // Snapshot numeric records, then release RefCell before callbacks.
        let mut records = [[0i64; 41]; MAX_EMITTED_ANCHORS];
        let mut count = 0;
        let mut numeric_overflow = false;
        let valid = STATE.with_borrow_mut(|s| {
            if s.overflow || s.render != self.render || s.session != self.session {
                return false;
            }
            if sources::scopes_open(s) { s.reasons |= Reason::SourceScopeOrder as u32; return false; }
            // Fixed-size priority insertion: retained identities first, then
            // numeric key order. A fresh smaller key cannot displace a retained
            // eligible key. Weak handles remain confined to this frame's registry.
            let mut chosen: [Option<(bool, AnchorKey, usize)>; MAX_EMITTED_ANCHORS] =
                [None; MAX_EMITTED_ANCHORS];
            sources::check_liveness(s);
            for (index, entry) in s.entries.iter_mut().enumerate() {
                let Some(e) = entry else {
                    continue;
                };
                if e.weak.upgrade().is_none() {
                    e.invalid = true;
                    s.reasons |= Reason::WeakDead as u32;
                }
                if e.invalid || e.root.is_none() { continue; }
                let Some(w) = e.witness else {
                    continue;
                };
                let key = e.key(w);
                let rank = (!s.selected.contains(&Some(key)), key, index);
                if let Some(position) = chosen.iter().position(|c| c.is_none_or(|c| rank < c)) {
                    for i in (position + 1..MAX_EMITTED_ANCHORS).rev() {
                        chosen[i] = chosen[i - 1];
                    }
                    chosen[position] = Some(rank);
                }
            }
            let mut next_selected = [None; MAX_EMITTED_ANCHORS];
            for (_, key, index) in chosen.into_iter().flatten() {
                let e = s.entries[index].as_ref().unwrap();
                let (root, w) = (e.root.unwrap(), e.witness.unwrap());
                let rec = &mut records[count];
                rec[..13].copy_from_slice(&[
                    s.render as i64,
                    16,
                    s.session as i64,
                    s.render as i64,
                    generation,
                    id,
                    e.owner as i64,
                    e.epoch as i64,
                    e.index as i64,
                    e.binding as i64,
                    w.source as i64,
                    w.source_epoch as i64,
                    i64::from(w.kind),
                ]);
                let values = w
                    .anchor
                    .into_iter()
                    .chain(root.logical)
                    .chain(root.canvas)
                    .chain(w.matrix);
                for (target, value) in rec[13..39].iter_mut().zip(values) {
                    let n = f64::from(value) * 1024.;
                    if !n.is_finite() || n.abs() >= i64::MAX as f64 {
                        numeric_overflow = true;
                        return false;
                    }
                    *target = n.round() as i64;
                }
                rec[39] = i64::from(w.primitive);
                rec[40] = s.render as i64;
                next_selected[count] = Some(key);
                count += 1;
            }
            s.selected = next_selected;
            true
        });
        if numeric_overflow {
            note(Reason::NumericOverflow);
            cancel();
            return;
        }
        if !valid {
            return;
        }
        for record in &records[..count] {
            let key=AnchorKey([record[6] as u64,record[7] as u64,record[8] as u64,
                record[9] as u64,record[10] as u64,record[11] as u64,
                record[39] as u64,record[12] as u64]);
            for (label, value) in LABELS.into_iter().zip(record) {
                // Callbacks may cancel/re-enter while emitting. An incomplete
                // tuple has no end marker and must remain unavailable to consumers.
                if !frame_matches(Some((self.session,self.render)))
                    || !STATE.with_borrow(|s|sources::key_live(s,key)) { return; }
                emit(label, *value);
            }
            self.emitted.set(self.emitted.get()+1);
        }
    }
    /// Numeric snapshot only. No extra drawing, traversal or registration.
    pub fn availability_snapshot(&self) -> Option<[i64; 15]> {
        if !self.live { return None; }
        let mut record = [0; 15];
        record[0] = self.render as i64;
        record[1] = self.session as i64;
        record[2] = self.render as i64;
        record[4] = self.generation.get();
        record[5] = self.egl_id.get();
        record[6] = i64::from(self.swapped.get());
        record[12] = self.emitted.get() as i64;
        record[14] = self.render as i64;
        let current = STATE.with_borrow(|s| {
            (active() && s.render == self.render).then(|| registry_counts(s))
        });
        let counts = if let Some(counts) = current {
            record[3] = 1;
            counts
        } else {
            // Never substitute a later frame's registry for this transaction.
            let mut counts = CANCELLED.get().filter(|(render, _)| *render == self.render)
                .map_or([-1, -1, -1, -1, -1, 0], |(_, counts)| counts);
            counts[5] |= Reason::Cancelled as i64;
            counts
        };
        record[7..12].copy_from_slice(&counts[..5]);
        record[13] = counts[5];
        Some(record)
    }
    /// Exactly once, outside any registry borrow; Drop is the runtime caller.
    pub fn report_availability(&self, mut emit: impl FnMut(&'static core::ffi::CStr, i64)) {
        if !self.live || self.reported.replace(true) { return; }
        if let Some(record) = self.availability_snapshot() {
            for (label, value) in AVAILABILITY_LABELS.into_iter().zip(record) { emit(label, value); }
        }
    }
}
// Numeric cleanup guard survives an observer callback unwinding. It can never
// clear a later transaction that was entered by a re-entrant observer.
struct FrameCleanup { render: u64, session: u64 }
impl Drop for FrameCleanup {
    fn drop(&mut self) {
        if CANCELLED.get().is_some_and(|(render, _)| render == self.render) {
            CANCELLED.set(None);
        }
        if STATE.with_borrow(|s| s.render == self.render && s.session == self.session) {
            clear_registry();
            ACTIVE.set(false);
        }
    }
}
impl Drop for FrameGuard {
    fn drop(&mut self) {
        if self.live {
            let _cleanup = FrameCleanup { render: self.render, session: self.session };
            if let Some(observer) = self.observer { self.report_availability(observer); }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn disabled_and_quiet_warmup_do_not_start_budget() {
        cancel();
        let f = FrameGuard::begin(true, true);
        assert!(!f.live);
        configure(true);
        let quiet = FrameGuard::begin(true, false);
        assert!(!quiet.live);
        STATE.with_borrow(|s| {
            assert!(s.start.is_none());
            assert_eq!(s.frames, 0);
        });
        let sdk = FrameGuard::begin(false, true);
        assert!(!sdk.live);
        let natural = FrameGuard::begin(true, true);
        assert!(natural.live);
        drop(natural);
        cancel();
    }
    #[test]
    fn expires_from_first_traced_natural_frame_and_frame_cap() {
        cancel();
        configure(true);
        let first = FrameGuard::begin(true, true);
        drop(first);
        STATE.with_borrow_mut(|s| s.start = Some(Instant::now() - LIFETIME));
        assert!(!FrameGuard::begin(true, true).live);
        assert!(!enabled());
        configure(true);
        let first = FrameGuard::begin(true, true);
        drop(first);
        STATE.with_borrow_mut(|s| s.frames = MAX_FRAMES);
        assert!(!FrameGuard::begin(true, true).live);
        assert!(!enabled());
    }
    #[test]
    fn nested_render_cancels_without_mixing_frames() {
        cancel();
        configure(true);
        let outer = FrameGuard::begin(true, true);
        assert!(outer.live);
        let nested = FrameGuard::begin(true, true);
        assert!(!nested.live);
        assert!(!active());
        assert!(!enabled());
    }
    #[test]
    fn numeric_matrix_contract_rejects_perspective_and_nan() {
        let mut g = Geometry {
            canvas: [1., 0., 2., 0., 1., 3., 0., 0., 1.],
            logical: [1., 0., 0., 1., 2., 3.],
        };
        assert!(g.supported());
        g.canvas[6] = 0.1;
        assert!(!g.supported());
        g.canvas[6] = 0.;
        g.logical[5] = f32::NAN;
        assert!(!g.supported());
    }
}

#[cfg(test)]
pub(crate) fn v16_source_counts()->[usize;3]{STATE.with_borrow(sources::counts)}

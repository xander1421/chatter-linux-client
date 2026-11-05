//! V16 exact Conditional source lifetimes. Only numbers persist in Conditional;
//! every weak handle below belongs to the active bounded frame/visit transaction.
use super::*;
use crate::item_tree::ItemRc;
const SOURCE_CAPACITY: usize = 64;
const VISIT_DEPTH: usize = 128;
const SOURCE_DEPTH: usize = 16;
#[derive(Default)]
pub struct ConditionalSource {
    nonce: Cell<u64>,
    epoch: Cell<u64>,
    parent: Cell<Option<(u64, u64)>>,
}
impl ConditionalSource {
    fn stamp(&self) -> bool {
        if !enabled() {
            return false;
        }
        if self.nonce.get() == 0 {
            let Some(n) = serial() else {
                return false;
            };
            self.nonce.set(n);
        }
        if self.epoch.get() == 0 {
            let Some(n) = serial() else {
                return false;
            };
            self.epoch.set(n);
        }
        true
    }
    pub fn rebound(&self) {
        self.invalidate();
        if !enabled() {
            self.nonce.set(0);
            self.epoch.set(0);
            self.parent.set(None);
            return;
        }
        if let Some(n) = serial() {
            self.epoch.set(n);
        }
    }
    pub fn retired(&self) {
        self.invalidate();
        self.nonce.set(0);
        self.epoch.set(0);
        self.parent.set(None);
    }
    pub fn materialized(&self) {
        self.retired();
        self.stamp();
    }
    fn invalidate(&self) {
        if active() && self.nonce.get() != 0 {
            STATE.with_borrow_mut(|s| invalidate_nonce(s, self.nonce.get()));
        }
    }
    pub fn scope(&self, root: ItemWeak) -> SourceGuard {
        if !active() || !self.stamp() {
            return SourceGuard(None);
        }
        // Numeric parent-context continuity prevents an externally held nested
        // source from retaining its identity when an ancestor is replaced and
        // re-adopts the same allocation. No parent handle survives this call.
        let parent = STATE.with_borrow(|s| {
            let (_, parent) = parent_route(s)?;
            Some(parent.map_or((0, 0), |i| {
                let e = s.sources.entries[i].as_ref().unwrap();
                (e.nonce, e.epoch)
            }))
        });
        if let Some(parent) = parent {
            if self.parent.get().is_some_and(|old| old != parent) {
                self.rebound();
            }
            self.parent.set(Some(parent));
        }
        if !active() {
            return SourceGuard(None);
        }
        let nonce = self.nonce.get();
        let epoch = self.epoch.get();
        let key = STATE.with_borrow_mut(|s| {
            let Some(token) = next_token(s) else {
                return None;
            };
            if s.sources.scope_len == SOURCE_DEPTH {
                overflow(s);
                return None;
            }
            let route = parent_route(s);
            let slot = if let Some((row, parent)) = route {
                let e = s.entries[row].as_ref().unwrap();
                let owner = [e.owner, e.epoch, e.index, e.binding];
                if let Some(i) = s.sources.entries.iter().position(|x| {
                    x.as_ref()
                        .is_some_and(|x| x.nonce == nonce && x.epoch == epoch)
                }) {
                    let old = s.sources.entries[i].as_ref().unwrap();
                    if old.weak != root
                        || old.row != row
                        || old.owner != owner
                        || old.parent != parent
                    {
                        s.reasons |= Reason::SourceMismatch as u32;
                        s.sources.entries[i].as_mut().unwrap().valid = false;
                        None
                    } else {
                        Some(i)
                    }
                } else if let Some(i) = s.sources.entries.iter().position(Option::is_none) {
                    s.sources.entries[i] = Some(SourceEntry {
                        nonce,
                        epoch,
                        weak: root,
                        row,
                        owner,
                        parent,
                        valid: true,
                        observed: false,
                    });
                    Some(i)
                } else {
                    overflow(s);
                    None
                }
            } else {
                s.reasons |= Reason::SourceUnknownParent as u32;
                None
            };
            // A rejected nested scope is a barrier, never a fallback to an outer source.
            s.sources.scopes[s.sources.scope_len] = ScopeSlot { token, slot };
            s.sources.scope_len += 1;
            Some((s.render, s.session, token))
        });
        SourceGuard(key)
    }
}
impl Drop for ConditionalSource {
    fn drop(&mut self) {
        self.invalidate();
    }
}
struct SourceEntry {
    nonce: u64,
    epoch: u64,
    weak: ItemWeak,
    row: usize,
    owner: [u64; 4],
    parent: Option<usize>,
    valid: bool,
    observed: bool,
}
struct VisitSlot {
    token: u64,
    root: ItemWeak,
}
#[derive(Clone, Copy, Default)]
struct ScopeSlot {
    token: u64,
    slot: Option<usize>,
}
pub(super) struct SourceState {
    entries: [Option<SourceEntry>; SOURCE_CAPACITY],
    visits: [Option<VisitSlot>; VISIT_DEPTH],
    visit_len: usize,
    scopes: [ScopeSlot; SOURCE_DEPTH],
    scope_len: usize,
    pub(super) token: u64,
}
impl Default for SourceState {
    fn default() -> Self {
        Self {
            entries: [const { None }; SOURCE_CAPACITY],
            visits: [const { None }; VISIT_DEPTH],
            visit_len: 0,
            scopes: [ScopeSlot::default(); SOURCE_DEPTH],
            scope_len: 0,
            token: 0,
        }
    }
}
fn overflow(s: &mut State) {
    s.overflow = true;
    s.reasons |= Reason::SourceOverflow as u32;
}
fn next_token(s: &mut State) -> Option<u64> {
    let n = s.sources.token.checked_add(1);
    if let Some(n) = n {
        s.sources.token = n;
    } else {
        overflow(s);
    }
    n
}
fn chain_live(s: &State, mut i: usize) -> bool {
    for _ in 0..SOURCE_CAPACITY {
        let Some(e) = s.sources.entries.get(i).and_then(Option::as_ref) else {
            return false;
        };
        let Some(row) = s.entries.get(e.row).and_then(Option::as_ref) else {
            return false;
        };
        if !e.valid
            || row.invalid
            || e.owner != [row.owner, row.epoch, row.index, row.binding]
            || e.weak.upgrade().is_none()
            || row.weak.upgrade().is_none()
        {
            return false;
        }
        match e.parent {
            Some(p) => i = p,
            None => return true,
        }
    }
    false
}
// An exact registered row may itself be beneath a validated Conditional.
// Preserve that ancestor dependency; a held row allocation must not outlive
// the source that actually visited it. A rejected source scope is a barrier.
fn parent_route(s: &State) -> Option<(usize, Option<usize>)> {
    let current = &s.sources.visits[..s.sources.visit_len]
        .last()?
        .as_ref()?
        .root;
    let parent = if let Some(scope) = s.sources.scopes[..s.sources.scope_len].last() {
        let i = scope.slot?;
        let e = s.sources.entries[i].as_ref()?;
        if !e.observed
            || !chain_live(s, i)
            || !s.sources.visits[..s.sources.visit_len]
                .iter()
                .flatten()
                .any(|v| v.root == e.weak)
        {
            return None;
        }
        Some(i)
    } else {
        None
    };
    if let Some(row) = s.entries.iter().position(|e| {
        e.as_ref()
            .is_some_and(|e| e.weak == *current && e.root.is_some() && !e.invalid)
    }) {
        return Some((row, parent));
    }
    let i = parent?;
    let e = s.sources.entries[i].as_ref()?;
    (e.weak == *current).then_some((e.row, Some(i)))
}
fn invalidate_where(s: &mut State, initial: impl Fn(&SourceEntry) -> bool) {
    let mut dead = [false; SOURCE_CAPACITY];
    for (i, e) in s.sources.entries.iter().enumerate() {
        dead[i] = e.as_ref().is_some_and(&initial);
    }
    for _ in 0..SOURCE_CAPACITY {
        let mut changed = false;
        for (i, e) in s.sources.entries.iter().enumerate() {
            if !dead[i]
                && e.as_ref()
                    .is_some_and(|e| e.parent.is_some_and(|p| dead[p]))
            {
                dead[i] = true;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    let mut any = false;
    for (i, e) in s.sources.entries.iter_mut().enumerate() {
        if dead[i]
            && let Some(e) = e
        {
            e.valid = false;
            any = true;
        }
    }
    for e in s.entries.iter_mut().flatten() {
        if e.witness
            .is_some_and(|w| w.source_slot.is_some_and(|i| dead[i]))
        {
            e.invalid = true;
        }
    }
    if any {
        s.reasons |= Reason::SourceChanged as u32;
    }
}
fn invalidate_nonce(s: &mut State, nonce: u64) {
    invalidate_where(s, |e| e.nonce == nonce);
}
pub(super) fn invalidate_owner(s: &mut State, owner: u64) {
    invalidate_where(s, |e| e.owner[0] == owner);
}
pub(super) fn check_liveness(s: &mut State) {
    invalidate_where(s, |e| !e.valid || !e.observed || e.weak.upgrade().is_none());
    for i in 0..CAPACITY {
        let bad = s.entries[i]
            .as_ref()
            .and_then(|e| e.witness)
            .and_then(|w| w.source_slot)
            .is_some_and(|slot| !chain_live(s, slot));
        if bad {
            s.entries[i].as_mut().unwrap().invalid = true;
            s.reasons |= Reason::WeakDead as u32;
        }
    }
}
pub(super) fn observe_root(
    s: &mut State,
    weak: &ItemWeak,
    geometry: Geometry,
    mask: u64,
    supported: bool,
) {
    let Some(i) = s.sources.scopes[..s.sources.scope_len]
        .last()
        .and_then(|v| v.slot)
    else {
        return;
    };
    let e = s.sources.entries[i].as_ref().unwrap();
    if e.weak != *weak {
        return;
    }
    let current = s.sources.visits[..s.sources.visit_len]
        .last()
        .and_then(Option::as_ref)
        .is_some_and(|v| v.root == e.weak);
    let valid = current
        && chain_live(s, i)
        && supported
        && geometry.supported()
        && mask & (1u64 << e.row) != 0;
    let e = s.sources.entries[i].as_mut().unwrap();
    if valid {
        e.observed = true;
    } else {
        e.valid = false;
        s.reasons |= Reason::SourceMismatch as u32;
    }
}
pub(super) fn witness_origin(
    s: &State,
    row: usize,
    root: &ItemWeak,
) -> Option<(u64, u64, Option<usize>)> {
    let visit = s.sources.visits[..s.sources.visit_len].last()?.as_ref()?;
    if visit.root != *root {
        return None;
    }
    let i = s.sources.scopes[..s.sources.scope_len].last()?.slot?;
    let e = s.sources.entries[i].as_ref()?;
    (e.row == row && e.weak == *root && e.observed && chain_live(s, i)).then_some((
        e.nonce,
        e.epoch,
        Some(i),
    ))
}
/// Scoped traversal component; no handles persist after its owning frame.
pub struct VisitGuard(Option<(u64, u64, u64)>);
impl VisitGuard {
    pub fn begin(item: &ItemRc) -> Self {
        if !active() {
            return Self(None);
        }
        let root = ItemRc::new_root(item.item_tree().clone()).downgrade();
        Self(STATE.with_borrow_mut(|s| {
            if s.sources.visit_len == VISIT_DEPTH {
                overflow(s);
                return None;
            }
            let token = next_token(s)?;
            let i = s.sources.visit_len;
            s.sources.visits[i] = Some(VisitSlot { token, root });
            s.sources.visit_len += 1;
            Some((s.render, s.session, token))
        }))
    }
}
impl Drop for VisitGuard {
    fn drop(&mut self) {
        let Some((render, session, token)) = self.0 else {
            return;
        };
        if !active() {
            return;
        }
        STATE.with_borrow_mut(|s| {
            if s.render != render || s.session != session {
                return;
            }
            let top = s.sources.visit_len.checked_sub(1);
            if top.is_some_and(|i| {
                s.sources.visits[i]
                    .as_ref()
                    .is_some_and(|v| v.token == token)
            }) {
                s.sources.visit_len -= 1;
                s.sources.visits[s.sources.visit_len] = None;
            } else {
                s.overflow = true;
                s.reasons |= Reason::SourceScopeOrder as u32;
            }
        });
    }
}
/// A guard never pops state belonging to another frame/session or later scope.
pub struct SourceGuard(Option<(u64, u64, u64)>);
impl Drop for SourceGuard {
    fn drop(&mut self) {
        let Some((render, session, token)) = self.0 else {
            return;
        };
        if !active() {
            return;
        }
        STATE.with_borrow_mut(|s| {
            if s.render != render || s.session != session {
                return;
            }
            let top = s.sources.scope_len.checked_sub(1);
            if top.is_some_and(|i| s.sources.scopes[i].token == token) {
                s.sources.scope_len -= 1;
                s.sources.scopes[s.sources.scope_len] = Default::default();
            } else {
                s.overflow = true;
                s.reasons |= Reason::SourceScopeOrder as u32;
            }
        });
    }
}

pub(super) fn source_scope_open(s: &State) -> bool {
    s.sources.scope_len != 0
}

pub(super) fn scopes_open(s: &State) -> bool {
    s.sources.visit_len != 0 || s.sources.scope_len != 0
}
#[cfg(test)]
pub(super) fn counts(s: &State) -> [usize; 3] {
    [
        s.sources.entries.iter().flatten().count(),
        s.sources.visit_len,
        s.sources.scope_len,
    ]
}

pub(super) fn key_live(s: &State, key: AnchorKey) -> bool {
    s.entries.iter().flatten().any(|e| {
        !e.invalid
            && e.weak.upgrade().is_some()
            && e.witness
                .is_some_and(|w| e.key(w) == key && w.source_slot.is_none_or(|i| chain_live(s, i)))
    })
}

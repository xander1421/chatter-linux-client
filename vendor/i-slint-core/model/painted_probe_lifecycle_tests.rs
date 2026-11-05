//! Private acceptance fixture. Metadata is registered only by real Repeater calls.
//! The delegate implements the same update/init/layout contract as generated Rust.
#![allow(unsafe_code)]
use super::*;
use crate::SharedString;
use crate::accessibility::{
    AccessibilityAction, AccessibleStringProperty, SupportedAccessibilityAction,
};
use crate::item_tree::{
    IndexRange, ItemRc, ItemTree, ItemTreeNode, ItemTreeWeak, ItemVisitorVTable, ItemWeak,
    VisitChildrenResult,
};
use crate::items::{AccessibleRole, ItemVTable, Rectangle};
use crate::layout::LayoutInfo;
use crate::lengths::{LogicalPoint, LogicalRect, LogicalSize};
use crate::model::VecModel;
use crate::painted_scroll_probe as probe;
use crate::slice::Slice;
use crate::window::WindowAdapterRc;
use std::boxed::Box;
use std::rc::Rc;
use vtable::{VRef, VRefMut};

#[derive(Default)]
struct Counts {
    creates: Cell<usize>,
    updates: Cell<usize>,
    inits: Cell<usize>,
    ensures: Cell<usize>,
    layouts: Cell<usize>,
    drops: Cell<usize>,
}
struct Delegate {
    root: Rectangle,
    width: Property<LogicalLength>,
    height: Property<LogicalLength>,
    y: Property<LogicalLength>,
    index: Property<i32>,
    data: Property<i32>,
    initialized: Cell<bool>,
    counts: Rc<Counts>,
    on_ensure: RefCell<Option<Box<dyn FnOnce()>>>,
}
impl Drop for Delegate {
    fn drop(&mut self) {
        self.counts.drops.set(self.counts.drops.get() + 1);
    }
}
impl Delegate {
    fn new(counts: &Rc<Counts>) -> ItemTreeRc<Self> {
        counts.creates.set(counts.creates.get() + 1);
        let d = vtable::VRc::new(Self {
            root: Rectangle::default(),
            width: Property::new(LogicalLength::new(100.)),
            height: Property::new(LogicalLength::new(10.)),
            y: Default::default(),
            index: Property::new(-1),
            data: Property::new(-1),
            initialized: Cell::new(false),
            counts: counts.clone(),
            on_ensure: Default::default(),
        });
        d
    }
    fn root(d: &ItemTreeRc<Self>) -> ItemRc {
        ItemRc::new_root(vtable::VRc::into_dyn(d.clone()))
    }
}
impl RepeatedItemTree for Delegate {
    type Data = i32;
    fn update(&self, index: usize, data: i32) {
        self.counts.updates.set(self.counts.updates.get() + 1);
        self.index.set(index as i32);
        self.data.set(data);
    }
    fn init(&self) {
        assert!(
            !self.initialized.replace(true),
            "init is once per materialization"
        );
        assert!(self.index.get_internal() >= 0, "update must precede init");
        self.counts.inits.set(self.counts.inits.get() + 1);
    }
    fn listview_layout(self: Pin<&Self>, offset_y: &mut LogicalLength) -> LogicalLength {
        self.counts.layouts.set(self.counts.layouts.get() + 1);
        self.y.set(*offset_y);
        *offset_y += unsafe { self.map_unchecked(|s| &s.height) }.get();
        LogicalLength::new(100.)
    }
}
const NODES: [ItemTreeNode; 1] = [ItemTreeNode::Item {
    is_accessible: false,
    children_count: 0,
    children_index: 1,
    parent_index: 0,
    item_array_index: 0,
}];
impl ItemTree for Delegate {
    fn visit_children_item(
        self: Pin<&Self>,
        _: isize,
        _: TraversalOrder,
        _: VRefMut<crate::item_tree::ItemVisitorVTable>,
    ) -> VisitChildrenResult {
        VisitChildrenResult::CONTINUE
    }
    fn get_item_ref(self: Pin<&Self>, index: u32) -> Pin<VRef<'_, ItemVTable>> {
        assert_eq!(index, 0);
        unsafe { Pin::new_unchecked(VRef::new(&self.get_ref().root)) }
    }
    fn get_subtree_range(self: Pin<&Self>, _: u32) -> IndexRange {
        (0..0).into()
    }
    fn get_subtree(self: Pin<&Self>, _: u32, _: usize, _: &mut ItemTreeWeak) {}
    fn get_item_tree(self: Pin<&Self>) -> Slice<'_, ItemTreeNode> {
        Slice::from_slice(&NODES)
    }
    fn parent_node(self: Pin<&Self>, _: &mut crate::item_tree::ItemWeak) {}
    fn embed_component(self: Pin<&Self>, _: &ItemTreeWeak, _: u32) -> bool {
        false
    }
    fn subtree_index(self: Pin<&Self>) -> usize {
        self.index.get_internal() as usize
    }
    fn layout_info(self: Pin<&Self>, _: Orientation) -> LayoutInfo {
        LayoutInfo::default()
    }
    fn ensure_instantiated(self: Pin<&Self>) -> bool {
        assert!(
            self.initialized.get(),
            "real delegate initialization precedes child ensure"
        );
        self.counts.ensures.set(self.counts.ensures.get() + 1);
        let callback = self.on_ensure.borrow_mut().take();
        if let Some(callback) = callback {
            callback();
        }
        false
    }
    fn item_geometry(self: Pin<&Self>, _: u32) -> LogicalRect {
        LogicalRect::new(
            LogicalPoint::new(0., self.y.get_internal().get()),
            LogicalSize::new(
                self.width.get_internal().get(),
                unsafe { self.map_unchecked(|s| &s.height) }.get().get(),
            ),
        )
    }
    fn accessible_role(self: Pin<&Self>, _: u32) -> AccessibleRole {
        Default::default()
    }
    fn accessible_string_property(
        self: Pin<&Self>,
        _: u32,
        _: AccessibleStringProperty,
        _: &mut SharedString,
    ) -> bool {
        false
    }
    fn accessibility_action(self: Pin<&Self>, _: u32, _: &AccessibilityAction) {}
    fn supported_accessibility_actions(self: Pin<&Self>, _: u32) -> SupportedAccessibilityAction {
        Default::default()
    }
    fn item_element_infos(self: Pin<&Self>, _: u32, _: &mut SharedString) -> bool {
        false
    }
    fn window_adapter(self: Pin<&Self>, _: bool, _: &mut Option<WindowAdapterRc>) {}
}
crate::item_tree::ItemTreeVTable_static!(static DELEGATE_VT for Delegate);

struct Fixture {
    repeater: Pin<Box<Repeater<Delegate>>>,
    model: Rc<VecModel<i32>>,
    current: Pin<Rc<Property<ModelRc<i32>>>>,
    counts: Rc<Counts>,
    y: Pin<Box<Property<LogicalLength>>>,
    h: Pin<Box<Property<LogicalLength>>>,
    cw: Pin<Box<Property<LogicalLength>>>,
    ch: Pin<Box<Property<LogicalLength>>>,
}
impl Fixture {
    fn new(rows: usize) -> Self {
        probe::cancel();
        let model = Rc::new(VecModel::from((0..rows as i32).collect::<Vec<_>>()));
        let current = Rc::pin(Property::new(ModelRc::from(model.clone())));
        let repeater = Box::pin(Repeater::<Delegate>::default());
        let c = current.clone();
        repeater.set_model_binding(move || c.as_ref().get());
        Self {
            repeater,
            model,
            current,
            counts: Rc::new(Counts::default()),
            y: Box::pin(Property::new(LogicalLength::new(0.))),
            h: Box::pin(Property::new(LogicalLength::new(30.))),
            cw: Box::pin(Default::default()),
            ch: Box::pin(Default::default()),
        }
    }
    fn ensure(&self) -> bool {
        self.repeater.as_ref().ensure_updated_listview(
            || Delegate::new(&self.counts),
            Some(self.cw.as_ref()),
            Some(self.ch.as_ref()),
            self.y.as_ref(),
            LogicalLength::new(100.),
            self.h.as_ref(),
        )
    }
    fn eager(&self) -> bool {
        self.repeater
            .as_ref()
            .ensure_updated(|| Delegate::new(&self.counts))
    }
    fn at(&self, index: usize) -> ItemTreeRc<Delegate> {
        self.repeater.instance_at(index).unwrap()
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Identity {
    owner: i64,
    epoch: i64,
    index: i64,
    binding: i64,
}
fn begin() -> probe::FrameGuard {
    probe::configure(true);
    probe::FrameGuard::begin(true, true)
}
fn geometry(y: f32) -> probe::Geometry {
    probe::Geometry {
        logical: [1., 0., 0., 1., 0., y],
        canvas: [1., 0., 0., 0., 1., y, 0., 0., 1.],
    }
}
fn paint(f: &Fixture) {
    for d in f.repeater.instances_vec() {
        let root = Delegate::root(&d);
        let weak = root.downgrade();
        let g = geometry(d.y.get_internal().get());
        let mask = probe::enter(&weak, g, 0, true);
        probe::witness(&weak, 0, mask, g, 1., 1, true);
    }
}
fn records(frame: &probe::FrameGuard) -> Vec<Identity> {
    let mut values = Vec::new();
    frame.commit(1, Some(7), true, |l, v| values.push((l, v)));
    assert_eq!(values.len() % 38, 0);
    values
        .chunks_exact(38)
        .map(|r| {
            for (i, (l, _)) in r.iter().enumerate() {
                assert_eq!(*l, probe::LABELS[i]);
            }
            assert_eq!(r[0].1, r[37].1);
            assert!(r[5].1 > 0);
            assert!(r[8].1 > 0);
            Identity {
                owner: r[5].1,
                epoch: r[6].1,
                index: r[7].1,
                binding: r[8].1,
            }
        })
        .collect()
}
fn observe(f: &Fixture) -> Vec<Identity> {
    let frame = begin();
    f.ensure();
    paint(f);
    records(&frame)
}
fn row(r: &[Identity], i: usize) -> Identity {
    *r.iter().find(|x| x.index == i as i64).unwrap()
}
fn changed(a: Identity, b: Identity) {
    assert_eq!(a.owner, b.owner);
    assert_ne!(a.epoch, b.epoch);
    assert_ne!(a.binding, b.binding);
}

#[test]
fn real_clean_listview_registers_after_trace_starts_without_create_rebind_or_layout() {
    let f = Fixture::new(100);
    assert!(f.ensure());
    let d = f.at(0);
    let weak = Delegate::root(&d).downgrade();
    drop(d);
    let before = (
        f.counts.creates.get(),
        f.counts.updates.get(),
        f.counts.inits.get(),
        f.counts.layouts.get(),
    );
    let a = observe(&f);
    let b = observe(&f);
    assert_eq!(a, b);
    assert_eq!(a.len(), 3);
    assert_eq!(
        before,
        (
            f.counts.creates.get(),
            f.counts.updates.get(),
            f.counts.inits.get(),
            f.counts.layouts.get()
        )
    );
    assert!(f.counts.ensures.get() > 6);
    assert!(weak.upgrade().is_some());
}
#[test]
fn real_row_changed_direct_rebind_invalidates_painted_frame_and_reregisters_clean() {
    let f = Fixture::new(100);
    let a = observe(&f);
    let same = f.at(1);
    let updates = f.counts.updates.get();
    let frame = begin();
    assert!(!f.ensure());
    paint(&f);
    f.model.set_row_data(1, 900);
    assert_eq!(same.data.get_internal(), 900);
    assert_eq!(f.counts.updates.get(), updates + 1);
    assert!(records(&frame).is_empty());
    assert!(!f.ensure());
    paint(&f);
    let b = records(&frame);
    changed(row(&a, 1), row(&b, 1));
    assert_eq!(Delegate::root(&same), Delegate::root(&f.at(1)));
}
#[test]
fn real_before_visible_prepend_and_remove_early_returns_end_all_continuity() {
    let f = Fixture::new(100);
    f.y.as_ref().set(LogicalLength::new(-200.));
    let a = observe(&f);
    assert_eq!(f.repeater.range(), 20..23);
    let retained = f.at(20);
    let frame = begin();
    f.ensure();
    paint(&f);
    f.model.insert(0, 700);
    assert!(records(&frame).is_empty());
    assert_eq!(f.repeater.range(), 21..24);
    f.ensure();
    paint(&f);
    let b = records(&frame);
    changed(row(&a, 20), row(&b, 21));
    assert_eq!(retained.index.get_internal(), 21);
    assert_eq!(retained.data.get_internal(), 20);
    drop(frame);
    let frame = begin();
    f.ensure();
    paint(&f);
    f.model.remove(0);
    assert!(records(&frame).is_empty());
    f.ensure();
    paint(&f);
    let c = records(&frame);
    changed(row(&b, 21), row(&c, 20));
    assert_eq!(retained.index.get_internal(), 20);
}
#[test]
fn real_outside_view_add_remove_and_row_changed_still_end_epoch() {
    let f = Fixture::new(100);
    let a = observe(&f);
    let frame = begin();
    f.ensure();
    paint(&f);
    f.model.push(1000);
    assert!(records(&frame).is_empty());
    f.ensure();
    paint(&f);
    let b = records(&frame);
    changed(row(&a, 0), row(&b, 0));
    drop(frame);
    f.model.remove(100);
    let c = observe(&f);
    changed(row(&b, 0), row(&c, 0));
    f.model.set_row_data(99, 222);
    let d = observe(&f);
    changed(row(&c, 0), row(&d, 0));
}
#[test]
fn real_in_view_insert_remove_reset_and_model_replacement() {
    let f = Fixture::new(100);
    let a = observe(&f);
    let old = f.at(1);
    let oldweak = Delegate::root(&old).downgrade();
    f.model.insert(1, 555);
    let b = observe(&f);
    changed(row(&a, 1), row(&b, 1));
    assert_eq!(f.at(1).data.get_internal(), 555);
    assert_eq!(old.index.get_internal(), 2);
    f.model.remove(1);
    let c = observe(&f);
    changed(row(&b, 1), row(&c, 1));
    assert_eq!(Delegate::root(&old), Delegate::root(&f.at(1)));
    let frame = begin();
    f.ensure();
    paint(&f);
    f.model.set_vec((1000..1100).collect::<Vec<_>>());
    assert!(records(&frame).is_empty());
    f.ensure();
    paint(&f);
    let d = records(&frame);
    changed(row(&c, 1), row(&d, 1));
    assert_ne!(Delegate::root(&old), Delegate::root(&f.at(1)));
    assert!(oldweak.upgrade().is_some());
    drop(frame);
    let new = Rc::new(VecModel::from((2000..2100).collect::<Vec<_>>()));
    f.current.set(ModelRc::from(new.clone()));
    let e = observe(&f);
    changed(row(&d, 1), row(&e, 1));
    assert_eq!(f.at(1).data.get_internal(), 2001);
    f.model.set_row_data(1, 777);
    let same = observe(&f);
    assert_eq!(e, same, "old model peer was detached");
}
#[test]
fn real_virtualization_new_materialization_changes_owner_binding_only() {
    let f = Fixture::new(100);
    let a = observe(&f);
    let retained = f.at(1);
    f.y.as_ref().set(LogicalLength::new(-10.));
    let b = observe(&f);
    let old = row(&a, 1);
    let new = row(&b, 1);
    assert_eq!(old.owner, new.owner);
    assert_eq!(old.epoch, new.epoch);
    assert_ne!(old.binding, new.binding);
    assert_eq!(Delegate::root(&retained), Delegate::root(&f.at(1)));
    assert_eq!(f.repeater.range(), 1..4);
}
#[test]
fn real_virtualization_removal_only_changes_owner_binding_conservatively() {
    let f = Fixture::new(100);
    let a = observe(&f);
    let retained = f.at(0);
    f.h.as_ref().set(LogicalLength::new(10.));
    let b = observe(&f);
    assert_eq!(f.repeater.range(), 0..1);
    let old = row(&a, 0);
    let new = row(&b, 0);
    assert_eq!(old.owner, new.owner);
    assert_eq!(old.epoch, new.epoch);
    assert_ne!(
        old.binding, new.binding,
        "removal-only virtualization must end the owner binding interval"
    );
    assert_eq!(Delegate::root(&retained), Delegate::root(&f.at(0)));
}
#[test]
fn real_reset_empty_repopulate_and_eager_repeaters() {
    let f = Fixture::new(3);
    let frame = begin();
    assert!(f.eager());
    paint(&f);
    let a = records(&frame);
    assert_eq!(a.len(), 3);
    drop(frame);
    f.model.clear();
    let frame = begin();
    assert!(f.eager());
    paint(&f);
    assert!(records(&frame).is_empty());
    drop(frame);
    f.model.extend([11, 12, 13]);
    let frame = begin();
    assert!(f.eager());
    paint(&f);
    let b = records(&frame);
    changed(row(&a, 1), row(&b, 1));
    drop(frame);
    let frame = begin();
    assert!(!f.eager());
    paint(&f);
    assert_eq!(b, records(&frame));
}
#[test]
fn real_owner_drop_invalidates_even_with_retained_delegate_and_next_owner_is_distinct() {
    let mut f = Fixture::new(3);
    let a = observe(&f);
    let d = f.at(0);
    let weak = Delegate::root(&d).downgrade();
    let frame = begin();
    f.ensure();
    paint(&f);
    let old = std::mem::replace(&mut f.repeater, Box::pin(Repeater::default()));
    drop(old);
    assert!(weak.upgrade().is_some());
    assert!(records(&frame).is_empty());
    let m = f.current.clone();
    f.repeater.set_model_binding(move || m.as_ref().get());
    f.ensure();
    paint(&f);
    let b = records(&frame);
    assert_ne!(row(&a, 0).owner, row(&b, 0).owner);
    drop(frame);
    drop(f);
    assert!(weak.upgrade().is_some());
    drop(d);
    assert!(weak.upgrade().is_none());
}
#[test]
fn real_child_ensure_model_mutation_after_registration_invalidates_current_frame() {
    let f = Fixture::new(3);
    let a = observe(&f);
    let model = f.model.clone();
    f.at(0)
        .on_ensure
        .replace(Some(Box::new(move || model.set_row_data(1, 901))));
    let frame = begin();
    f.ensure();
    paint(&f);
    assert!(
        records(&frame).is_empty(),
        "mutation after final-pass registration stays invalid until a new ensure"
    );
    f.ensure();
    paint(&f);
    let b = records(&frame);
    changed(row(&a, 1), row(&b, 1));
}

struct ReuseSlot {
    slot: Box<core::mem::MaybeUninit<Repeater<Delegate>>>,
}
impl ReuseSlot {
    fn new() -> Self {
        let mut slot = Box::new(core::mem::MaybeUninit::uninit());
        slot.write(Repeater::default());
        Self { slot }
    }
    fn pin(&self) -> Pin<&Repeater<Delegate>> {
        unsafe { Pin::new_unchecked(self.slot.assume_init_ref()) }
    }
    fn replace_in_place(&mut self) {
        // No Pin/borrow survives this call. Run pinned Drop before reusing the allocation.
        unsafe {
            core::ptr::drop_in_place(self.slot.as_mut_ptr());
            self.slot.as_mut_ptr().write(Repeater::default());
        }
    }
}
impl Drop for ReuseSlot {
    fn drop(&mut self) {
        unsafe {
            core::ptr::drop_in_place(self.slot.as_mut_ptr());
        }
    }
}
fn paint_repeater(repeater: &Repeater<Delegate>) {
    for d in repeater.instances_vec() {
        let weak = Delegate::root(&d).downgrade();
        let g = geometry(d.y.get_internal().get());
        let mask = probe::enter(&weak, g, 0, true);
        probe::witness(&weak, 0, mask, g, 1., 1, true);
    }
}
#[test]
fn real_owner_storage_address_reuse_is_deterministic_and_never_reuses_anonymous_id() {
    probe::cancel();
    let mut slot = ReuseSlot::new();
    let counts = Rc::new(Counts::default());
    let old_model = Rc::new(VecModel::from([10, 11, 12].to_vec()));
    let m = old_model.clone();
    slot.pin()
        .set_model_binding(move || ModelRc::from(m.clone()));
    let frame = begin();
    slot.pin().ensure_updated(|| Delegate::new(&counts));
    paint_repeater(&slot.pin());
    let a = records(&frame);
    let held = slot.pin().instance_at(0).unwrap();
    let weak = Delegate::root(&held).downgrade();
    let address = slot.pin().get_ref() as *const Repeater<Delegate>;
    slot.replace_in_place();
    assert_eq!(address, slot.pin().get_ref() as *const Repeater<Delegate>);
    assert!(records(&frame).is_empty());
    assert!(
        weak.upgrade().is_some(),
        "fixture keeps old delegate live across owner drop"
    );
    let new_model = Rc::new(VecModel::from([20, 21, 22].to_vec()));
    let m = new_model.clone();
    slot.pin()
        .set_model_binding(move || ModelRc::from(m.clone()));
    slot.pin().ensure_updated(|| Delegate::new(&counts));
    paint_repeater(&slot.pin());
    let b = records(&frame);
    assert_ne!(row(&a, 0).owner, row(&b, 0).owner);
    assert_ne!(row(&a, 0).binding, row(&b, 0).binding);
    old_model.set_row_data(0, 55);
    assert_eq!(
        b,
        records(&frame),
        "old notification peer detached before storage was reused"
    );
    drop(held);
    assert!(
        weak.upgrade().is_none(),
        "probe registry retains only a weak delegate handle"
    );
    assert_eq!(
        counts.drops.get(),
        3,
        "all old delegate values dropped despite the live frame"
    );
}
#[test]
fn real_reset_drops_delegate_values_even_while_frame_registry_holds_weak_handles() {
    let f = Fixture::new(3);
    let frame = begin();
    f.ensure();
    paint(&f);
    let weak = Delegate::root(&f.at(0)).downgrade();
    f.model.clear();
    assert_eq!(f.counts.drops.get(), 3);
    assert!(weak.upgrade().is_none());
    assert!(records(&frame).is_empty());
    f.ensure();
    assert!(f.repeater.is_empty());
    assert!(records(&frame).is_empty());
}
#[test]
fn real_two_repeaters_use_distinct_anonymous_owners_with_independent_notification_epochs() {
    let f = Fixture::new(3);
    let g = Fixture::new(3);
    // Two rows per owner fit the diagnostic's four-anchor output budget.
    f.h.as_ref().set(LogicalLength::new(20.));
    g.h.as_ref().set(LogicalLength::new(20.));
    let frame = begin();
    f.ensure();
    g.ensure();
    paint(&f);
    paint(&g);
    let a = records(&frame);
    assert_eq!(a.len(), 4);
    assert_ne!(a[0].owner, a[2].owner);
    let other = a[2..].to_vec();
    f.model.set_row_data(0, 500);
    let remaining = records(&frame);
    assert_eq!(
        other, remaining,
        "one owner mutation does not invalidate the other repeater"
    );
}
#[test]
fn real_removal_only_invalidates_already_painted_removed_delegate_even_if_still_strongly_held() {
    let f = Fixture::new(100);
    let frame = begin();
    f.ensure();
    paint(&f);
    let a = records(&frame);
    assert_eq!(a.len(), 3);
    let removed = f.at(2);
    let weak = Delegate::root(&removed).downgrade();
    f.h.as_ref().set(LogicalLength::new(10.));
    f.ensure();
    assert_eq!(f.repeater.range(), 0..1);
    assert!(weak.upgrade().is_some());
    assert!(
        records(&frame).is_empty(),
        "removed delegate witness must be invalidated even if its allocation remains live"
    );
    paint(&f);
    let b = records(&frame);
    assert_eq!(b.len(), 1);
    assert_ne!(row(&a, 0).binding, row(&b, 0).binding);
}
#[test]
fn real_row_changed_with_dirty_equal_model_binding_updates_delegate_data() {
    let f = Fixture::new(3);
    let a = observe(&f);
    let trigger = Rc::pin(Property::new(false));
    let t = trigger.clone();
    let m = f.model.clone();
    f.repeater.set_model_binding(move || {
        let _ = t.as_ref().get();
        ModelRc::from(m.clone())
    });
    f.ensure();
    trigger.as_ref().set(true);
    assert!(f.repeater.as_ref().data().project_ref().model.is_dirty());
    f.model.set_row_data(1, 902);
    f.ensure();
    assert_eq!(
        f.at(1).data.get_internal(),
        902,
        "dirty same-identity model binding must not lose a row_changed notification"
    );
    let b = observe(&f);
    changed(row(&a, 1), row(&b, 1));
}
#[test]
fn real_eager_row_changed_with_dirty_equal_model_binding_updates_delegate_data() {
    let f = Fixture::new(3);
    f.eager();
    let trigger = Rc::pin(Property::new(false));
    let t = trigger.clone();
    let m = f.model.clone();
    f.repeater.set_model_binding(move || {
        let _ = t.as_ref().get();
        ModelRc::from(m.clone())
    });
    f.eager();
    trigger.as_ref().set(true);
    f.model.set_row_data(1, 903);
    f.eager();
    assert_eq!(
        f.at(1).data.get_internal(),
        903,
        "eager materialization must consume a row_changed while its equal model binding is dirty"
    );
}


#[test]
fn real_v15_availability_clean_reuse_keeps_first_evidence_and_counts() {
    let f=Fixture::new(100);let frame=begin();f.ensure();paint(&f);
    let first=records(&frame);assert_eq!(first.len(),3);
    assert_eq!(frame.availability_snapshot().unwrap()[7..13],[3,0,3,3,0,3]);
    let counts=(f.counts.creates.get(),f.counts.updates.get(),f.counts.layouts.get());
    assert!(!f.ensure());
    assert_eq!(records(&frame),first);
    assert_eq!(counts,(f.counts.creates.get(),f.counts.updates.get(),f.counts.layouts.get()));
    f.model.set_row_data(1,777);
    assert!(records(&frame).is_empty());assert_eq!(frame.availability_snapshot().unwrap()[11],3);
    f.ensure();paint(&f);let next=records(&frame);assert_eq!(next.len(),3);
    assert_ne!(next[0].binding,first[0].binding);
    assert_eq!(frame.availability_snapshot().unwrap()[7..13],[3,0,3,3,0,3]);
}

#[test]
fn real_v15_eager_capacity_overflow_reports_capped_registry_and_zero_output() {
    let f=Fixture::new(65);let frame=begin();f.eager();paint(&f);
    assert_eq!(f.repeater.instances_vec().len(),65);assert!(records(&frame).is_empty());
    assert_eq!(frame.availability_snapshot().unwrap()[7..13],[64,1,64,64,0,0]);
    drop(frame);
    let frame=begin();f.model.set_vec((0..3).collect::<Vec<_>>());f.eager();paint(&f);
    assert_eq!(records(&frame).len(),3);assert_eq!(frame.availability_snapshot().unwrap()[7..13],[3,0,3,3,0,3]);
}

#[test]
fn real_v15_empty_repeater_has_a_complete_zero_availability_record() {
    let f=Fixture::new(0);let frame=begin();f.eager();
    assert!(records(&frame).is_empty());
    let mut out=Vec::new();frame.report_availability(|l,v|out.push((l,v)));
    assert_eq!(out.len(),15);for (i,(l,_)) in out.iter().enumerate(){assert_eq!(*l,probe::AVAILABILITY_LABELS[i]);}
    assert_eq!(out[0].1,out[14].1);assert_eq!(out[3].1,1);
    assert_eq!(frame.availability_snapshot().unwrap()[7..13],[0,0,0,0,0,0]);
}

#[test]
fn real_v15_same_binding_geometry_change_rejects_the_old_first_root_basis() {
    let f=Fixture::new(10);let frame=begin();f.ensure();paint(&f);assert_eq!(records(&frame).len(),3);
    let retained=f.at(1);let weak=Delegate::root(&retained).downgrade();
    let changed_y=retained.y.get_internal().get()+1.;
    let mask=probe::enter(&weak,geometry(changed_y),0,true);
    probe::witness(&weak,0,mask,geometry(changed_y),1.,1,true);
    let out=records(&frame);assert_eq!(out.len(),2);assert!(!out.iter().any(|x|x.index==1));
    assert_eq!(frame.availability_snapshot().unwrap()[11],1);
    assert_ne!(frame.availability_snapshot().unwrap()[13]&probe::Reason::RootGeometryChanged as i64,0);
}

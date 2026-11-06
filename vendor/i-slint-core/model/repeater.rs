// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

//! This module contains the [`Repeater`] and [`Conditional`] types that are
//! used by generated code to instantiate items from a model using the `for`
//! syntax, and [`RepeatedItemTree`] which is the trait implemented by the
//! generated repeated components.
//!
//! The [`RepeaterInstanceOps`] trait abstracts over instance storage so the
//! update algorithm can be shared between Rust and C++ (via FFI).

use super::model_peer::{ModelChangeListener, ModelChangeListenerContainer};
use super::{Model, ModelExt, ModelRc};
use crate::android_trace::{
    Counter as TraceCounter, Identity as TraceIdentity, Section as TraceSection,
};
use crate::item_tree::{ItemTreeVTable, TraversalOrder};
use crate::layout::Orientation;
use crate::lengths::{LogicalLength, RectLengths};
use crate::{Coord, Property};
use alloc::vec::Vec;
use core::cell::{Cell, RefCell};
use core::pin::Pin;
#[allow(unused)]
use euclid::num::Floor;
use pin_project::pin_project;

type ItemTreeRc<C> = vtable::VRc<crate::item_tree::ItemTreeVTable, C>;

// Counts include every Rust repeater, not only the message ListView. Nested
// sections identify the list update that caused the work; never label by data.
static TRACE_UPDATES: TraceCounter = TraceCounter::new(c"Slint listview updates");
static TRACE_PASSES: TraceCounter = TraceCounter::new(c"Slint listview layout passes");
static TRACE_LAYOUTS: TraceCounter = TraceCounter::new(c"Slint delegate layouts");
static TRACE_CREATES: TraceCounter = TraceCounter::new(c"Slint delegate creates");
static TRACE_REMOVES: TraceCounter = TraceCounter::new(c"Slint delegate slots removed");
static TRACE_DROPS: TraceCounter = TraceCounter::new(c"Slint delegate refs dropped");
static TRACE_REBINDS: TraceCounter = TraceCounter::new(c"Slint dirty row rebinds");
static TRACE_READS: TraceCounter = TraceCounter::new(c"Slint repeater row_data reads");

fn traced_row_data<T: 'static>(model: &ModelRc<T>, row: usize) -> Option<T> {
    let trace = TraceSection::new(c"Slint model row_data");
    trace.count(&TRACE_READS, 1);
    trace.counter(c"Slint model row", row as i64);
    model.row_data(row)
}

/// Observe releases without retaining another component or reading model data.
/// The row range is half-open. A dropped VRc is not proof that its pointee was
/// destroyed: another owner may still hold a strong reference.
fn trace_removed_instances<C: RepeatedItemTree>(
    instances: &[(RepeatedInstanceState, Option<ItemTreeRc<C>>)],
    first_row: usize,
    identity: &TraceIdentity,
) -> TraceSection {
    let trace = TraceSection::new(c"Slint delegate remove");
    trace.identify(identity);
    if trace.enabled() {
        trace.count(&TRACE_REMOVES, instances.len());
        trace.count(
            &TRACE_DROPS,
            instances.iter().filter(|(_, c)| c.is_some()).count(),
        );
        trace.counter(c"Slint removed row begin", first_row as i64);
        trace.counter(
            c"Slint removed row end",
            first_row.saturating_add(instances.len()) as i64,
        );
    }
    trace
}

/// ItemTree that can be instantiated by a repeater.
pub trait RepeatedItemTree:
    crate::item_tree::ItemTree + vtable::HasStaticVTable<ItemTreeVTable> + 'static
{
    /// The data corresponding to the model
    type Data: Default + 'static;

    /// Update this ItemTree at the given index and the given data
    fn update(&self, index: usize, data: Self::Data);

    /// Called once after the ItemTree has been instantiated and update()
    /// was called once.
    fn init(&self) {}

    /// Layout this item in the listview
    ///
    /// offset_y is the `y` position where this item should be placed.
    /// it should be updated to be to the y position of the next item.
    ///
    /// Returns the minimum item width which will be used to compute the listview's content width
    fn listview_layout(self: Pin<&Self>, _offset_y: &mut LogicalLength) -> LogicalLength {
        LogicalLength::default()
    }

    /// Returns what's needed to perform the layout if this ItemTree is in a layout
    /// In case of repeated Rows, the index of a child item is set
    fn layout_item_info(
        self: Pin<&Self>,
        _orientation: Orientation,
        _child_index: Option<usize>,
    ) -> crate::layout::LayoutItemInfo {
        crate::layout::LayoutItemInfo::default()
    }

    /// Vertical layout info measured at the given cross-axis (container) width.
    /// A box layout calls this so a height-for-width instance wraps to the
    /// real width. The default ignores the width (non-height-for-width cells);
    /// the generated code overrides it for height-for-width instances.
    fn layout_item_info_at_cross_width(
        self: Pin<&Self>,
        _cross_width: f32,
    ) -> crate::layout::LayoutItemInfo {
        self.layout_item_info(Orientation::Vertical, None)
    }

    /// Returns what's needed to perform a flexbox layout if this ItemTree is in a FlexboxLayout.
    /// Includes flex-specific properties (layout-order).
    fn flexbox_layout_item_info(
        self: Pin<&Self>,
        orientation: Orientation,
        child_index: Option<usize>,
    ) -> crate::layout::FlexboxLayoutItemInfo {
        self.layout_item_info(orientation, child_index).into()
    }

    /// Vertical flexbox info measured at the given cross-axis (container) width.
    /// A column FlexboxLayout calls this so a height-for-width instance wraps to
    /// the real width. The default ignores the width (non-height-for-width
    /// cells); the generated code overrides it for height-for-width instances.
    fn flexbox_layout_item_info_at_cross_width(
        self: Pin<&Self>,
        _cross_width: f32,
    ) -> crate::layout::FlexboxLayoutItemInfo {
        self.flexbox_layout_item_info(Orientation::Vertical, None)
    }

    /// Fills in the grid layout input data for this ItemTree if it is in a grid layout.
    /// This will be a single GridLayoutInputData if the repeated item is a single cell,
    /// or multiple GridLayoutInputData if the repeated item is a full Row.
    /// The slice must have the exact size required (known at compile time).
    fn grid_layout_input_data(
        self: Pin<&Self>,
        _new_row: bool,
        _result: &mut [crate::layout::GridLayoutInputData],
    ) {
        crate::debug_log!(
            "Internal error in Slint: RepeatedItemTree::grid_layout_input_data() not implemented for {}",
            core::any::type_name::<Self>()
        );
        // the actual implementation is in the code generated by generate_repeated_component()
    }

    /// The z value used to sort this instance among the other instances of the repeater,
    /// when the repeated element has a dynamic z binding
    fn z_order(self: Pin<&Self>) -> Option<f32> {
        None
    }
}

// Keep the concrete type available so empty generated ensure methods can inline.
// FFI vtables retain their extern "C" boundary, including its unwind behavior.
#[inline]
fn ensure_repeated_item_tree_instantiated<C: RepeatedItemTree>(instance: ItemTreeRc<C>) -> bool {
    #[cfg(not(feature = "ffi"))]
    {
        instance.as_pin_ref().ensure_instantiated()
    }
    #[cfg(feature = "ffi")]
    {
        crate::item_tree::ensure_item_tree_instantiated(&vtable::VRc::into_dyn(instance))
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum RepeatedInstanceState {
    /// The item is in a clean state
    Clean,
    /// The model data is stale and needs to be refreshed
    Dirty,
}

// Keep short-lived snapshots of small repeaters on the stack. All handles must
// be cloned before invoking any child: callbacks may re-enter this repeater or
// replace its model. Neither storage may outlive the current traversal.
enum ChildSnapshot<T> {
    Inline([Option<T>; 4]),
    Heap(Vec<T>),
}

impl<T> ChildSnapshot<T> {
    fn visit(self, mut callback: impl FnMut(T) -> bool) -> bool {
        let mut changed = false;
        match self {
            Self::Inline(inline) => {
                for instance in inline.into_iter().flatten() {
                    changed |= callback(instance);
                }
            }
            Self::Heap(heap) => {
                for instance in heap {
                    changed |= callback(instance);
                }
            }
        }
        changed
    }
}

fn child_snapshot<T>(instances: impl ExactSizeIterator<Item = Option<T>>) -> ChildSnapshot<T> {
    let mut inline = [const { None }; 4];
    if instances.len() <= inline.len() {
        for (slot, instance) in inline.iter_mut().zip(instances) {
            *slot = instance;
        }
        ChildSnapshot::Inline(inline)
    } else {
        // Unlike filter_map's size hint, the slot count bounds the snapshot.
        // Reserve once instead of repeatedly growing the vector for live rows.
        let mut heap = Vec::with_capacity(instances.len());
        heap.extend(instances.flatten());
        ChildSnapshot::Heap(heap)
    }
}

struct RepeaterInner<C: RepeatedItemTree> {
    instances: Vec<(RepeatedInstanceState, Option<ItemTreeRc<C>>)>,
    trace_identity: TraceIdentity,
    /// ListView-specific layout state (offset, cached heights, scroll position).
    layout_state: RepeaterLayoutState,
}

impl<C: RepeatedItemTree> Default for RepeaterInner<C> {
    fn default() -> Self {
        RepeaterInner {
            instances: Default::default(),
            trace_identity: Default::default(),
            layout_state: Default::default(),
        }
    }
}

impl<C: RepeatedItemTree> Drop for RepeaterInner<C> {
    fn drop(&mut self) {
        let trace = TraceSection::new(c"Slint repeater drop");
        trace.identify(&self.trace_identity);
        let _remove = trace_removed_instances(
            &self.instances,
            self.layout_state.offset,
            &self.trace_identity,
        );
        // Same release order as the implicit field drop, enclosed by the trace.
        self.instances.clear();
    }
}

/// Persistent layout state for a ListView repeater.
#[derive(Default, Clone, Debug)]
#[repr(C)]
pub struct RepeaterLayoutState {
    /// The model row index of the first instance in the collection.
    pub offset: usize,
    /// The average visible item height (cached between frames).
    pub cached_item_height: Coord,
    /// The content_y value from the previous layout pass.
    /// It is used to detect if we are scrolling up or down
    pub previous_content_y: Coord,
    /// The y position of the item at `offset`.
    pub anchor_y: Coord,
}

/// Abstraction over a repeater's instance collection so the same algorithm
/// works for both native Rust repeaters and C++ repeaters via FFI.
trait RepeaterInstanceOps {
    /// Number of currently instantiated items.
    fn len(&self) -> usize;

    /// Replace the range `position..position+remove` with `add` new empty/dirty slots.
    // first_row is observational only: row of the first removed/added slot.
    fn splice(&mut self, position: usize, remove: usize, add: usize, first_row: usize);

    /// If dirty, ensure the instance is created, initialized, and updated
    /// for `row`. Returns `true` if freshly created.
    fn ensure_updated(&mut self, instance_idx: usize, row: usize) -> bool;

    /// Height of the instance, or `None` if not yet created.
    fn height(&self, instance_idx: usize) -> Option<Coord>;

    /// Call `listview_layout` on the instance.
    /// Advances `*y` to the next item position. Returns item width.
    fn listview_layout(&self, instance_idx: usize, y: &mut Coord, row: usize) -> Coord;
}

/// More rows than this is presumably a bug (e.g. a division by zero) and would run out of memory
const MAX_EAGER_INSTANCE_COUNT: usize = 1 << 20;

/// Update all instances in the repeater, creating any that are missing.
fn update_all_instances(ops: &mut impl RepeaterInstanceOps, offset: usize, count: usize) {
    let count = if count > MAX_EAGER_INSTANCE_COUNT {
        crate::debug_log!("A repeater's model has {count} rows: too many to instantiate");
        0
    } else {
        count
    };
    let cur = ops.len();
    if count > cur {
        ops.splice(cur, 0, count - cur, offset + cur);
    } else if count < cur {
        ops.splice(count, cur - count, 0, offset + count);
    }
    for i in 0..count {
        ops.ensure_updated(i, i + offset);
    }
}

/// Access to the ListView content properties.
///
/// `update_visible_instances` reads and writes the content geometry at
/// several points; this trait abstracts whether the storage is a
/// strongly-typed `Property<LogicalLength>` (rust and C++ generated code,
/// see `TypedListViewProps`) or another backing such as the interpreter's
/// `Property<Value>`, or a native-item property accessed through rtti.
pub trait ListViewProperties {
    fn content_y_get(&self) -> LogicalLength;
    /// Read `content-y` without evaluating a binding on it
    /// (see [`Property::get_internal`]).
    fn content_y_get_internal(&self) -> LogicalLength;
    fn content_y_set(&self, value: LogicalLength);
    fn content_y_has_binding(&self) -> bool;
    /// True when the ListView computes `content-height` from the rows; false
    /// when the user explicitly sets it, so it doesn't track the rows and the
    /// past-the-end seek heuristic must not rely on it.
    fn computes_content_height(&self) -> bool;
    /// Set `content-width`. A no-op when the user explicitly sets it.
    fn content_width_set(&self, value: LogicalLength);
    /// Set `content-height`. A no-op when [`Self::computes_content_height`] is false.
    fn content_height_set(&self, value: LogicalLength);
    /// Register the content properties as dependencies of the current
    /// binding evaluation without reading them.
    fn register_as_dependencies(&self);
}

struct TypedListViewProps<'a> {
    content_width: Option<Pin<&'a Property<LogicalLength>>>,
    content_height: Option<Pin<&'a Property<LogicalLength>>>,
    content_y: Pin<&'a Property<LogicalLength>>,
}

impl ListViewProperties for TypedListViewProps<'_> {
    fn content_y_get(&self) -> LogicalLength {
        self.content_y.get()
    }
    fn content_y_get_internal(&self) -> LogicalLength {
        self.content_y.get_internal()
    }
    fn content_y_set(&self, value: LogicalLength) {
        self.content_y.set(value);
    }
    fn content_y_has_binding(&self) -> bool {
        self.content_y.has_binding()
    }
    fn computes_content_height(&self) -> bool {
        self.content_height.is_some()
    }
    fn content_width_set(&self, value: LogicalLength) {
        if let Some(content_width) = self.content_width {
            content_width.set(value);
        }
    }
    fn content_height_set(&self, value: LogicalLength) {
        if let Some(content_height) = self.content_height {
            content_height.set(value);
        }
    }
    fn register_as_dependencies(&self) {
        if let Some(content_width) = self.content_width {
            content_width.register_as_dependency();
        }
        if let Some(content_height) = self.content_height {
            content_height.register_as_dependency();
        }
        self.content_y.register_as_dependency();
    }
}

/// Update only the instances visible in the ListView viewport.
///
/// This is the core virtualization algorithm: it estimates which model rows
/// are visible, instantiates/updates those, lays them out, and cleans up
/// off-screen instances. Returns whether any instance was created.
fn update_visible_instances(
    ops: &mut impl RepeaterInstanceOps,
    state: &mut RepeaterLayoutState,
    row_count: usize,
    props: &dyn ListViewProperties,
    listview_width: LogicalLength,
    listview_height: LogicalLength,
) -> bool {
    let trace = TraceSection::new(c"Slint listview virtualize");
    if trace.enabled() {
        trace.counter(c"Slint listview model rows", row_count as i64);
        trace.counter(c"Slint listview range before begin", state.offset as i64);
        trace.counter(
            c"Slint listview range before end",
            state.offset.saturating_add(ops.len()) as i64,
        );
    }
    let zero = LogicalLength::default();
    let mut content_width_value = listview_width.get();
    let listview_height = listview_height.get();

    if row_count == 0 {
        ops.splice(0, ops.len(), 0, state.offset);
        props.content_height_set(zero);
        props.content_y_set(zero);
        props.content_width_set(listview_width);
        trace.counter(c"Slint listview range after begin", state.offset as i64);
        trace.counter(c"Slint listview range after end", state.offset as i64);
        return false;
    }

    let mut content_y_value = props.content_y_get().get();
    if trace.enabled() {
        trace.counter(
            c"Slint listview viewport y mpx",
            (content_y_value as f64 * 1000.) as i64,
        );
        trace.counter(
            c"Slint listview height mpx",
            (listview_height as f64 * 1000.) as i64,
        );
    }
    if !props.content_y_has_binding() {
        content_y_value = content_y_value.min(0 as Coord);
    }

    let mut changed = false;

    // Estimate element height from cached value or by measuring existing instances.
    let element_height = if state.cached_item_height > 0 as Coord {
        state.cached_item_height
    } else {
        let mut total_height: Coord = 0 as Coord;
        let mut count = 0usize;
        for i in 0..ops.len() {
            if let Some(h) = ops.height(i) {
                total_height += h;
                count += 1;
            }
        }

        if count > 0 {
            total_height / count as Coord
        } else {
            // No items exist yet. Create one to measure.
            state.offset = state.offset.min(row_count - 1);
            let _measure = TraceSection::new(c"Slint listview seed measure");
            ops.splice(0, ops.len(), 1, state.offset);
            changed |= ops.ensure_updated(0, state.offset);
            ops.height(0).unwrap_or(0 as Coord)
        }
    };
    if trace.enabled() {
        trace.counter(
            c"Slint listview estimated row height mpx",
            (element_height as f64 * 1000.) as i64,
        );
    }

    if state.offset >= row_count {
        state.offset = row_count - 1;
    }

    let one_and_a_half_screen = listview_height * 3 as Coord / 2 as Coord;
    let first_item_y = state.anchor_y;
    let last_item_bottom = first_item_y + element_height * ops.len() as Coord;

    let (mut new_offset, mut new_offset_y) = if first_item_y
        > -content_y_value + one_and_a_half_screen
        || (props.computes_content_height() && last_item_bottom + element_height < -content_y_value)
    {
        // Jumping more than 1.5 screens: random seek.
        let _seek = TraceSection::new(c"Slint listview random seek");
        ops.splice(0, ops.len(), 0, state.offset);
        state.offset = ((-content_y_value / element_height).floor() as usize).min(row_count - 1);
        (state.offset, 0 as Coord)
    } else if content_y_value < state.previous_content_y {
        // Scrolled down: find the new offset by walking existing instances.
        let mut it_y = first_item_y + content_y_value;
        let mut new_off = state.offset;
        for i in 0..ops.len() {
            changed |= ops.ensure_updated(i, new_off);
            let h = ops.height(i).unwrap_or(0 as Coord);
            if it_y + h > 0 as Coord || new_off + 1 >= row_count {
                break;
            }
            it_y += h;
            new_off += 1;
        }
        (new_off, it_y)
    } else {
        // Scrolled up: will instantiate items before offset in the loop below.
        (state.offset, first_item_y + content_y_value)
    };

    let mut loop_count = 0;
    loop {
        let pass = TraceSection::new(c"Slint listview_layout pass");
        pass.count(&TRACE_PASSES, 1);
        pass.counter(c"Slint listview pass ordinal", (loop_count + 1) as i64);
        // Fill gap before new_offset using already-instantiated items.
        while new_offset > state.offset && new_offset_y > 0 as Coord {
            new_offset -= 1;
            new_offset_y -= ops.height(new_offset - state.offset).unwrap_or(0 as Coord);
        }
        // If there is still a gap, create new instances before the current ones.
        let mut prepend_count = 0;
        while new_offset > 0 && new_offset_y > 0 as Coord {
            new_offset -= 1;
            ops.splice(0, 0, 1, new_offset);
            changed |= ops.ensure_updated(0, new_offset);
            new_offset_y -= ops.height(0).unwrap_or(0 as Coord);
            prepend_count += 1;
        }
        if prepend_count > 0 {
            state.offset = new_offset;
        }
        debug_assert!(new_offset >= state.offset && new_offset <= state.offset + ops.len());

        // Layout items until we fill the view, starting with already-instantiated ones.
        let mut y = new_offset_y;
        let mut idx = new_offset;
        let instances_begin = new_offset - state.offset;
        for i in instances_begin..ops.len() {
            if idx >= row_count {
                break;
            }
            changed |= ops.ensure_updated(i, idx);
            content_width_value = content_width_value.max(ops.listview_layout(i, &mut y, idx));
            idx += 1;
            if y >= listview_height {
                break;
            }
        }

        // Create more items until there is no more room.
        while y < listview_height && idx < row_count {
            let i = ops.len();
            ops.splice(i, 0, 1, idx);
            changed |= ops.ensure_updated(i, idx);
            content_width_value = content_width_value.max(ops.listview_layout(i, &mut y, idx));
            idx += 1;
        }

        if y < listview_height && content_y_value < 0 as Coord && loop_count < 3 {
            let _retry = TraceSection::new(c"Slint listview tail retry");
            debug_assert!(idx >= row_count);
            // Reached end of model with room to spare. Scroll up.
            content_y_value += listview_height - y;
            loop_count += 1;
            continue;
        }

        // Clean up instances that are not shown.
        if new_offset != state.offset {
            let remove_count = new_offset - state.offset;
            ops.splice(0, remove_count, 0, state.offset);
            state.offset = new_offset;
        }
        let keep = idx - new_offset;
        if ops.len() > keep {
            ops.splice(keep, ops.len() - keep, 0, state.offset + keep);
        }

        if ops.len() == 0 {
            break;
        }

        // Recompute coordinates for the scrollbar.
        let _publish = TraceSection::new(c"Slint listview publish viewport");
        state.cached_item_height = (y - new_offset_y) / ops.len() as Coord;
        state.anchor_y = state.cached_item_height * state.offset as Coord;
        props.content_height_set(LogicalLength::new(
            state.cached_item_height * row_count as Coord,
        ));
        props.content_width_set(LogicalLength::new(content_width_value));
        let new_content_y = -state.anchor_y + new_offset_y;
        // Important: Use get_internal here, the content_y may have a binding on it (especially
        // a physical animation).
        // We must not yet trigger a re-evaluation of that binding, as we have already updated the
        // content_width and content_height, but the content_y is not yet consistent.
        // So the physics animations limit value may be inconsistent.
        if new_content_y != props.content_y_get_internal().get() {
            // If a physics animation is ongoing (e.g. due to a flick), we should not interrupt it.
            // The physics animation implements intercept_set, and is therefore not interrupted by
            // a call to set() - so it's okay to just use a normal set here.
            props.content_y_set(LogicalLength::new(new_content_y));
        }
        state.previous_content_y = new_content_y;
        if trace.enabled() {
            trace.counter(
                c"Slint listview published y mpx",
                (new_content_y as f64 * 1000.) as i64,
            );
        }

        break;
    }

    if trace.enabled() {
        trace.counter(c"Slint listview range after begin", state.offset as i64);
        trace.counter(
            c"Slint listview range after end",
            state.offset.saturating_add(ops.len()) as i64,
        );
    }
    changed
}

/// Adapter implementing [`RepeaterInstanceOps`] for the native Rust repeater.
struct RustRepeaterOps<'a, C: RepeatedItemTree> {
    inner: &'a RefCell<RepeaterInner<C>>,
    init: &'a dyn Fn() -> ItemTreeRc<C>,
    model: &'a ModelRc<C::Data>,
    #[cfg(feature = "painted-scroll-probe")]
    painted_probe: &'a crate::painted_scroll_probe::Owner,
}

impl<C: RepeatedItemTree> RepeaterInstanceOps for RustRepeaterOps<'_, C> {
    fn len(&self) -> usize {
        self.inner.borrow().instances.len()
    }

    fn splice(&mut self, position: usize, remove: usize, add: usize, first_row: usize) {
        // Removal-only virtualization returns no newly created instance. End the
        // binding interval here, including witnesses for externally held delegates.
        #[cfg(feature = "painted-scroll-probe")]
        if remove != 0 {
            self.painted_probe.rebound();
        }
        let trace = TraceSection::new(c"Slint repeater splice");
        trace.counter(c"Slint splice add slots", add as i64);
        let mut inner = self.inner.borrow_mut();
        trace.identify(&inner.trace_identity);
        let _remove = (remove > 0).then(|| {
            trace_removed_instances(
                &inner.instances[position..position + remove],
                first_row,
                &inner.trace_identity,
            )
        });
        inner.instances.splice(
            position..position + remove,
            core::iter::repeat_with(|| (RepeatedInstanceState::Dirty, None)).take(add),
        );
    }

    fn ensure_updated(&mut self, instance_idx: usize, row: usize) -> bool {
        let (created, instance) = {
            let mut inner = self.inner.borrow_mut();
            let inner = &mut *inner;
            let identity = &inner.trace_identity;
            let c = &mut inner.instances[instance_idx];
            if c.0 != RepeatedInstanceState::Dirty {
                return false;
            }
            let created = c.1.is_none();
            if created {
                let trace = TraceSection::new(c"Slint delegate create");
                trace.identify(identity);
                trace.count(&TRACE_CREATES, 1);
                trace.counter(c"Slint delegate row", row as i64);
                c.1 = Some((self.init)());
            }
            let trace = TraceSection::new(if created {
                c"Slint delegate initial bind"
            } else {
                c"Slint dirty row rebind"
            });
            trace.identify(identity);
            if !created {
                trace.count(&TRACE_REBINDS, 1);
            }
            trace.counter(c"Slint delegate row", row as i64);
            c.1.as_ref()
                .unwrap()
                .update(row, traced_row_data(self.model, row).unwrap_or_default());
            c.0 = RepeatedInstanceState::Clean;
            (created, c.1.as_ref().unwrap().clone())
        };
        if created {
            let trace = TraceSection::new(c"Slint delegate init");
            if trace.enabled() {
                trace.identify(&self.inner.borrow().trace_identity);
            }
            crate::properties::evaluate_no_tracking(|| instance.init());
        }
        let trace = TraceSection::new(c"Slint delegate instantiate children");
        if trace.enabled() {
            trace.identify(&self.inner.borrow().trace_identity);
        }
        crate::item_tree::ensure_item_tree_instantiated(&vtable::VRc::into_dyn(instance));
        created
    }

    fn height(&self, instance_idx: usize) -> Option<Coord> {
        let _trace = TraceSection::new(c"Slint delegate height");
        self.inner.borrow().instances[instance_idx]
            .1
            .as_ref()
            .map(|x| x.as_pin_ref().item_geometry(0).height_length().get())
    }

    fn listview_layout(&self, instance_idx: usize, y: &mut Coord, row: usize) -> Coord {
        let trace = TraceSection::new(c"Slint delegate listview_layout");
        trace.count(&TRACE_LAYOUTS, 1);
        trace.counter(c"Slint delegate row", row as i64);
        let inner = self.inner.borrow();
        trace.identify(&inner.trace_identity);
        let mut y_len = LogicalLength::new(*y);
        let w = inner.instances[instance_idx]
            .1
            .as_ref()
            .unwrap()
            .as_pin_ref()
            .listview_layout(&mut y_len);
        *y = y_len.get();
        w.get()
    }
}

/// Avoid repeating list layout when its tracked geometry is unchanged.
#[pin_project]
#[derive(Default)]
struct ListViewLayoutTracker {
    #[pin]
    geometry: crate::properties::PropertyTracker,
    // Viewport dimensions are passed by value, so they cannot be tracked as a
    // property here. Only numeric geometry is retained, never row contents.
    width: Cell<Option<LogicalLength>>,
    height: Cell<Option<LogicalLength>>,
    reusable: Cell<bool>,
}

impl ListViewLayoutTracker {
    fn evaluate(
        self: Pin<&Self>,
        model_changed: bool,
        width: LogicalLength,
        height: LogicalLength,
        reuse_geometry: bool,
        layout: impl FnOnce() -> bool,
    ) -> bool {
        let width_changed = self.width.replace(Some(width)) != Some(width);
        let height_changed = self.height.replace(Some(height)) != Some(height);
        // A public callback may replace every property without changing geometry.
        // Re-entering the typed path must also replace those callback dependencies.
        let previously_reusable = self.reusable.replace(reuse_geometry);
        let tracker = self.project_ref().geometry;
        if model_changed
            || width_changed
            || height_changed
            || !reuse_geometry
            || !previously_reusable
            || tracker.is_dirty()
        {
            tracker.evaluate(layout)
        } else {
            // A clean invocation still connects the outer redraw/layout
            // tracker to row-height and viewport changes discovered earlier.
            tracker.register_as_dependency_to_current_binding();
            false
        }
    }
}

/// This struct is put in a component when using the `for` syntax.
/// It helps instantiating the ItemTree `T`.
#[pin_project]
pub struct RepeaterTracker<T: RepeatedItemTree> {
    #[cfg(feature = "painted-scroll-probe")]
    painted_probe: crate::painted_scroll_probe::Owner,

    inner: RefCell<RepeaterInner<T>>,
    #[pin]
    model: Property<ModelRc<T::Data>>,
    #[pin]
    /// Set to true when the model becomes dirty.
    is_dirty: Property<bool>,
    #[pin]
    /// Marked dirty by `ensure_updated` when instances are added or
    /// removed.  Layout and visit code register this as a dependency so
    /// they re-evaluate only after the update pass materializes the
    /// change, not when the model first becomes dirty.
    instance_generation: Property<()>,
    /// Only used for the list view to track if the scrollbar has changed and item needs to be laid out again.
    #[pin]
    listview_geometry_tracker: ListViewLayoutTracker,
}

impl<T: RepeatedItemTree> ModelChangeListener for RepeaterTracker<T> {
    /// Notify the peers that a specific row was changed
    fn row_changed(self: Pin<&Self>, row: usize) {
        #[cfg(feature = "painted-scroll-probe")]
        self.painted_probe.mutated();
        let trace = TraceSection::new(c"Slint repeater row_changed");
        trace.counter(c"Slint model row", row as i64);
        let mut inner = self.inner.borrow_mut();
        trace.identify(&inner.trace_identity);
        let inner = &mut *inner;
        let identity = &inner.trace_identity;
        if let Some(c) = inner
            .instances
            .get_mut(row.wrapping_sub(inner.layout_state.offset))
        {
            if !self.model.is_dirty() {
                if let Some(comp) = c.1.as_ref() {
                    let trace = TraceSection::new(c"Slint dirty row rebind");
                    trace.identify(identity);
                    trace.count(&TRACE_REBINDS, 1);
                    trace.counter(c"Slint delegate row", row as i64);
                    let model = self.project_ref().model.get_untracked();
                    comp.update(row, traced_row_data(&model, row).unwrap_or_default());
                    c.0 = RepeatedInstanceState::Clean;
                }
            } else {
                c.0 = RepeatedInstanceState::Dirty;
                // An equal model handle still needs to consume this deferred row bind.
                self.is_dirty.set(true);
            }
        }
    }
    /// Notify the peers that rows were added
    fn row_added(self: Pin<&Self>, mut index: usize, mut count: usize) {
        #[cfg(feature = "painted-scroll-probe")]
        self.painted_probe.mutated();
        let trace = TraceSection::new(c"Slint repeater row_added");
        trace.counter(c"Slint changed row begin", index as i64);
        trace.counter(c"Slint changed row count", count as i64);
        let mut inner = self.inner.borrow_mut();
        trace.identify(&inner.trace_identity);
        if index < inner.layout_state.offset {
            if index + count <= inner.layout_state.offset {
                // Entirely before the visible range: shift the offset.
                inner.layout_state.offset += count;
                self.is_dirty.set(true);
                for c in inner.instances.iter_mut() {
                    c.0 = RepeatedInstanceState::Dirty;
                }
                return;
            }
            count -= inner.layout_state.offset - index;
            index = 0;
        } else {
            index -= inner.layout_state.offset;
        }
        if count == 0 || index > inner.instances.len() {
            return;
        }
        self.is_dirty.set(true);
        inner.instances.splice(
            index..index,
            core::iter::repeat_n((RepeatedInstanceState::Dirty, None), count),
        );
        for c in inner.instances[index + count..].iter_mut() {
            // Because all the indexes are dirty
            c.0 = RepeatedInstanceState::Dirty;
        }
    }
    /// Notify the peers that rows were removed
    fn row_removed(self: Pin<&Self>, mut index: usize, mut count: usize) {
        #[cfg(feature = "painted-scroll-probe")]
        self.painted_probe.mutated();
        let trace = TraceSection::new(c"Slint repeater row_removed");
        trace.counter(c"Slint changed row begin", index as i64);
        trace.counter(c"Slint changed row count", count as i64);
        let mut inner = self.inner.borrow_mut();
        trace.identify(&inner.trace_identity);
        if index < inner.layout_state.offset {
            if index + count <= inner.layout_state.offset {
                // Entirely before the visible range: shift the offset.
                inner.layout_state.offset -= count;
                self.is_dirty.set(true);
                for c in inner.instances.iter_mut() {
                    c.0 = RepeatedInstanceState::Dirty;
                }
                return;
            }
            count -= inner.layout_state.offset - index;
            inner.layout_state.offset = index;
            index = 0;
        } else {
            index -= inner.layout_state.offset;
        }
        if count == 0 || index >= inner.instances.len() {
            return;
        }
        if (index + count) > inner.instances.len() {
            count = inner.instances.len() - index;
        }
        self.is_dirty.set(true);
        let _remove = trace_removed_instances(
            &inner.instances[index..index + count],
            inner.layout_state.offset + index,
            &inner.trace_identity,
        );
        inner.instances.drain(index..(index + count));
        for c in inner.instances[index..].iter_mut() {
            // Because all the indexes are dirty
            c.0 = RepeatedInstanceState::Dirty;
        }
    }

    fn reset(self: Pin<&Self>) {
        #[cfg(feature = "painted-scroll-probe")]
        self.painted_probe.mutated();
        let trace = TraceSection::new(c"Slint repeater reset");
        self.is_dirty.set(true);
        let mut inner = self.inner.borrow_mut();
        trace.identify(&inner.trace_identity);
        let _remove = trace_removed_instances(
            &inner.instances,
            inner.layout_state.offset,
            &inner.trace_identity,
        );
        inner.instances.clear();
    }
}

impl<C: RepeatedItemTree> Default for RepeaterTracker<C> {
    fn default() -> Self {
        Self {
            #[cfg(feature = "painted-scroll-probe")]
            painted_probe: Default::default(),
            inner: Default::default(),
            model: Property::new_named(ModelRc::default(), "i_slint_core::Repeater::model"),
            is_dirty: Property::new_named(false, "i_slint_core::Repeater::is_dirty"),
            instance_generation: Property::new_named(
                (),
                "i_slint_core::Repeater::instance_generation",
            ),
            listview_geometry_tracker: Default::default(),
        }
    }
}

#[pin_project]
pub struct Repeater<C: RepeatedItemTree>(#[pin] ModelChangeListenerContainer<RepeaterTracker<C>>);

impl<C: RepeatedItemTree> Default for Repeater<C> {
    fn default() -> Self {
        Self(Default::default())
    }
}

impl<C: RepeatedItemTree + 'static> Repeater<C> {
    fn data(self: Pin<&Self>) -> Pin<&RepeaterTracker<C>> {
        self.project_ref().0.get()
    }

    /// Register the model and dirty flag as dependencies of the current
    /// tracking scope (e.g. the redraw tracker) so it is notified when the
    /// model or its data changes.
    pub fn track_model_changes(self: Pin<&Self>) {
        self.data().project_ref().model.register_as_dependency();
        self.data().project_ref().is_dirty.register_as_dependency();
    }

    /// Register the instance generation as a dependency of the current
    /// tracking scope. This is for layout and visit code that should
    /// re-evaluate only after `ensure_updated` has materialized instance
    /// changes, not when the model first becomes dirty.
    pub fn track_instance_changes(self: Pin<&Self>) {
        self.data()
            .project_ref()
            .instance_generation
            .register_as_dependency();
    }

    fn model(self: Pin<&Self>) -> ModelRc<C::Data> {
        let model = self.data().project_ref().model;

        if model.is_dirty() {
            let old_model = model.get_internal();
            let m = model.get();
            if old_model != m {
                #[cfg(feature = "painted-scroll-probe")]
                self.data().painted_probe.mutated();
                let trace = TraceSection::new(c"Slint repeater model replace");
                {
                    let data = self.data();
                    let mut inner = data.inner.borrow_mut();
                    trace.identify(&inner.trace_identity);
                    let mut replacement = RepeaterInner::default();
                    // Keep the anonymous serial stable when this repeater's
                    // model changes; the replace event marks a new row epoch.
                    replacement.trace_identity = inner.trace_identity.clone();
                    *inner = replacement;
                }
                self.data().is_dirty.set(true);
                let peer = self.project_ref().0.model_peer();
                m.model_tracker().attach_peer(peer);
            }
            m
        } else {
            model.get()
        }
    }

    /// Call this function to make sure that the model is updated.
    /// The init function is the function to create a ItemTree.
    /// Returns `true` if instances were actually created or removed.
    /// Also recurses into child instances to ensure they are instantiated.
    pub fn ensure_updated(self: Pin<&Self>, init: impl Fn() -> ItemTreeRc<C>) -> bool {
        let trace = TraceSection::new(c"Slint repeater update");
        let model = self.model();
        if trace.enabled() {
            trace.identify(&self.data().inner.borrow().trace_identity);
        }
        let changed = if self.data().project_ref().is_dirty.get() {
            let count = model.row_count();
            let offset = self.0.inner.borrow().layout_state.offset;
            let mut ops = RustRepeaterOps {
                inner: &self.0.inner,
                init: &init,
                model: &model,
                #[cfg(feature = "painted-scroll-probe")]
                painted_probe: &self.data().painted_probe,
            };
            self.data().is_dirty.set(false);
            update_all_instances(&mut ops, offset, count);
            self.data().instance_generation.mark_dirty();
            true
        } else {
            false
        };
        #[cfg(feature = "painted-scroll-probe")]
        if changed { self.data().painted_probe.rebound(); }
        self.ensure_children_instantiated() || changed
    }

    /// Recurse into child instances to ensure they are instantiated.
    fn ensure_children_instantiated(&self) -> bool {
        let trace = TraceSection::new(c"Slint repeater instantiate children");
        if trace.enabled() {
            trace.identify(&self.0.inner.borrow().trace_identity);
        }
        let snapshot = {
            let inner = self.0.inner.borrow();
            child_snapshot(inner.instances.iter().map(|(_, instance)| instance.clone()))
        };
        snapshot.visit(ensure_repeated_item_tree_instantiated)
    }

    /// Register the ListView content properties as dependencies so that
    /// scrolling triggers a redraw.  Model dependencies are registered by
    /// [`Self::visit`], so this only covers the content geometry.
    pub fn track_changes_listview(
        self: Pin<&Self>,
        content_width: Option<Pin<&Property<LogicalLength>>>,
        content_height: Option<Pin<&Property<LogicalLength>>>,
        content_y: Pin<&Property<LogicalLength>>,
        listview_width: LogicalLength,
        listview_height: Pin<&Property<LogicalLength>>,
    ) {
        let props = TypedListViewProps {
            content_width,
            content_height,
            content_y,
        };
        self.track_changes_listview_callback(&props, listview_width);
        listview_height.register_as_dependency();
    }

    /// Trait-based variant of [`Self::track_changes_listview`] for runtime
    /// consumers that can't expose the content storage as strongly-typed
    /// `Pin<&Property<LogicalLength>>` references. The caller is
    /// responsible for registering the listview height as a dependency.
    pub fn track_changes_listview_callback(
        self: Pin<&Self>,
        props: &dyn ListViewProperties,
        listview_width: LogicalLength,
    ) {
        props.register_as_dependencies();
        // listview_width is passed as a value, not a property, so it cannot
        // be registered as a dependency. Kept in the signature for symmetry
        // with ensure_updated_listview.
        let _ = listview_width;
    }

    /// Same as `Self::ensure_updated` but for a ListView.
    /// Returns `true` if any instances were created or any child changed.
    pub fn ensure_updated_listview(
        self: Pin<&Self>,
        init: impl Fn() -> ItemTreeRc<C>,
        content_width: Option<Pin<&Property<LogicalLength>>>,
        content_height: Option<Pin<&Property<LogicalLength>>>,
        content_y: Pin<&Property<LogicalLength>>,
        listview_width: LogicalLength,
        listview_height: Pin<&Property<LogicalLength>>,
    ) -> bool {
        let props = TypedListViewProps {
            content_width,
            content_height,
            content_y,
        };
        self.ensure_updated_listview_impl(init, &props, listview_width, listview_height.get(), true)
    }

    /// Trait-based variant for runtime consumers with dynamically backed content
    /// properties. Each call refreshes dependencies because property identity can
    /// change even when the viewport dimensions are unchanged.
    pub fn ensure_updated_listview_callback(
        self: Pin<&Self>,
        init: impl Fn() -> ItemTreeRc<C>,
        props: &dyn ListViewProperties,
        listview_width: LogicalLength,
        listview_height: LogicalLength,
    ) -> bool {
        self.ensure_updated_listview_impl(init, props, listview_width, listview_height, false)
    }

    fn ensure_updated_listview_impl(
        self: Pin<&Self>,
        init: impl Fn() -> ItemTreeRc<C>,
        props: &dyn ListViewProperties,
        listview_width: LogicalLength,
        listview_height: LogicalLength,
        reuse_geometry: bool,
    ) -> bool {
        let trace = TraceSection::new(c"Slint listview_update");
        trace.count(&TRACE_UPDATES, 1);
        let model = self.model();
        let data = self.data();
        let model_changed = data.project_ref().is_dirty.get();
        data.project_ref().is_dirty.set(false);
        let changed = data.project_ref().listview_geometry_tracker.evaluate(
            model_changed,
            listview_width,
            listview_height,
            reuse_geometry,
            || {
                // Structural changes outside the materialized range still affect extent.
                model.model_tracker().track_row_count_changes();
                props.register_as_dependencies();
                let row_count = model.row_count();
                let mut layout_state = {
                    let inner = data.inner.borrow();
                    trace.identify(&inner.trace_identity);
                    inner.layout_state.clone()
                };
                let mut ops = RustRepeaterOps {
                    inner: &data.inner,
                    init: &init,
                    model: &model,
                    #[cfg(feature = "painted-scroll-probe")]
                    painted_probe: &data.painted_probe,
                };
                let changed = update_visible_instances(
                    &mut ops,
                    &mut layout_state,
                    row_count,
                    props,
                    listview_width,
                    listview_height,
                );
                data.inner.borrow_mut().layout_state = layout_state;
                changed
            },
        );
        if changed {
            self.data().instance_generation.mark_dirty();
        }
        #[cfg(feature = "painted-scroll-probe")]
        if changed { self.data().painted_probe.rebound(); }
        // Preserve the original descendant instantiation before observing final
        // membership. Only this actual ListView path registers diagnostic roots;
        // clean geometry reuse must register too. Eager repeaters consume no slots.
        let child_changed = self.ensure_children_instantiated();
        #[cfg(feature = "painted-scroll-probe")]
        if crate::painted_scroll_probe::active() {
            let inner = self.0.inner.borrow();
            for (slot, (_, instance)) in inner.instances.iter().enumerate() {
                if let Some(instance) = instance {
                    let root = crate::item_tree::ItemRc::new(vtable::VRc::into_dyn(instance.clone()), 0);
                    self.0.painted_probe.register(root.downgrade(), inner.layout_state.offset + slot);
                }
            }
        }
        child_changed || changed
    }

    /// Sets the data directly in the model
    pub fn model_set_row_data(self: Pin<&Self>, row: usize, data: C::Data) {
        let model = self.model();
        model.set_row_data(row, data);
    }

    /// Read a row from the model, registering a dependency on it when
    /// called from a binding evaluation.
    pub fn model_row_data(self: Pin<&Self>, row: usize) -> Option<C::Data> {
        let trace = TraceSection::new(c"Slint model row_data tracked");
        if trace.enabled() {
            trace.identify(&self.data().inner.borrow().trace_identity);
        }
        trace.count(&TRACE_READS, 1);
        trace.counter(c"Slint model row", row as i64);
        self.model().row_data_tracked(row)
    }

    /// Set the model binding
    pub fn set_model_binding(&self, binding: impl Fn() -> ModelRc<C::Data> + 'static) {
        self.0.model.set_binding(binding);
    }

    /// Call the visitor for the root of each instance.
    /// Also registers model dependencies so the current tracking scope
    /// (e.g. the redraw tracker) is notified when the model changes.
    pub fn visit(
        self: Pin<&Self>,
        order: TraversalOrder,
        mut visitor: crate::item_tree::ItemVisitorRefMut,
    ) -> crate::item_tree::VisitChildrenResult {
        self.track_model_changes();
        // We can't keep self.inner borrowed because the event might modify the model
        let count = self.0.inner.borrow().instances.len() as u32;
        for i in 0..count {
            let i = if order == TraversalOrder::BackToFront {
                i
            } else {
                count - i - 1
            };
            let c = self
                .0
                .inner
                .borrow()
                .instances
                .get(i as usize)
                .and_then(|c| c.1.clone());
            if let Some(c) = c
                && c.as_pin_ref()
                    .visit_children_item(-1, order, visitor.borrow_mut())
                    .has_aborted()
            {
                return crate::item_tree::VisitChildrenResult::abort(i, 0);
            }
        }
        crate::item_tree::VisitChildrenResult::CONTINUE
    }

    /// Call `cb` with the model row index and the z value of every instance, when the
    /// repeated element has a dynamic z binding. The row index is the one accepted by
    /// [`Self::instance_at`] (and thus by the `get_subtree` vtable entry).
    /// Also registers model dependencies so the current tracking scope is notified
    /// when the model changes.
    pub fn for_each_instance_z(self: Pin<&Self>, cb: &mut dyn FnMut(u32, f32)) {
        self.track_model_changes();
        // Read the z values without holding the borrow: evaluating the z property's
        // binding can run user code
        let (offset, instances): (usize, Vec<_>) = {
            let inner = self.0.inner.borrow();
            (
                inner.layout_state.offset,
                inner.instances.iter().map(|c| c.1.clone()).collect(),
            )
        };
        for (i, c) in instances.iter().enumerate() {
            let z = c
                .as_ref()
                .and_then(|c| c.as_pin_ref().z_order())
                .unwrap_or_default();
            cb((offset + i) as u32, z);
        }
    }

    /// Return the amount of instances currently in the repeater
    pub fn len(&self) -> usize {
        self.0.inner.borrow().instances.len()
    }

    /// Return the range of indices used by this Repeater.
    ///
    /// Two values are necessary here since the Repeater can start to insert the data from its
    /// model at an offset.
    pub fn range(&self) -> core::ops::Range<usize> {
        let inner = self.0.inner.borrow();
        core::ops::Range {
            start: inner.layout_state.offset,
            end: inner.layout_state.offset + inner.instances.len(),
        }
    }

    /// Return the instance for the given model index.
    /// The index should be within [`Self::range()`]
    pub fn instance_at(&self, index: usize) -> Option<ItemTreeRc<C>> {
        let inner = self.0.inner.borrow();
        inner
            .instances
            .get(index.checked_sub(inner.layout_state.offset)?)
            .and_then(|c| c.1.clone())
    }

    /// Return true if the Repeater as empty
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns a vector containing all instances
    pub fn instances_vec(&self) -> Vec<ItemTreeRc<C>> {
        self.0
            .inner
            .borrow()
            .instances
            .iter()
            .flat_map(|x| x.1.clone())
            .collect()
    }
}

#[pin_project]
pub struct Conditional<C: RepeatedItemTree> {
    #[cfg(feature = "painted-scroll-probe")]
    painted_source: crate::painted_scroll_probe::ConditionalSource,
    #[pin]
    model: Property<bool>,
    #[pin]
    instance_generation: Property<()>,
    instance: RefCell<Option<ItemTreeRc<C>>>,
}

impl<C: RepeatedItemTree> Default for Conditional<C> {
    fn default() -> Self {
        Self {
            #[cfg(feature = "painted-scroll-probe")]
            painted_source: Default::default(),
            model: Property::new_named(false, "i_slint_core::Conditional::model"),
            instance_generation: Property::new_named(
                (),
                "i_slint_core::Conditional::instance_generation",
            ),
            instance: RefCell::new(None),
        }
    }
}

impl<C: RepeatedItemTree + 'static> Conditional<C> {
    /// Register the condition as a dependency of the current tracking scope
    /// (e.g. the redraw tracker) so it is notified when the condition changes.
    pub fn track_model_changes(self: Pin<&Self>) {
        self.project_ref().model.register_as_dependency();
    }

    /// Register the instance generation as a dependency of the current
    /// tracking scope. Layout code uses this to re-evaluate only after
    /// `ensure_updated` materializes instance changes.
    pub fn track_instance_changes(self: Pin<&Self>) {
        self.project_ref()
            .instance_generation
            .register_as_dependency();
    }

    /// Call this function to make sure that the model is updated.
    /// The init function is the function to create a ItemTree.
    /// Returns `true` if the instance was created or removed, or any child changed.
    pub fn ensure_updated(self: Pin<&Self>, init: impl Fn() -> ItemTreeRc<C>) -> bool {
        #[cfg(feature = "painted-scroll-probe")]
        if self.project_ref().model.is_dirty() { self.painted_source.rebound(); }
        let model = self.project_ref().model.get();

        let changed = if !model {
            #[cfg(feature = "painted-scroll-probe")]
            self.painted_source.retired();
            self.instance.take().is_some()
        } else if self.instance.borrow().is_none() {
            let i = init();
            self.instance.replace(Some(i.clone()));
            #[cfg(feature = "painted-scroll-probe")]
            self.painted_source.materialized();
            i.init();
            true
        } else {
            false
        };
        if changed {
            self.instance_generation.mark_dirty();
        }
        if let Some(instance) = self.instance.borrow().as_ref() {
            ensure_repeated_item_tree_instantiated(instance.clone()) || changed
        } else {
            changed
        }
    }

    /// Set the model binding
    pub fn set_model_binding(&self, binding: impl Fn() -> bool + 'static) {
        #[cfg(feature = "painted-scroll-probe")]
        self.painted_source.rebound();
        self.model.set_binding(binding);
    }

    /// Call the visitor for the root of each instance.
    /// Also registers model dependencies so the current tracking scope
    /// (e.g. the redraw tracker) is notified when the condition changes.
    pub fn visit(
        self: Pin<&Self>,
        order: TraversalOrder,
        mut visitor: crate::item_tree::ItemVisitorRefMut,
    ) -> crate::item_tree::VisitChildrenResult {
        self.track_model_changes();
        // We can't keep self.inner borrowed because the event might modify the model
        let instance = self.instance.borrow().clone();
        if let Some(c) = instance {
            #[cfg(feature = "painted-scroll-probe")]
            let _painted_source_scope = self.painted_source.scope(
                crate::item_tree::ItemRc::new_root(vtable::VRc::into_dyn(c.clone())).downgrade());
            let result = c.as_pin_ref().visit_children_item(-1, order, visitor.borrow_mut());
            if result.has_aborted() {
                return crate::item_tree::VisitChildrenResult::abort(0, 0);
            }
        }

        crate::item_tree::VisitChildrenResult::CONTINUE
    }

    /// Call `cb` with the index and the z value of the instance if the condition is
    /// active, when the conditional element has a dynamic z binding.
    /// Also registers the condition as a dependency of the current tracking scope.
    pub fn for_each_instance_z(self: Pin<&Self>, cb: &mut dyn FnMut(u32, f32)) {
        self.track_model_changes();
        let instance = self.instance.borrow().clone();
        if let Some(c) = instance {
            cb(0, c.as_pin_ref().z_order().unwrap_or_default());
        }
    }

    /// Return the amount of instances (1 if the conditional is active, 0 otherwise)
    pub fn len(&self) -> usize {
        self.instance.borrow().is_some() as usize
    }

    /// Return the range of indices used by this Conditional.
    ///
    /// Similar to Repeater::range, but the range is always [0, 1] if the Conditional is active.
    pub fn range(&self) -> core::ops::Range<usize> {
        0..self.len()
    }

    /// Return the instance for the given model index.
    /// The index should be within [`Self::range()`]
    pub fn instance_at(&self, index: usize) -> Option<ItemTreeRc<C>> {
        if index != 0 {
            return None;
        }
        self.instance.borrow().clone()
    }

    /// Return true if the Repeater as empty
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns a vector containing all instances
    pub fn instances_vec(&self) -> Vec<ItemTreeRc<C>> {
        self.instance.borrow().clone().into_iter().collect()
    }
}

#[cfg(test)]
mod child_snapshot_tests {
    use super::*;
    use alloc::rc::Rc;

    #[test]
    fn snapshot_storage_preserves_holes_order_and_all_callback_results() {
        for len in 0..=12 {
            let slots: Vec<_> = (0..len).map(|i| (i % 3 != 1).then_some(i)).collect();
            let snapshot = child_snapshot(slots.iter().copied());
            match &snapshot {
                ChildSnapshot::Inline(_) => assert!(len <= 4),
                ChildSnapshot::Heap(heap) => {
                    assert!(len > 4);
                    assert!(heap.capacity() >= len);
                }
            }
            let mut visited = Vec::new();
            let changed = snapshot.visit(|i| {
                visited.push(i);
                true // Must not short-circuit the remaining callbacks.
            });
            assert_eq!(visited, slots.into_iter().flatten().collect::<Vec<_>>());
            assert_eq!(changed, !visited.is_empty());
        }
    }

    #[test]
    fn reentrant_replacement_keeps_original_snapshot_and_releases_removed_children() {
        for len in [1, 4, 5, 12] {
            let live = RefCell::new((0..len).map(|i| Some(Rc::new(i))).collect::<Vec<_>>());
            let weak: Vec<_> = live.borrow().iter().flatten().map(Rc::downgrade).collect();
            let snapshot = child_snapshot(live.borrow().iter().cloned());
            let mut visited = Vec::new();
            assert!(!snapshot.visit(|instance| {
                if visited.is_empty() {
                    *live.borrow_mut() = alloc::vec![Some(Rc::new(99))];
                    let nested = child_snapshot(live.borrow().iter().cloned());
                    let mut nested_visited = Vec::new();
                    nested.visit(|x| {
                        nested_visited.push(*x);
                        false
                    });
                    assert_eq!(nested_visited, alloc::vec![99]);
                    assert!(weak.iter().all(|w| w.upgrade().is_some()));
                }
                visited.push(*instance);
                false
            }));
            assert_eq!(visited, (0..len).collect::<Vec<_>>());
            assert!(weak.iter().all(|w| w.upgrade().is_none()));
        }
    }

    #[test]
    fn snapshot_does_not_hide_child_property_dependencies() {
        for len in [4, 5] {
            let property = core::pin::pin!(Property::new(1));
            let outer = core::pin::pin!(crate::properties::PropertyTracker::<false>::default());
            let slots = alloc::vec![Some(()); len];
            outer.as_ref().evaluate(|| {
                child_snapshot(slots.iter().copied()).visit(|_| {
                    property.as_ref().get();
                    false
                })
            });
            assert!(!outer.is_dirty());
            property.as_ref().set(2);
            assert!(outer.is_dirty());
        }
    }

    #[cfg(feature = "std")]
    #[test]
    fn unwind_releases_unvisited_snapshot_children() {
        for len in [4, 5] {
            let slots: Vec<_> = (0..len).map(|i| Some(Rc::new(i))).collect();
            let weak: Vec<_> = slots.iter().flatten().map(Rc::downgrade).collect();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                child_snapshot(slots.into_iter()).visit(|_| panic!("child callback"));
            }));
            assert!(result.is_err());
            assert!(weak.iter().all(|w| w.upgrade().is_none()));
        }
    }
}

#[cfg(test)]
mod layout_tracker_tests {
    use super::*;

    #[test]
    fn clean_layout_stays_connected_to_outer_redraw_dependencies() {
        let tracker = core::pin::pin!(ListViewLayoutTracker::default());
        let outer = core::pin::pin!(crate::properties::PropertyTracker::<false>::default());
        let height = core::pin::pin!(Property::new(LogicalLength::new(30.)));
        let viewport = core::pin::pin!(Property::new(LogicalLength::new(0.)));
        let calls = Cell::new(0);
        let run = || {
            tracker.as_ref().evaluate(
                false,
                LogicalLength::new(200.),
                LogicalLength::new(100.),
                true,
                || {
                    height.as_ref().get();
                    viewport.as_ref().get();
                    calls.set(calls.get() + 1);
                    true
                },
            )
        };
        assert!(outer.as_ref().evaluate(run));
        assert!(!outer.as_ref().evaluate(run));
        assert_eq!(calls.get(), 1);
        height.as_ref().set(LogicalLength::new(60.));
        assert!(
            outer.is_dirty(),
            "row-height changes must still request layout/redraw"
        );
        assert!(outer.as_ref().evaluate(run));
        assert!(!outer.as_ref().evaluate(run));
        viewport.as_ref().set(LogicalLength::new(-10.));
        assert!(
            outer.is_dirty(),
            "scrolling must invalidate a previously clean pass"
        );
        assert!(outer.as_ref().evaluate(run));
        assert_eq!(calls.get(), 3);
    }

    #[test]
    fn width_height_model_replacement_and_offscreen_row_count_changes_rerun_layout() {
        let tracker = core::pin::pin!(ListViewLayoutTracker::default());
        let model = crate::model::VecModel::from(alloc::vec![1, 2]);
        let calls = Cell::new(0);
        let layout = || {
            model.model_tracker().track_row_count_changes();
            calls.set(calls.get() + 1);
            true
        };
        let width = LogicalLength::new(200.);
        let height = LogicalLength::new(100.);
        assert!(
            tracker
                .as_ref()
                .evaluate(false, width, height, true, layout)
        );
        assert!(
            !tracker
                .as_ref()
                .evaluate(false, width, height, true, layout)
        );
        model.push(3);
        assert!(
            tracker
                .as_ref()
                .evaluate(false, width, height, true, layout)
        );
        model.remove(2);
        assert!(
            tracker
                .as_ref()
                .evaluate(false, width, height, true, layout)
        );
        assert!(
            tracker
                .as_ref()
                .evaluate(false, LogicalLength::new(250.), height, true, layout)
        );
        assert!(
            tracker
                .as_ref()
                .evaluate(true, LogicalLength::new(250.), height, true, layout)
        );
        assert!(
            !tracker
                .as_ref()
                .evaluate(false, LogicalLength::new(250.), height, true, layout)
        );
        let taller = LogicalLength::new(150.);
        assert!(
            tracker
                .as_ref()
                .evaluate(false, LogicalLength::new(250.), taller, true, layout)
        );
        assert!(
            !tracker
                .as_ref()
                .evaluate(false, LogicalLength::new(250.), taller, true, layout)
        );
        assert!(
            tracker
                .as_ref()
                .evaluate(false, LogicalLength::new(250.), height, true, layout)
        );
        assert_eq!(calls.get(), 7);
    }
    #[test]
    fn callback_properties_refresh_and_typed_reentry_replaces_dependencies() {
        let tracker = core::pin::pin!(ListViewLayoutTracker::default());
        let typed_y = core::pin::pin!(Property::new(LogicalLength::new(0.)));
        let callback_y = core::pin::pin!(Property::new(LogicalLength::new(0.)));
        let replacement_y = core::pin::pin!(Property::new(LogicalLength::new(0.)));
        let width = LogicalLength::new(200.);
        let height = LogicalLength::new(100.);
        let run = |property: Pin<&Property<LogicalLength>>, reuse| {
            tracker.as_ref().evaluate(false, width, height, reuse, || {
                property.get();
                true
            })
        };
        assert!(run(typed_y.as_ref(), true));
        assert!(!run(typed_y.as_ref(), true));
        assert!(run(callback_y.as_ref(), false));
        assert!(run(replacement_y.as_ref(), false));
        assert!(run(typed_y.as_ref(), true));
        assert!(!run(typed_y.as_ref(), true));
        replacement_y.as_ref().set(LogicalLength::new(-10.));
        assert!(
            !run(typed_y.as_ref(), true),
            "obsolete callback dependencies must be detached"
        );
        typed_y.as_ref().set(LogicalLength::new(-10.));
        assert!(
            run(typed_y.as_ref(), true),
            "typed dependencies must be restored"
        );
    }
}

#[cfg(feature = "ffi")]
mod ffi {
    #![allow(unsafe_code)]

    use super::*;

    /// C++ callback table for [`RepeaterInstanceOps`], including the opaque
    /// user_data pointer that is passed to each callback.
    #[repr(C)]
    pub struct RepeaterInstanceOpsVTable {
        pub user_data: *mut core::ffi::c_void,
        pub len: unsafe extern "C" fn(user_data: *mut core::ffi::c_void) -> usize,
        pub splice: unsafe extern "C" fn(
            user_data: *mut core::ffi::c_void,
            position: usize,
            remove: usize,
            add: usize,
        ),
        pub ensure_updated: unsafe extern "C" fn(
            user_data: *mut core::ffi::c_void,
            instance_idx: usize,
            row: usize,
        ) -> bool,
        /// Height of instance, or NaN if not yet created.
        pub height:
            unsafe extern "C" fn(user_data: *mut core::ffi::c_void, instance_idx: usize) -> Coord,
        pub listview_layout: Option<
            unsafe extern "C" fn(
                user_data: *mut core::ffi::c_void,
                instance_idx: usize,
                y: &mut Coord,
            ) -> Coord,
        >,
        pub init: unsafe extern "C" fn(user_data: *mut core::ffi::c_void, instance_idx: usize),
    }

    impl RepeaterInstanceOps for RepeaterInstanceOpsVTable {
        fn len(&self) -> usize {
            unsafe { (self.len)(self.user_data) }
        }
        fn splice(&mut self, position: usize, remove: usize, add: usize, _first_row: usize) {
            unsafe { (self.splice)(self.user_data, position, remove, add) }
        }
        fn ensure_updated(&mut self, instance_idx: usize, row: usize) -> bool {
            let created = unsafe { (self.ensure_updated)(self.user_data, instance_idx, row) };
            if created {
                unsafe { (self.init)(self.user_data, instance_idx) };
            }
            created
        }
        fn height(&self, instance_idx: usize) -> Option<Coord> {
            let h = unsafe { (self.height)(self.user_data, instance_idx) };
            if h.is_nan() { None } else { Some(h) }
        }
        fn listview_layout(&self, instance_idx: usize, y: &mut Coord, _row: usize) -> Coord {
            self.listview_layout.map_or(0 as Coord, |f| unsafe {
                f(self.user_data, instance_idx, y)
            })
        }
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn slint_repeater_ensure_updated(
        ops: &mut RepeaterInstanceOpsVTable,
        offset: usize,
        count: usize,
    ) {
        update_all_instances(ops, offset, count);
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn slint_repeater_ensure_updated_listview(
        ops: &mut RepeaterInstanceOpsVTable,
        state: &mut RepeaterLayoutState,
        row_count: usize,
        content_width: Option<Pin<&Property<LogicalLength>>>,
        content_height: Option<Pin<&Property<LogicalLength>>>,
        content_y: Pin<&Property<LogicalLength>>,
        listview_width: LogicalLength,
        listview_height: LogicalLength,
    ) -> bool {
        let props = TypedListViewProps {
            content_width,
            content_height,
            content_y,
        };
        update_visible_instances(
            ops,
            state,
            row_count,
            &props,
            listview_width,
            listview_height,
        )
    }
}

#[cfg(all(test, feature = "painted-scroll-probe"))]
#[path = "painted_probe_lifecycle_tests.rs"]
mod painted_probe_lifecycle_tests;

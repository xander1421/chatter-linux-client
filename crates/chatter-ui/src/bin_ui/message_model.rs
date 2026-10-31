//! Retained message rows with identity-only lookup and batched history insertion.
use crate::MessageItemData;
use slint::{Model, ModelNotify, ModelTracker, SharedString};
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
};

#[derive(Default)]
pub struct MessageModel {
    rows: RefCell<Vec<MessageItemData>>,
    positions: RefCell<HashMap<SharedString, usize>>,
    notify: ModelNotify,
    pub has_newer: Cell<bool>,
    pub deferred_revision: Cell<u64>,
    pub revision: Cell<u64>,
    pub newer_cursor: RefCell<Option<String>>,
}
impl From<Vec<MessageItemData>> for MessageModel {
    fn from(rows: Vec<MessageItemData>) -> Self {
        let positions = rows
            .iter()
            .enumerate()
            .map(|(i, row)| (row.message_id.clone(), i))
            .collect();
        Self {
            rows: RefCell::new(rows),
            positions: RefCell::new(positions),
            notify: ModelNotify::default(),
            ..Default::default()
        }
    }
}
impl MessageModel {
    fn reindex(&self) {
        *self.positions.borrow_mut() = self
            .rows
            .borrow()
            .iter()
            .enumerate()
            .map(|(i, row)| (row.message_id.clone(), i))
            .collect();
    }
    pub fn row_of(&self, id: &str) -> Option<usize> {
        self.positions
            .borrow()
            .get(&SharedString::from(id))
            .copied()
    }
    pub fn has_pending(&self) -> bool {
        self.positions
            .borrow()
            .keys()
            .any(|id| id.starts_with("pending:"))
    }
    pub fn ids(&self) -> Vec<String> {
        self.positions
            .borrow()
            .keys()
            .map(ToString::to_string)
            .collect()
    }
    pub fn push(&self, row: MessageItemData) {
        let index = self.rows.borrow().len();
        self.positions
            .borrow_mut()
            .insert(row.message_id.clone(), index);
        self.rows.borrow_mut().push(row);
        self.revision.set(self.revision.get().wrapping_add(1));
        self.notify.row_added(index, 1);
    }
    pub fn insert(&self, index: usize, row: MessageItemData) {
        self.rows.borrow_mut().insert(index, row);
        self.reindex();
        self.revision.set(self.revision.get().wrapping_add(1));
        self.notify.row_added(index, 1);
    }
    pub fn prepend(&self, rows: Vec<MessageItemData>) -> usize {
        let count = rows.len();
        if count == 0 {
            return 0;
        }
        self.rows.borrow_mut().splice(0..0, rows);
        self.reindex();
        self.revision.set(self.revision.get().wrapping_add(1));
        self.notify.row_added(0, count);
        count
    }
    pub fn append_page(&self, rows: Vec<MessageItemData>) -> usize {
        let count = rows.len();
        if count == 0 {
            return 0;
        }
        let start = self.row_count();
        if self.has_pending() {
            for row in rows {
                let keys: Vec<_> = self
                    .rows
                    .borrow()
                    .iter()
                    .map(|row| {
                        (
                            row.timestamp_ns.parse::<u64>().unwrap_or(0),
                            row.sender_id.to_string(),
                        )
                    })
                    .collect();
                let refs: Vec<_> = keys
                    .iter()
                    .map(|(time, sender)| (*time, sender.as_str()))
                    .collect();
                if let chatter_core::domain::message::InsertPosition::At(index) =
                    chatter_core::domain::message::ordered_insert_position(
                        &refs,
                        row.timestamp_ns.parse().unwrap_or(0),
                        row.sender_id.as_str(),
                    )
                {
                    self.insert(index, row);
                }
            }
            return self.row_count() - start;
        }
        self.rows.borrow_mut().extend(rows);
        self.reindex();
        self.revision.set(self.revision.get().wrapping_add(1));
        self.notify.row_added(start, count);
        count
    }
    pub fn retain_range(&self, range: std::ops::Range<usize>) -> (usize, usize) {
        let back = self.row_count().saturating_sub(range.end);
        if back > 0 {
            self.has_newer.set(true);
            *self.newer_cursor.borrow_mut() = self
                .rows
                .borrow()
                .get(range.end.saturating_sub(1))
                .map(|row| row.message_id.to_string());
            self.rows.borrow_mut().truncate(range.end);
            self.revision.set(self.revision.get().wrapping_add(1));
            self.reindex();
            self.notify.row_removed(range.end, back);
        }
        if range.start > 0 {
            self.rows.borrow_mut().drain(..range.start);
            self.revision.set(self.revision.get().wrapping_add(1));
            self.reindex();
            self.notify.row_removed(0, range.start);
        }
        self.reindex();
        (range.start, back)
    }
    pub fn note_deferred(&self) {
        self.deferred_revision
            .set(self.deferred_revision.get().wrapping_add(1));
    }
    pub fn remove(&self, index: usize) -> MessageItemData {
        let removed = self.rows.borrow_mut().remove(index);
        self.reindex();
        self.revision.set(self.revision.get().wrapping_add(1));
        self.notify.row_removed(index, 1);
        removed
    }
}
impl Model for MessageModel {
    type Data = MessageItemData;
    fn row_count(&self) -> usize {
        self.rows.borrow().len()
    }
    fn row_data(&self, row: usize) -> Option<MessageItemData> {
        self.rows.borrow().get(row).cloned()
    }
    fn set_row_data(&self, row: usize, data: MessageItemData) {
        let mut rows = self.rows.borrow_mut();
        let Some(slot) = rows.get_mut(row) else {
            return;
        };
        if slot.message_id != data.message_id {
            let mut positions = self.positions.borrow_mut();
            positions.remove(&slot.message_id);
            positions.insert(data.message_id.clone(), row);
        }
        *slot = data;
        drop(rows);
        self.revision.set(self.revision.get().wrapping_add(1));
        self.notify.row_changed(row);
    }
    fn model_tracker(&self) -> &dyn ModelTracker {
        &self.notify
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn row(id: &str) -> MessageItemData {
        MessageItemData {
            message_id: id.into(),
            ..Default::default()
        }
    }
    #[test]
    fn identity_index_tracks_prepend_replace_and_remove() {
        let model = MessageModel::from(vec![row("c"), row("d")]);
        assert_eq!(model.prepend(vec![row("a"), row("b")]), 2);
        assert_eq!(model.row_of("c"), Some(2));
        model.set_row_data(2, row("replacement"));
        assert_eq!(model.row_of("c"), None);
        model.remove(0);
        assert_eq!(model.row_of("replacement"), Some(1));
        model.insert(1, row("inserted"));
        model.push(row("tail"));
        assert_eq!(model.row_of("tail"), Some(4));
        assert_eq!(model.row_data(0).unwrap().message_id, "b");
    }
    #[test]
    fn retained_window_drops_pixels_by_row_ownership_and_tracks_reload_cursor() {
        let model = MessageModel::from((0..200).map(|i| row(&i.to_string())).collect::<Vec<_>>());
        assert_eq!(model.retain_range(25..175), (25, 25));
        assert_eq!(model.row_count(), 150);
        assert_eq!(model.row_of("25"), Some(0));
        assert_eq!(model.row_of("174"), Some(149));
        assert_eq!(model.row_of("0"), None);
        assert_eq!(model.row_of("199"), None);
        assert!(model.has_newer.get());
        assert_eq!(model.newer_cursor.borrow().as_deref(), Some("174"));
        model.note_deferred();
        assert_eq!(model.deferred_revision.get(), 1);
        model.append_page(vec![row("175")]);
        assert_eq!(model.row_of("175"), Some(150));
    }
}

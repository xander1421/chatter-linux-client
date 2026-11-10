//! Small LRU of decoded full-res viewer images, keyed by attachment id.
//!
//! Navigating between open images re-ran the whole decrypt + decode pipeline
//! each time, so `select()` blanked the source and the viewer flashed
//! blank -> low-res thumbnail -> full image on every page turn (the "reload
//! bloom"). This caches the decoded full image per attachment so revisiting one
//! restores it in the same event-loop tick, with no blank and no re-decode.
//!
//! UI-thread only: `slint::Image` is not `Sync`, and every caller is the Slint
//! event loop. GIFs are deliberately NOT cached — they must re-run their own
//! animation flow on revisit, so a cache miss there is correct.
use slint::{ComponentHandle, SharedString};
use std::cell::RefCell;

struct Entry {
    id: String,
    image: slint::Image,
    filename: SharedString,
    size: SharedString,
}

// ponytail: plain Vec ring, not a real LRU map. 6 entries, linear scan is
// nothing; grow to an indexed LRU only if the cap ever needs to be large.
const CAP: usize = 6;

thread_local! {
    static CACHE: RefCell<Vec<Entry>> = const { RefCell::new(Vec::new()) };
}

/// Cache a decoded full image. Call only for static images, never GIFs.
pub(super) fn put(id: &str, image: slint::Image, filename: SharedString, size: SharedString) {
    CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        cache.retain(|entry| entry.id != id);
        cache.push(Entry {
            id: id.to_string(),
            image,
            filename,
            size,
        });
        while cache.len() > CAP {
            cache.remove(0);
        }
    });
}

/// On hit, restore the viewer source and chrome from cache (moving the entry to
/// most-recent) and return true, so the caller skips the decrypt/decode. Runs
/// synchronously right after `select()` blanked the source, so the blank never
/// reaches a repaint.
pub(super) fn try_restore(window: &crate::MainWindow, id: &str) -> bool {
    CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        let Some(index) = cache.iter().position(|entry| entry.id == id) else {
            return false;
        };
        let entry = cache.remove(index);
        let pane = window.global::<crate::ChatPane>();
        pane.set_image_viewer_source(entry.image.clone());
        pane.set_image_viewer_filename(entry.filename.clone());
        pane.set_image_viewer_size(entry.size.clone());
        pane.set_image_viewer_visible(true);
        cache.push(entry);
        true
    })
}

/// Drop decrypted pixels when the viewer closes, so they do not linger in RAM.
pub(super) fn clear() {
    CACHE.with(|cache| cache.borrow_mut().clear());
}

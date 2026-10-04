//! Test-only lifetime for temporary directories.
//!
//! Fixtures that hand out an `Arc<Database>` (or a path) need their `TempDir`
//! to outlive the helper that made it. They used to `Box::leak` it, which kept
//! it alive for the whole process and never deleted it — on a tmpfs `/tmp`
//! that was thousands of directories of SQLite files held in RAM. [`hold`]
//! parks the `TempDir` in a thread-local instead. libtest runs every test on a
//! thread of its own, so the directory is deleted when that test's thread
//! exits, i.e. when the test ends.

use std::cell::RefCell;

thread_local! {
    static HELD: RefCell<Vec<tempfile::TempDir>> = const { RefCell::new(Vec::new()) };
}

/// Keep `dir` until the current test ends, then delete it.
pub(crate) fn hold(dir: tempfile::TempDir) {
    HELD.with(|h| h.borrow_mut().push(dir));
}

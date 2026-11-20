//! Photos picker bridge to the iOS app shell.

use std::ffi::{CStr, c_char, c_void};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

use tokio::sync::oneshot;

unsafe extern "C" {
    fn chatter_ios_pick_image(
        context: *mut c_void,
        callback: extern "C" fn(*mut c_void, *const c_char),
    );
}

type ImageSender = oneshot::Sender<Option<PathBuf>>;

extern "C" fn image_picked(context: *mut c_void, path: *const c_char) {
    // The app shell calls back once. Its path pointer is borrowed only during
    // this callback, so copy it before waking the awaiting task.
    let sender = unsafe { Box::from_raw(context.cast::<ImageSender>()) };
    let path = (!path.is_null()).then(|| {
        let bytes = unsafe { CStr::from_ptr(path) }.to_bytes();
        PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
    });
    if let Err(Some(path)) = sender.send(path) {
        let _ = std::fs::remove_file(path);
    }
}

pub(crate) async fn pick_image() -> Option<PathBuf> {
    let (sender, receiver) = oneshot::channel();
    let context = Box::into_raw(Box::new(sender));
    // The app shell takes ownership of the context via the completion callback
    // and dispatches UIKit presentation onto the main queue.
    unsafe { chatter_ios_pick_image(context.cast(), image_picked) };
    receiver.await.ok().flatten()
}

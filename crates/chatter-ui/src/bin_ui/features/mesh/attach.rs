//! The composer's attach path, when the open conversation is a mesh one.
//!
//! The same shape as [`super::send`]: an early interception, before the picker
//! reaches the staging machinery that exists to upload to a CDN. A mesh file
//! makes no RPC — it is fragmented onto the radio — so it must never enter that
//! pipeline, and must never be *staged* either. Staging is for a send that
//! happens later with text attached; a mesh file goes out on its own.

use std::path::PathBuf;
use std::sync::Arc;

use slint::ComponentHandle;

use chatter_core::client::ChatterClient;

use crate::{MainWindow, MeshGlobal, run_ui};

/// Route the attach button into the mesh.
///
/// Returns `true` when the tap was consumed, so the caller must stop. Returns
/// `false` when no mesh conversation is open and the ordinary staging path
/// should run.
///
/// # Why a mesh DM is refused here rather than in the picker
///
/// A `FileTransfer` packet is cleartext, and every mesh DM is Noise-sealed, so
/// core refuses a directed file outright. The composer hides the button for a
/// mesh DM for that reason — but the callback can still arrive from a keyboard
/// shortcut or a stale frame, so the refusal is repeated here rather than
/// assumed. It reports, instead of failing silently: the button was visible a
/// moment ago in a different conversation, and a tap that does nothing looks
/// like a bug.
#[must_use]
pub fn intercept_attachment(ui_ctx: &Arc<ChatterClient>, weak: slint::Weak<MainWindow>) -> bool {
    use chatter_core::services::mesh::MeshConversation;

    let Some(conversation) = super::active() else {
        return false;
    };

    if matches!(conversation, MeshConversation::Dm(_)) {
        if let Some(w) = weak.upgrade() {
            w.invoke_finish_attachment_picker(true);
        }
        run_ui!(weak, |w| w.global::<MeshGlobal>().set_notice(
            "Files are not sent in a mesh direct message — the file packet is not encrypted."
                .into()
        ));
        return true;
    }

    let ui_ctx = ui_ctx.clone();
    tokio::spawn(async move {
        // One file, not many, on both platforms. Each is an independent transfer
        // on a link measured in single-digit kB/s, so a multi-select would
        // silently queue minutes of airtime behind one tap.
        let picked: Vec<PathBuf> = pick_one().await;
        let finish = weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = finish.upgrade() {
                w.invoke_finish_attachment_picker(super::active().as_ref() == Some(&conversation));
            }
        });
        let Some(path) = picked.into_iter().next() else {
            return;
        };

        // Core decides single-packet versus streamed, and refuses anything over
        // the cap. The UI does not get to guess which.
        match ui_ctx.mesh_send_file(&path) {
            Ok(message_id) => {
                tracing::info!(
                    %message_id,
                    path = %path.display(),
                    "mesh: file handed to the radio"
                );
                run_ui!(weak, |w| w.global::<MeshGlobal>().set_notice("".into()));
            }
            Err(error) => {
                // Same reasoning as a failed text send: over the radio there is
                // no server to blame and no retry queue to hide it in, so a
                // refusal has to be said out loud.
                tracing::warn!(%error, path = %path.display(), "mesh: file not sent");
                let notice = format!("Not sent: {error}");
                run_ui!(weak, |w| w
                    .global::<MeshGlobal>()
                    .set_notice(notice.clone().into()));
            }
        }
    });

    true
}

/// The platform's file picker, restricted to a single file.
///
/// The same fork the staging path makes in
/// [`crate::bin_ui::features::composer::attachments`]: desktop has a native
/// dialog in `chatter-core`, and on Android that module is `cfg`'d out entirely
/// in favour of the Storage Access Framework, which needs no permission.
#[cfg(not(target_os = "android"))]
async fn pick_one() -> Vec<PathBuf> {
    use chatter_core::media::file_picker::{FileFilter, FilePicker, FilePickerConfig};

    let config = FilePickerConfig::new()
        .with_title("Send over the mesh")
        .with_filter(FileFilter::all())
        .with_filter(FileFilter::images())
        .with_filter(FileFilter::documents())
        .with_multiple(false);
    FilePicker::pick_files(config).await
}

#[cfg(target_os = "android")]
async fn pick_one() -> Vec<PathBuf> {
    // `"*/*"`, as the staging path uses: the mesh will take any file, and the
    // size cap is core's to enforce, not the chooser's.
    crate::platform::android::file_picker::pick_files(&["*/*"])
        .await
        .into_iter()
        .take(1)
        .collect()
}

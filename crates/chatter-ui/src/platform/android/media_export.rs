//! Explicit viewer exports, with bounded reads into an owned file descriptor.
use super::activity;
use jni::objects::{JLongArray, JValue};
use std::fs::File;
use std::os::fd::FromRawFd;

pub struct ExportSession {
    token: i64,
    pub file: Option<File>,
    completed: bool,
}

impl ExportSession {
    pub fn begin(name: &str, mime: &str, copy: bool) -> Result<Self, String> {
        Self::begin_kind(name, mime, copy, false)
    }

    fn begin_kind(name: &str, mime: &str, copy: bool, pdf: bool) -> Result<Self, String> {
        let mut env = activity::jni_env()?;
        let activity = activity::activity()?;
        let name = env
            .new_string(name)
            .map_err(|_| "Filename conversion failed")?;
        let mime = env.new_string(mime).map_err(|_| "Type conversion failed")?;
        let result = env.call_method(
            activity.as_obj(),
            if pdf {
                "beginViewerPdfExport"
            } else {
                "beginViewerMediaExport"
            },
            "(Ljava/lang/String;Ljava/lang/String;Z)[J",
            &[
                JValue::Object(&name),
                JValue::Object(&mime),
                JValue::Bool(u8::from(copy)),
            ],
        );
        let object = match result.and_then(|v| v.l()) {
            Ok(v) if !v.is_null() => v,
            _ => {
                let _ = env.exception_clear();
                return Err("Media export unavailable".into());
            }
        };
        let array = JLongArray::from(object);
        let mut values = [0i64; 2];
        if env.get_long_array_region(&array, 0, &mut values).is_err() {
            let _ = env.exception_clear();
            return Err("Invalid export descriptor".into());
        }
        let fd = i32::try_from(values[1]).map_err(|_| "Invalid file descriptor")?;
        if values[0] <= 0 || fd < 0 {
            return Err("Invalid export descriptor".into());
        }
        // Kotlin detached this descriptor, transferring its sole ownership to Rust.
        let file = unsafe { File::from_raw_fd(fd) };
        Ok(Self {
            token: values[0],
            file: Some(file),
            completed: false,
        })
    }

    pub fn finish(mut self) -> Result<(), String> {
        self.file.take(); // close before publishing the MediaStore row / clipboard URI
        let result = finish(self.token, true);
        self.completed = result.is_ok();
        result
    }
}

impl Drop for ExportSession {
    fn drop(&mut self) {
        self.file.take();
        if !self.completed {
            let _ = finish(self.token, false);
        }
    }
}

fn finish(token: i64, success: bool) -> Result<(), String> {
    let mut env = activity::jni_env()?;
    let activity = activity::activity()?;
    let result = env.call_method(
        activity.as_obj(),
        "finishViewerMediaExport",
        "(JZ)Z",
        &[JValue::Long(token), JValue::Bool(u8::from(success))],
    );
    let success = result.and_then(|v| v.z()).unwrap_or(false);
    let exception = env.exception_check().unwrap_or(true);
    let _ = env.exception_clear();
    if success && !exception {
        Ok(())
    } else {
        Err("Media export failed".into())
    }
}

pub fn open_downloads() -> Result<(), String> {
    let mut env = activity::jni_env()?;
    let activity = activity::activity()?;
    let result = env.call_method(activity.as_obj(), "openViewerDownloads", "()V", &[]);
    let failed = result.is_err() || env.exception_check().unwrap_or(true);
    let _ = env.exception_clear();
    if failed {
        Err("Downloads unavailable".into())
    } else {
        Ok(())
    }
}

/// Export authenticated attachment ranges without allocating whole plaintext.
pub fn save_attachment(
    client: &chatter_core::client::ChatterClient,
    id: &chatter_core::domain::ids::AttachmentId,
    revision: u64,
) -> Result<(), String> {
    use std::io::Write;
    use zeroize::Zeroize;
    let check = || {
        if client.auth_state().get().session_revision == revision {
            Ok(())
        } else {
            Err("Session changed".to_string())
        }
    };
    check()?;
    let attachment = client.get_attachment(id).ok_or("Attachment unavailable")?;
    // Existing helper authenticates ciphertext before a destination is created.
    let reader = client
        .open_attachment_range(id)
        .map_err(|_| "Authenticated source unavailable")?;
    check()?;
    let mut session = ExportSession::begin(
        &attachment.metadata.name,
        &attachment.metadata.mime_type,
        false,
    )?;
    let output = session
        .file
        .as_mut()
        .ok_or("Export descriptor unavailable")?;
    let mut offset = 0;
    while offset < reader.plaintext_size() {
        check()?;
        let expected = (reader.plaintext_size() - offset).min(64 * 1024);
        let mut chunk = reader.read_range(offset, expected);
        if chunk.len() != expected {
            chunk.zeroize();
            return Err("Incomplete authenticated read".into());
        }
        let result = output.write_all(&chunk);
        chunk.zeroize();
        result.map_err(|_| "Download write failed")?;
        offset += expected;
    }
    output.sync_all().map_err(|_| "Unable to finish download")?;
    check()?;
    session.finish()
}

/// Only explicit PDF export can create a file in the provider's PDF directory.
pub fn prepare_pdf(name: &str, data: &[u8]) -> Result<ExportSession, String> {
    use std::io::Write;
    let mut session = ExportSession::begin_kind(name, "application/pdf", false, true)?;
    let file = session.file.as_mut().ok_or("PDF descriptor unavailable")?;
    for chunk in data.chunks(64 * 1024) {
        file.write_all(chunk).map_err(|_| "PDF write failed")?;
    }
    file.sync_all().map_err(|_| "Unable to finish PDF")?;
    Ok(session)
}

/// Drop private plaintext/grants on every authentication generation change.
/// Subscribe before launch; lagged events reconcile from the current snapshot.
pub fn wire_session(client: &std::sync::Arc<chatter_core::client::ChatterClient>) {
    let auth = client.auth_state().clone();
    let mut events = auth.subscribe();
    client.runtime_handle().spawn(async move {
        let mut previous = None;
        loop {
            let revision = auth.get().session_revision;
            if previous != Some(revision) {
                crate::bin_ui::features::chat_actions::media_viewer::invalidate();
                let result = (|| -> Result<(), String> {
                    let mut env = activity::jni_env()?;
                    let activity = activity::activity()?;
                    let result = env.call_method(
                        activity.as_obj(),
                        "clearViewerSession",
                        "(J)Z",
                        &[JValue::Long(revision as i64)],
                    );
                    let success = result.and_then(|v| v.z()).unwrap_or(false);
                    let _ = env.exception_clear();
                    if success {
                        Ok(())
                    } else {
                        Err("Private viewer cache cleanup failed".into())
                    }
                })();
                if let Err(error) = result {
                    tracing::warn!(%error, "Viewer session cleanup");
                }
                previous = Some(revision);
            }
            if matches!(
                events.recv().await,
                Err(tokio::sync::broadcast::error::RecvError::Closed)
            ) {
                break;
            }
        }
    });
}

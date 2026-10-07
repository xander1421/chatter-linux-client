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
        let mut env = activity::jni_env()?;
        let activity = activity::activity()?;
        let name = env
            .new_string(name)
            .map_err(|_| "Filename conversion failed")?;
        let mime = env.new_string(mime).map_err(|_| "Type conversion failed")?;
        let result = env.call_method(
            activity.as_obj(),
            "beginViewerMediaExport",
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
        self.completed = true;
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

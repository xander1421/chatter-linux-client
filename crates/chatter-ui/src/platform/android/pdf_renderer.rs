//! JNI bridge to Android's PdfRenderer. Call from a worker thread only.
use super::activity;
use jni::objects::{JByteArray, JLongArray, JValue};

fn with_pdf_jni<T>(
    f: impl FnOnce(&mut jni::Env<'_>, &jni::objects::JObject<'_>) -> Result<T, String>,
) -> Result<T, String> {
    activity::with_jni_env(|env| {
        let activity = activity::activity()?;
        let result = f(env, activity.as_obj());
        let _ = env.exception_clear();
        result
    })
}

pub fn load(data: &[u8]) -> Result<(i64, i32), String> {
    with_pdf_jni(|env, activity| {
        let bytes = env.byte_array_from_slice(data).map_err(|e| e.to_string())?;
        let value = env
            .call_method(
                activity,
                jni::jni_str!("loadViewerPdf"),
                jni::jni_sig!("([B)[J"),
                &[JValue::Object(bytes.as_ref())],
            )
            .and_then(|v| v.l())
            .map_err(|e| e.to_string())?;
        if value.is_null() {
            return Err("Failed to load PDF. It may be damaged or password protected.".into());
        }
        let array = env
            .cast_local::<JLongArray>(value)
            .map_err(|e| e.to_string())?;
        let mut values = [0i64; 2];
        array
            .get_region(env, 0, &mut values)
            .map_err(|e| e.to_string())?;
        let pages = i32::try_from(values[1]).map_err(|_| "Invalid PDF page count")?;
        if values[0] <= 0 || pages <= 0 {
            return Err("PDF has no pages".into());
        }
        Ok((values[0], pages))
    })
}

pub fn render(token: i64, page: i32) -> Result<(u32, u32, Vec<u8>), String> {
    with_pdf_jni(|env, activity| {
        let value = env
            .call_method(
                activity,
                jni::jni_str!("renderViewerPdfPage"),
                jni::jni_sig!("(JI)[B"),
                &[JValue::Long(token), JValue::Int(page)],
            )
            .and_then(|v| v.l())
            .map_err(|e| e.to_string())?;
        if value.is_null() {
            return Err("Failed to render PDF page".into());
        }
        let array = env
            .cast_local::<JByteArray>(value)
            .map_err(|e| e.to_string())?;
        let len = array.len(env).map_err(|e| e.to_string())? as usize;
        if len < 12 || len > 8 + 1200 * 2400 * 4 {
            return Err("Invalid PDF page dimensions".into());
        }
        let mut rgba = env.convert_byte_array(&array).map_err(|e| e.to_string())?;
        let width = u32::from_be_bytes(rgba[0..4].try_into().unwrap());
        let height = u32::from_be_bytes(rgba[4..8].try_into().unwrap());
        if width == 0
            || height == 0
            || (width as usize).checked_mul(height as usize).and_then(|n| n.checked_mul(4)) != Some(len - 8)
        {
            return Err("Invalid PDF page dimensions".into());
        }
        rgba.copy_within(8.., 0);
        rgba.truncate(len - 8);
        let zeros = [0i8; 4096];
        for offset in (0..len).step_by(zeros.len()) {
            if array.set_region(env, offset as i32, &zeros[..(len - offset).min(zeros.len())]).is_err() {
                let _ = env.exception_clear();
                break;
            }
        }
        Ok((width, height, rgba))
    })
}

pub fn close(token: i64) {
    let _ = with_pdf_jni(|env, activity| {
        env.call_method(
            activity,
            jni::jni_str!("closeViewerPdf"),
            jni::jni_sig!("(J)V"),
            &[JValue::Long(token)],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    });
}

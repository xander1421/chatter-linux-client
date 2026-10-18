//! JNI bridge to Android's PdfRenderer. Call from a worker thread only.
use super::activity;
use jni::objects::{JIntArray, JLongArray, JValue};
use zeroize::Zeroize;

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
                jni::jni_sig!("(JI)[I"),
                &[JValue::Long(token), JValue::Int(page)],
            )
            .and_then(|v| v.l())
            .map_err(|e| e.to_string())?;
        if value.is_null() {
            return Err("Failed to render PDF page".into());
        }
        let array = env
            .cast_local::<JIntArray>(value)
            .map_err(|e| e.to_string())?;
        let len = array.len(env).map_err(|e| e.to_string())? as usize;
        if len < 3 || len > 2 + 1200 * 2400 {
            return Err("Invalid PDF page dimensions".into());
        }
        let mut pixels = vec![0i32; len];
        array
            .get_region(env, 0, &mut pixels)
            .map_err(|e| e.to_string())?;
        let width = u32::try_from(pixels[0]).map_err(|_| "Invalid PDF width")?;
        let height = u32::try_from(pixels[1]).map_err(|_| "Invalid PDF height")?;
        if width == 0
            || height == 0
            || (width as usize).checked_mul(height as usize) != Some(len - 2)
        {
            return Err("Invalid PDF page dimensions".into());
        }
        let mut rgba = Vec::with_capacity((len - 2) * 4);
        for &pixel in pixels.iter().skip(2) {
            let argb = pixel as u32;
            rgba.extend_from_slice(&[
                (argb >> 16) as u8,
                (argb >> 8) as u8,
                argb as u8,
                (argb >> 24) as u8,
            ]);
        }
        pixels.zeroize();
        let _ = array.set_region(env, 0, &pixels);
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

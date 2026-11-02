//! Hand text to the OS share sheet (ACTION_SEND chooser), via the Activity's
//! `shareText` helper. Used by the friend-code sheet's "Share my code".

use jni::objects::JValue;

use super::activity::{activity, with_jni_env};

/// Open the Android share sheet with `text` as the shared plain text. Returns
/// `Err` if the Activity or the call is unavailable; the caller falls back to
/// copying.
pub fn share_text(text: &str) -> Result<(), String> {
    with_jni_env(|env| {
        let activity = activity()?;
        let jtext = env
            .new_string(text)
            .map_err(|_| "Share text conversion failed")?;
        let result = env.call_method(
            activity.as_obj(),
            jni::jni_str!("shareText"),
            jni::jni_sig!("(Ljava/lang/String;)V"),
            &[JValue::Object(&jtext)],
        );
        let failed = result.is_err() || env.exception_check();
        let _ = env.exception_clear();
        if failed {
            Err("Share sheet unavailable".into())
        } else {
            Ok(())
        }
    })
}

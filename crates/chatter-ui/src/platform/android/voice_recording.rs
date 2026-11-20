//! Foreground microphone lease for an explicitly started voice-note recording.
use super::activity::{activity, clear_exception, with_jni_env};
use jni::objects::{Global, JObject, JValue};

pub struct VoiceRecordingLease {
    activity: Global<JObject<'static>>,
    token: i64,
}

impl VoiceRecordingLease {
    fn request() -> Result<Self, String> {
        let activity = activity()?;
        let token = with_jni_env(|env| {
            let result = env.call_method(
                &activity,
                jni::jni_str!("startVoiceRecordingService"),
                jni::jni_sig!("()J"),
                &[],
            );
            if result.is_err() {
                clear_exception(env);
            }
            result
                .and_then(|value| value.j())
                .map_err(|e| e.to_string())
        })?;
        if token == 0 {
            return Err(chatter_i18n::tr(
                "Couldn't start voice recording. Return to the chat and try again.",
            ));
        }
        Ok(Self { activity, token })
    }

    fn status(&self) -> Result<i32, String> {
        with_jni_env(|env| {
            let result = env.call_method(
                &self.activity,
                jni::jni_str!("voiceRecordingServiceStatus"),
                jni::jni_sig!("(J)I"),
                &[JValue::Long(self.token)],
            );
            if result.is_err() {
                clear_exception(env);
            }
            result
                .and_then(|value| value.i())
                .map_err(|e| e.to_string())
        })
    }

    pub async fn start() -> Result<Self, String> {
        let lease = Self::request()?;
        for attempt in 0..=100 {
            match lease.status()? {
                1 => return Ok(lease),
                -1 => break,
                _ if attempt < 100 => {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await
                }
                _ => break,
            }
        }
        Err(chatter_i18n::tr(
            "Couldn't start voice recording. Return to the chat and try again.",
        ))
    }
}

impl Drop for VoiceRecordingLease {
    fn drop(&mut self) {
        let _ = with_jni_env(|env| {
            let result = env.call_method(
                &self.activity,
                jni::jni_str!("stopVoiceRecordingService"),
                jni::jni_sig!("(J)V"),
                &[JValue::Long(self.token)],
            );
            if result.is_err() {
                clear_exception(env);
            }
            result.map(drop).map_err(|e| e.to_string())
        });
    }
}

//! Haptic feedback via `View.performHapticFeedback`.
//!
//! Not the `Vibrator` service on purpose. `performHapticFeedback` is routed
//! through the system's "Touch feedback" setting, so a user who has switched
//! haptics off gets silence without us checking anything; `Vibrator` ignores
//! that preference and additionally needs the VIBRATE permission. The whole
//! point of this file is a tick that carries information, and a tick the user
//! has asked not to receive carries none.
//!
//! Constants are inlined rather than read back over JNI — they are frozen ABI
//! in `android.view.HapticFeedbackConstants` and have been since API 3.

use jni::objects::JObject;

use super::activity::{activity, clear_exception, jni_env};

/// `HapticFeedbackConstants.LONG_PRESS` — the tick Android itself uses when a
/// long-press arms. Matching it means our hold feels like every other hold on
/// the device.
const LONG_PRESS: i32 = 0;

/// `HapticFeedbackConstants.REJECT` (API 30+). A distinctly different pattern
/// from LONG_PRESS, which is the point: "that did not work" must not feel like
/// "that worked".
const REJECT: i32 = 17;

/// Fire one haptic on the Activity's decor view.
///
/// Every failure path is a `warn` and a return. A missing buzz is not worth
/// risking anything above it, and this is called from UI callbacks where an
/// unwrap would take the window down.
fn perform(effect: i32) {
    let activity = match activity() {
        Ok(a) => a,
        // Pre-`onCreate`. Nothing is on screen to be held, so nothing is lost.
        Err(e) => {
            tracing::debug!("haptics: no activity yet ({e})");
            return;
        }
    };
    let mut env = match jni_env() {
        Ok(env) => env,
        Err(e) => {
            tracing::warn!("haptics: no JNI env ({e})");
            return;
        }
    };

    let result = (|| -> Result<(), jni::errors::Error> {
        let window = env
            .call_method(activity, "getWindow", "()Landroid/view/Window;", &[])?
            .l()?;
        let view = env
            .call_method(&window, "getDecorView", "()Landroid/view/View;", &[])?
            .l()?;
        // The Z return is "was a haptic actually played" — false when the user
        // has feedback disabled or the view opted out. That is a legitimate
        // outcome, not an error, so it is discarded.
        env.call_method(
            &view,
            "performHapticFeedback",
            "(I)Z",
            &[jni::objects::JValue::Int(effect)],
        )?;
        Ok(())
    })();

    if let Err(e) = result {
        clear_exception(&mut env);
        tracing::warn!("haptics: performHapticFeedback failed ({e})");
    }
    // Keep the borrow checker honest about `JObject` lifetimes above.
    let _ = JObject::null();
}

/// A hold gesture just armed and its menu is about to open.
pub fn long_press() {
    perform(LONG_PRESS);
}

/// An action failed. Only ever alongside a visible error, never instead of one.
pub fn error() {
    perform(REJECT);
}

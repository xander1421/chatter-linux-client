//! Binds the `Haptics` Slint global to the platform.
//!
//! Android is the only target with anything to bind. Everywhere else this
//! function is a no-op and the Slint callbacks stay unhandled, which Slint
//! treats as "do nothing" — so `.slint` files can call `Haptics.long-press()`
//! unconditionally and never need a platform check.
//!
//! Keeping the wiring in one function rather than at each call site is the
//! point: haptics are easy to sprinkle and hard to remove, and a single binding
//! site means the answer to "where can this app buzz?" is one screenful.

use slint::ComponentHandle;

use crate::Haptics;

/// Point the `Haptics` global at the platform's feedback API.
pub fn install(window: &crate::MainWindow) {
    let haptics = window.global::<Haptics>();

    #[cfg(target_os = "android")]
    {
        haptics.on_long_press(crate::platform::android::haptics::long_press);
        haptics.on_error(crate::platform::android::haptics::error);
    }

    // Desktop: nothing to bind. Named so the unused binding does not warn and
    // so the reason is visible here rather than inferred from a `cfg`.
    #[cfg(not(target_os = "android"))]
    {
        let _ = haptics;
    }
}

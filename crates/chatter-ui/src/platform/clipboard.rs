//! Clipboard writes shared by the settings and friend-code UI.

/// Write text to the system clipboard. Recovery codes use `secret = true`.
pub fn set_text(label: &str, text: &str, secret: bool) -> Result<(), String> {
    if secret {
        return set_secret_text(label, text);
    }

    #[cfg(target_os = "android")]
    return super::android::clipboard::set_text(label, text);

    #[cfg(target_os = "ios")]
    return ios_set_text(text, false);

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        let _ = label;
        #[cfg(target_os = "linux")]
        {
            if std::process::Command::new("wl-copy")
                .arg(text)
                .status()
                .is_ok_and(|status| status.success())
            {
                return Ok(());
            }
            if std::process::Command::new("xclip")
                .args(["-selection", "clipboard"])
                .stdin(std::process::Stdio::piped())
                .spawn()
                .and_then(|mut child| {
                    use std::io::Write;
                    if let Some(stdin) = child.stdin.as_mut() {
                        stdin.write_all(text.as_bytes())?;
                    }
                    child.wait()
                })
                .is_ok_and(|status| status.success())
            {
                return Ok(());
            }
        }
        let mut clipboard = arboard::Clipboard::new().map_err(|e| e.to_string())?;
        clipboard.set_text(text).map_err(|e| e.to_string())
    }
}

/// Recovery codes unlock a backup, so their clipboard lifetime is limited.
fn set_secret_text(label: &str, text: &str) -> Result<(), String> {
    #[cfg(target_os = "ios")]
    {
        let _ = label;
        return ios_set_text(text, true);
    }

    #[cfg(not(target_os = "ios"))]
    {
        #[cfg(target_os = "android")]
        super::android::clipboard::set_text(label, text)?;
        #[cfg(not(target_os = "android"))]
        {
            let _ = label;
            // Keep the write and timed clear on the same desktop backend.
            let mut clipboard = arboard::Clipboard::new().map_err(|e| e.to_string())?;
            clipboard.set_text(text).map_err(|e| e.to_string())?;
        }
        std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_secs(60));
            #[cfg(target_os = "android")]
            match super::android::clipboard::set_text("", "") {
                Ok(()) => tracing::info!("Clipboard auto-cleared after 60s"),
                Err(e) => tracing::warn!("Failed to auto-clear clipboard: {e}"),
            }
            #[cfg(not(target_os = "android"))]
            if let Err(e) = slint::invoke_from_event_loop(|| {
                if let Ok(mut clipboard) = arboard::Clipboard::new() {
                    if let Err(e) = clipboard.clear() {
                        tracing::warn!("Failed to auto-clear clipboard: {e}");
                    } else {
                        tracing::info!("Clipboard auto-cleared after 60s");
                    }
                }
            }) {
                tracing::warn!("Failed to auto-clear clipboard: {e}");
            }
        });
        Ok(())
    }
}

#[cfg(target_os = "ios")]
fn ios_set_text(text: &str, secret: bool) -> Result<(), String> {
    use std::ffi::{CString, c_char};

    unsafe extern "C" {
        fn chatter_ios_clipboard_set_text(text: *const c_char, secret: bool) -> bool;
    }

    let text = CString::new(text).map_err(|_| "clipboard text contains a NUL byte".to_string())?;
    // The Objective-C bridge dispatches to the main queue when this is called
    // from an async worker (the safety number callback does that).
    if unsafe { chatter_ios_clipboard_set_text(text.as_ptr(), secret) } {
        Ok(())
    } else {
        Err("UIPasteboard rejected clipboard text".into())
    }
}

//! Cross-platform hardware-accelerated video rendering.
//!
//! Provides platform-specific GPU video rendering:
//! - Linux: EGL + GStreamer glsinkbin (zero-copy GL textures)
//! - Android: EGL + GStreamer glsinkbin (via NDK)
//! - Windows: D3D11 + GStreamer d3d11 (DXVA2/D3D11VA hardware decoding)
//! - iOS: Metal + GStreamer applemedia (VideoToolbox hardware decoding)
//!
//! Falls back to software rendering if hardware acceleration unavailable.

#[cfg(target_os = "linux")]
mod gl_linux;

/// Call-video GL texture uploader (Linux). Persistent per-participant GL
/// textures updated in place, fed by `timers.rs` and uploaded inside
/// `gl_linux`'s rendering notifier. Public so `features::calls::timers` can
/// publish frames into its inbox.
#[cfg(target_os = "linux")]
pub mod gl_call;

#[cfg(all(target_os = "android", feature = "desktop-media"))]
mod gl_android;

#[cfg(target_os = "windows")]
mod d3d11_windows;

#[cfg(target_os = "ios")]
mod metal_ios;

use crate::MainWindow;

/// Initialize hardware-accelerated video for the window.
/// Returns true if hardware video was successfully initialized.
pub fn init_hw_video(window: &MainWindow) -> bool {
    #[cfg(target_os = "linux")]
    {
        gl_linux::init_gl_video(window)
    }

    #[cfg(all(target_os = "android", feature = "desktop-media"))]
    {
        gl_android::init_gl_video(window)
    }

    #[cfg(target_os = "windows")]
    {
        d3d11_windows::init_d3d11_video(window)
    }

    #[cfg(target_os = "ios")]
    {
        metal_ios::init_metal_video(window)
    }

    #[cfg(not(any(target_os = "linux", all(target_os = "android", feature = "desktop-media"), target_os = "windows", target_os = "ios")))]
    {
        let _ = window;
        tracing::info!("Hardware video not supported on this platform");
        false
    }
}

/// Check if hardware video is available.
pub fn is_hw_available() -> bool {
    #[cfg(target_os = "linux")]
    {
        gl_linux::is_gl_available()
    }

    #[cfg(all(target_os = "android", feature = "desktop-media"))]
    {
        gl_android::is_gl_available()
    }

    #[cfg(target_os = "windows")]
    {
        d3d11_windows::is_d3d11_available()
    }

    #[cfg(target_os = "ios")]
    {
        metal_ios::is_metal_available()
    }

    #[cfg(not(any(target_os = "linux", all(target_os = "android", feature = "desktop-media"), target_os = "windows", target_os = "ios")))]
    {
        false
    }
}

/// Check if hardware video context is connected and ready.
pub fn is_hw_connected() -> bool {
    #[cfg(target_os = "linux")]
    {
        gl_linux::is_gl_connected()
    }

    #[cfg(all(target_os = "android", feature = "desktop-media"))]
    {
        gl_android::is_gl_connected()
    }

    #[cfg(target_os = "windows")]
    {
        d3d11_windows::is_d3d11_connected()
    }

    #[cfg(target_os = "ios")]
    {
        metal_ios::is_metal_connected()
    }

    #[cfg(not(any(target_os = "linux", all(target_os = "android", feature = "desktop-media"), target_os = "windows", target_os = "ios")))]
    {
        false
    }
}

/// Start hardware-accelerated video playback from memory.
/// Returns duration in seconds, or None if failed.
pub fn start_hw_video(data: Vec<u8>) -> Option<f64> {
    #[cfg(target_os = "linux")]
    {
        gl_linux::start_gl_video(data)
    }

    #[cfg(all(target_os = "android", feature = "desktop-media"))]
    {
        gl_android::start_gl_video(data)
    }

    #[cfg(target_os = "windows")]
    {
        d3d11_windows::start_d3d11_video(data)
    }

    #[cfg(target_os = "ios")]
    {
        metal_ios::start_metal_video(data)
    }

    #[cfg(not(any(target_os = "linux", all(target_os = "android", feature = "desktop-media"), target_os = "windows", target_os = "ios")))]
    {
        let _ = data;
        None
    }
}

/// Start hardware-accelerated video from an authenticated range decryptor
/// (streaming — no full decrypt-to-memory). Returns duration, or None if the
/// platform/backend can't stream (caller should fall back to software).
pub fn start_hw_video_range(
    dec: std::sync::Arc<chatter_core::services::attachment::CbcRangeDecryptor>,
) -> Option<f64> {
    #[cfg(target_os = "linux")]
    {
        gl_linux::start_gl_video_range(dec)
    }

    #[cfg(not(target_os = "linux"))]
    {
        // Range streaming is wired for the GL/Linux backend only so far; other
        // platforms fall back to the in-memory path.
        let _ = dec;
        None
    }
}

/// Stop hardware video playback.
pub fn stop_hw_video() {
    #[cfg(target_os = "linux")]
    {
        gl_linux::stop_gl_video();
    }

    #[cfg(all(target_os = "android", feature = "desktop-media"))]
    {
        gl_android::stop_gl_video();
    }

    #[cfg(target_os = "windows")]
    {
        d3d11_windows::stop_d3d11_video();
    }

    #[cfg(target_os = "ios")]
    {
        metal_ios::stop_metal_video();
    }
}

/// Set hardware video playing state.
pub fn set_hw_video_playing(playing: bool) {
    #[cfg(target_os = "linux")]
    {
        gl_linux::set_gl_video_playing(playing);
    }

    #[cfg(all(target_os = "android", feature = "desktop-media"))]
    {
        gl_android::set_gl_video_playing(playing);
    }

    #[cfg(target_os = "windows")]
    {
        d3d11_windows::set_d3d11_video_playing(playing);
    }

    #[cfg(target_os = "ios")]
    {
        metal_ios::set_metal_video_playing(playing);
    }
}

/// Check if hardware video is currently active.
pub fn is_hw_video_active() -> bool {
    #[cfg(target_os = "linux")]
    {
        gl_linux::is_gl_video_active()
    }

    #[cfg(all(target_os = "android", feature = "desktop-media"))]
    {
        gl_android::is_gl_video_active()
    }

    #[cfg(target_os = "windows")]
    {
        d3d11_windows::is_d3d11_video_active()
    }

    #[cfg(target_os = "ios")]
    {
        metal_ios::is_metal_video_active()
    }

    #[cfg(not(any(target_os = "linux", all(target_os = "android", feature = "desktop-media"), target_os = "windows", target_os = "ios")))]
    {
        false
    }
}

/// Get hardware video position in seconds.
pub fn get_hw_video_position() -> Option<f64> {
    #[cfg(target_os = "linux")]
    {
        gl_linux::get_gl_video_position()
    }

    #[cfg(all(target_os = "android", feature = "desktop-media"))]
    {
        gl_android::get_gl_video_position()
    }

    #[cfg(target_os = "windows")]
    {
        d3d11_windows::get_d3d11_video_position()
    }

    #[cfg(target_os = "ios")]
    {
        metal_ios::get_metal_video_position()
    }

    #[cfg(not(any(target_os = "linux", all(target_os = "android", feature = "desktop-media"), target_os = "windows", target_os = "ios")))]
    {
        None
    }
}

/// Get hardware video duration in seconds.
pub fn get_hw_video_duration() -> Option<f64> {
    #[cfg(target_os = "linux")]
    {
        gl_linux::get_gl_video_duration()
    }

    #[cfg(all(target_os = "android", feature = "desktop-media"))]
    {
        gl_android::get_gl_video_duration()
    }

    #[cfg(target_os = "windows")]
    {
        d3d11_windows::get_d3d11_video_duration()
    }

    #[cfg(target_os = "ios")]
    {
        metal_ios::get_metal_video_duration()
    }

    #[cfg(not(any(target_os = "linux", all(target_os = "android", feature = "desktop-media"), target_os = "windows", target_os = "ios")))]
    {
        None
    }
}

/// Seek hardware video to position in seconds.
pub fn seek_hw_video(position: f64) {
    #[cfg(target_os = "linux")]
    {
        gl_linux::seek_gl_video(position);
    }

    #[cfg(all(target_os = "android", feature = "desktop-media"))]
    {
        gl_android::seek_gl_video(position);
    }

    #[cfg(target_os = "windows")]
    {
        d3d11_windows::seek_d3d11_video(position);
    }

    #[cfg(target_os = "ios")]
    {
        metal_ios::seek_metal_video(position);
    }

    #[cfg(not(any(target_os = "linux", all(target_os = "android", feature = "desktop-media"), target_os = "windows", target_os = "ios")))]
    {
        let _ = position;
    }
}

/// Set hardware video volume (0.0 - 1.0).
pub fn set_hw_video_volume(volume: f64) {
    #[cfg(target_os = "linux")]
    {
        gl_linux::set_gl_video_volume(volume);
    }

    #[cfg(all(target_os = "android", feature = "desktop-media"))]
    {
        gl_android::set_gl_video_volume(volume);
    }

    #[cfg(target_os = "windows")]
    {
        d3d11_windows::set_d3d11_video_volume(volume);
    }

    #[cfg(target_os = "ios")]
    {
        metal_ios::set_metal_video_volume(volume);
    }

    #[cfg(not(any(target_os = "linux", all(target_os = "android", feature = "desktop-media"), target_os = "windows", target_os = "ios")))]
    {
        let _ = volume;
    }
}

/// Apply pending volume if audio is now ready (called during playback updates).
pub fn apply_pending_hw_volume() {
    #[cfg(target_os = "linux")]
    {
        gl_linux::apply_pending_volume();
    }

    // Other platforms don't have deferred volume handling
    #[cfg(not(target_os = "linux"))]
    {}
}

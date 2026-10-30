//! Small format/identity policies shared by capture, playback and pickers.
use std::sync::atomic::{AtomicU32, Ordering};

/// Separate from every host-qualified CPAL device ID; never an empty selection.
pub const SYSTEM_DEFAULT: &str = "chatter:system-default";

pub const fn android_rate_for_sdk(sdk: i32) -> u32 {
    // Legacy AudioManager SCO: mono 8 kHz input, mono 8/16 kHz output.
    // Keep both streams at 8 kHz for the session, including speaker transitions.
    if sdk < 31 { 8_000 } else { 48_000 }
}
static ANDROID_STREAM_RATE: AtomicU32 = AtomicU32::new(8_000);
pub fn set_android_sdk(sdk: i32) {
    ANDROID_STREAM_RATE.store(android_rate_for_sdk(sdk), Ordering::Relaxed);
}
pub fn android_stream_rate() -> u32 {
    ANDROID_STREAM_RATE.load(Ordering::Relaxed)
}

/// WebRTC's remix/resample API consumes 10 ms. Preserve partial callbacks and
/// discard a partial OLD format on device change, never join unlike PCM frames.
#[derive(Default)]
pub struct CaptureChunks {
    format: (u32, u32),
    pending: Vec<i16>,
}
impl CaptureChunks {
    pub fn push(&mut self, samples: Vec<i16>, rate: u32, channels: u32) -> Vec<Vec<i16>> {
        if self.format != (rate, channels) {
            self.pending.clear();
            self.format = (rate, channels);
        }
        if rate == 0 || rate % 100 != 0 || channels == 0 {
            return vec![];
        }
        self.pending.extend(samples);
        let size = (rate / 100 * channels) as usize;
        let complete = self.pending.len() / size * size;
        let chunks = self.pending[..complete]
            .chunks_exact(size)
            .map(|s| s.to_vec())
            .collect();
        self.pending.drain(..complete);
        chunks
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn partial_callbacks_preserve_exact_ten_ms_boundaries() {
        let mut c = CaptureChunks::default();
        assert!(c.push(vec![1; 113], 48_000, 1).is_empty());
        let frames = c.push(vec![2; 847], 48_000, 1);
        assert_eq!(
            frames.iter().map(Vec::len).collect::<Vec<_>>(),
            vec![480, 480]
        );
        assert_eq!(&frames[0][..113], &[1; 113]);
        assert_eq!(&frames[0][113..], &[2; 367]);
    }
    #[test]
    fn hotplug_48k_mono_to_44k1_stereo_drops_old_partial_only() {
        let mut c = CaptureChunks::default();
        assert!(c.push(vec![7; 100], 48_000, 1).is_empty());
        let frames = c.push(vec![9; 8_820], 44_100, 2);
        assert_eq!(frames.len(), 10);
        assert!(
            frames
                .iter()
                .all(|f| f.len() == 882 && f.iter().all(|&s| s == 9))
        );
    }
    #[test]
    fn legacy_sco_and_modern_formats_are_explicit() {
        assert_eq!(android_rate_for_sdk(29), 8_000);
        assert_eq!(android_rate_for_sdk(30), 8_000);
        assert_eq!(android_rate_for_sdk(31), 48_000);
        let mut c = CaptureChunks::default();
        assert_eq!(c.push(vec![0; 80], 8_000, 1)[0].len(), 80);
    }
    #[test]
    fn default_key_is_nonempty_and_outside_native_host_namespace() {
        assert_eq!(SYSTEM_DEFAULT, "chatter:system-default");
    }
}

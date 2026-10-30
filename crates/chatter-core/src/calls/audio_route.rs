//! Call audio routing policy for phones (speaker / earpiece / headset).
//!
//! Platform-neutral decision logic; the platform layer (chatter-ui's Android
//! bridge over `AudioManager`) reports which outputs exist and applies the
//! route this returns. Desktop does not use it — there the user picks an
//! output device from the list.
//!
//! The rules follow Signal-Android's `SignalAudioManager`:
//! - a voice call starts on the earpiece, a video call on the speaker;
//! - a headset (Bluetooth preferred over wired) takes over when present;
//! - connecting or disconnecting a headset drops the user's speaker choice, so
//!   the new device takes over (or the call falls back to its default);
//! - turning the camera on moves the call off the earpiece; turning it off
//!   leaves the audio where it is;
//! - a device with no earpiece (a tablet) uses the speaker instead.

/// Where call audio plays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioRoute {
    Earpiece,
    Speaker,
    /// Wired or USB headset / headphones.
    WiredHeadset,
    Bluetooth,
}

impl AudioRoute {
    /// Stable integer for the JNI bridge (`MainActivity.applyAudioRoute`).
    pub fn as_i32(self) -> i32 {
        match self {
            AudioRoute::Earpiece => 0,
            AudioRoute::Speaker => 1,
            AudioRoute::WiredHeadset => 2,
            AudioRoute::Bluetooth => 3,
        }
    }
}

/// The outputs the platform currently reports as usable for a call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioOutputs {
    pub earpiece: bool,
    pub wired_headset: bool,
    pub bluetooth: bool,
}

impl Default for AudioOutputs {
    /// A phone with nothing plugged in — what is assumed until the platform
    /// reports otherwise.
    fn default() -> Self {
        Self {
            earpiece: true,
            wired_headset: false,
            bluetooth: false,
        }
    }
}

/// Routing state for one call.
#[derive(Debug, Clone)]
pub struct CallAudioRouting {
    video: bool,
    outputs: AudioOutputs,
    /// The user's speaker toggle: `Some(true)` = speaker, `Some(false)` = off
    /// the speaker. `None` = no choice made, use the call's default.
    speaker_choice: Option<bool>,
}

impl CallAudioRouting {
    pub fn new(video: bool) -> Self {
        Self {
            video,
            outputs: AudioOutputs::default(),
            speaker_choice: None,
        }
    }

    /// The route the call should be on now.
    pub fn route(&self) -> AudioRoute {
        if self.speaker_choice == Some(true) {
            return AudioRoute::Speaker;
        }
        if self.outputs.bluetooth {
            return AudioRoute::Bluetooth;
        }
        if self.outputs.wired_headset {
            return AudioRoute::WiredHeadset;
        }
        let speaker = self.speaker_choice.map_or(self.video, |s| s);
        if speaker || !self.outputs.earpiece {
            AudioRoute::Speaker
        } else {
            AudioRoute::Earpiece
        }
    }

    pub fn speaker_on(&self) -> bool {
        self.route() == AudioRoute::Speaker
    }

    /// The platform reported a new set of outputs. A headset appearing or
    /// disappearing clears the speaker choice so the change takes effect.
    pub fn set_outputs(&mut self, outputs: AudioOutputs) -> AudioRoute {
        let headsets = |o: &AudioOutputs| (o.wired_headset, o.bluetooth);
        if headsets(&outputs) != headsets(&self.outputs) {
            self.speaker_choice = None;
        }
        self.outputs = outputs;
        self.route()
    }

    /// The speaker button: off the speaker goes back to the headset if one is
    /// connected, else the earpiece. On a device with no earpiece and no
    /// headset there is nowhere else to go and the route stays on the speaker.
    pub fn toggle_speaker(&mut self) -> AudioRoute {
        self.speaker_choice = Some(!self.speaker_on());
        self.route()
    }

    /// The local camera was turned on or off mid-call.
    pub fn set_video(&mut self, video: bool) -> AudioRoute {
        if video {
            if self.route() == AudioRoute::Earpiece {
                self.speaker_choice = Some(true);
            }
        } else if self.speaker_choice.is_none() {
            // Pin the current route so dropping the camera doesn't move audio.
            self.speaker_choice = Some(self.speaker_on());
        }
        self.video = video;
        self.route()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEADSET: AudioOutputs = AudioOutputs {
        earpiece: true,
        wired_headset: true,
        bluetooth: false,
    };
    const BT: AudioOutputs = AudioOutputs {
        earpiece: true,
        wired_headset: false,
        bluetooth: true,
    };
    const TABLET: AudioOutputs = AudioOutputs {
        earpiece: false,
        wired_headset: false,
        bluetooth: false,
    };

    #[test]
    fn defaults_voice_earpiece_video_speaker() {
        assert_eq!(CallAudioRouting::new(false).route(), AudioRoute::Earpiece);
        assert_eq!(CallAudioRouting::new(true).route(), AudioRoute::Speaker);
    }

    #[test]
    fn headset_takes_over_and_unplug_falls_back() {
        let mut r = CallAudioRouting::new(false);
        r.toggle_speaker();
        assert_eq!(r.route(), AudioRoute::Speaker);
        assert_eq!(r.set_outputs(HEADSET), AudioRoute::WiredHeadset);
        assert_eq!(r.set_outputs(AudioOutputs::default()), AudioRoute::Earpiece);
        let mut v = CallAudioRouting::new(true);
        assert_eq!(v.set_outputs(BT), AudioRoute::Bluetooth);
        assert_eq!(v.set_outputs(AudioOutputs::default()), AudioRoute::Speaker);
    }

    #[test]
    fn toggle_with_headset_returns_to_headset() {
        let mut r = CallAudioRouting::new(false);
        r.set_outputs(BT);
        assert_eq!(r.toggle_speaker(), AudioRoute::Speaker);
        assert_eq!(r.toggle_speaker(), AudioRoute::Bluetooth);
        // Unrelated report (same headsets) keeps the choice.
        r.toggle_speaker();
        assert_eq!(r.set_outputs(BT), AudioRoute::Speaker);
    }

    #[test]
    fn tablet_without_earpiece_stays_on_speaker() {
        let mut r = CallAudioRouting::new(false);
        assert_eq!(r.set_outputs(TABLET), AudioRoute::Speaker);
        assert_eq!(r.toggle_speaker(), AudioRoute::Speaker);
    }

    #[test]
    fn camera_on_leaves_earpiece_camera_off_keeps_route() {
        let mut r = CallAudioRouting::new(false);
        assert_eq!(r.set_video(true), AudioRoute::Speaker);
        assert_eq!(r.set_video(false), AudioRoute::Speaker);
        let mut v = CallAudioRouting::new(true);
        assert_eq!(v.set_video(false), AudioRoute::Speaker);
    }
}

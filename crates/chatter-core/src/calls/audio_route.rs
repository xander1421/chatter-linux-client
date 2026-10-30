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
//!
//! `CommunicationRouting` adds explicit per-device intent above these defaults;
//! an explicit choice survives inventory changes and temporarily falls back if absent.

use super::audio_format::SYSTEM_DEFAULT;

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

/// One OS-supported communication route. Input and output are a pair on Android.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommunicationDevice {
    /// Stable platform key, retained across reconnects and hotplug.
    pub id: String,
    pub name: String,
    pub route: AudioRoute,
}

/// Individual device selection layered over the voice/video default policy.
/// Missing requested devices fall back temporarily without forgetting intent.
#[derive(Debug, Clone)]
pub struct CommunicationRouting {
    pub policy: CallAudioRouting,
    devices: Vec<CommunicationDevice>,
    requested: Option<String>,
    applied: Option<String>,
    failed: Vec<String>,
}

impl CommunicationRouting {
    pub fn new(video: bool) -> Self {
        Self {
            policy: CallAudioRouting::new(video),
            devices: vec![],
            requested: None,
            applied: None,
            failed: vec![],
        }
    }

    pub fn devices(&self) -> &[CommunicationDevice] {
        &self.devices
    }
    pub fn requested(&self) -> Option<&str> {
        self.requested.as_deref()
    }
    pub fn applied(&self) -> Option<&str> {
        self.applied.as_deref()
    }

    pub fn refresh(&mut self, devices: Vec<CommunicationDevice>, applied: Option<String>) {
        if self.devices != devices {
            self.failed.clear();
        }
        self.policy.set_outputs(AudioOutputs {
            earpiece: devices.iter().any(|d| d.route == AudioRoute::Earpiece),
            wired_headset: devices.iter().any(|d| d.route == AudioRoute::WiredHeadset),
            bluetooth: devices.iter().any(|d| d.route == AudioRoute::Bluetooth),
        });
        self.devices = devices;
        self.applied = applied.filter(|id| self.devices.iter().any(|d| &d.id == id));
    }

    pub fn select(&mut self, id: &str) -> bool {
        self.failed.clear();
        if id == SYSTEM_DEFAULT {
            self.requested = None;
            return true;
        }
        if !self.devices.iter().any(|d| d.id == id) {
            return false;
        }
        self.requested = Some(id.to_owned());
        true
    }

    pub fn fail(&mut self, id: &str) {
        if !self.failed.iter().any(|failed| failed == id) {
            self.failed.push(id.to_owned());
        }
    }

    pub fn target(&self) -> Option<&CommunicationDevice> {
        let usable = |d: &&CommunicationDevice| !self.failed.contains(&d.id);
        self.requested
            .as_ref()
            .and_then(|id| self.devices.iter().filter(usable).find(|d| &d.id == id))
            .or_else(|| {
                self.devices
                    .iter()
                    .filter(usable)
                    .find(|d| d.route == self.policy.route())
            })
            .or_else(|| self.devices.iter().filter(usable).next())
    }

    /// Labels expose only combinations Android actually offers.
    pub fn picker_devices(&self) -> Vec<(String, String, bool)> {
        let default = self.target();
        let status = if !self.failed.is_empty() {
            "Fallback (route unavailable)"
        } else if self.requested.is_none() {
            "Automatic (selected)"
        } else if !self
            .devices
            .iter()
            .any(|d| Some(d.id.as_str()) == self.requested())
        {
            "Fallback (preferred device disconnected)"
        } else {
            "Automatic"
        };
        let mut rows = vec![(
            SYSTEM_DEFAULT.to_owned(),
            format!(
                "{} — {}",
                status,
                default.map_or("system route", |d| d.name.as_str())
            ),
            true,
        )];
        rows.extend(self.devices.iter().map(|d| {
            let input = match d.route {
                AudioRoute::Earpiece => "built-in mic",
                _ => "system-routed mic",
            };
            let pending = if self.failed.contains(&d.id) {
                " · unavailable"
            } else if Some(d.id.as_str()) == self.requested() && self.applied() != self.requested()
            {
                " · requested"
            } else {
                ""
            };
            (
                d.id.clone(),
                format!("{} + {} (linked){}", d.name, input, pending),
                false,
            )
        }));
        rows
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
    fn device(id: &str, route: AudioRoute) -> CommunicationDevice {
        CommunicationDevice {
            id: id.into(),
            name: id.into(),
            route,
        }
    }

    #[test]
    fn individual_headsets_are_selectable_and_ack_is_independent() {
        let mut r = CommunicationRouting::new(false);
        let devices = vec![
            device("phone", AudioRoute::Earpiece),
            device("bt-one", AudioRoute::Bluetooth),
            device("bt-two", AudioRoute::Bluetooth),
        ];
        r.refresh(devices.clone(), Some("bt-one".into()));
        assert!(r.select("bt-two"));
        assert_eq!(r.target().unwrap().id, "bt-two");
        assert_eq!(r.applied(), Some("bt-one")); // Request is not acknowledgement.
        r.refresh(devices, Some("bt-two".into()));
        assert_eq!(r.applied(), Some("bt-two"));
        assert!(!r.select("missing"));
        assert_eq!(r.requested(), Some("bt-two"));
    }

    #[test]
    fn unplug_falls_back_and_replug_restores_intent() {
        let mut r = CommunicationRouting::new(false);
        let phone = device("phone", AudioRoute::Earpiece);
        let bt = device("headset", AudioRoute::Bluetooth);
        r.refresh(vec![phone.clone(), bt.clone()], Some("headset".into()));
        r.select("headset");
        r.refresh(vec![phone.clone()], Some("headset".into()));
        assert_eq!(r.applied(), None);
        assert_eq!(r.target().unwrap().id, "phone");
        assert_eq!(r.requested(), Some("headset"));
        assert!(r.picker_devices()[0].1.contains("disconnected"));
        r.refresh(vec![phone, bt], Some("phone".into()));
        assert_eq!(r.target().unwrap().id, "headset");
    }

    #[test]
    fn explicit_builtin_route_survives_headset_and_video_changes() {
        let mut r = CommunicationRouting::new(false);
        let phone = device("phone", AudioRoute::Earpiece);
        r.refresh(vec![phone.clone()], Some("phone".into()));
        r.select("phone");
        r.refresh(
            vec![phone, device("bt", AudioRoute::Bluetooth)],
            Some("phone".into()),
        );
        r.policy.set_video(true);
        assert_eq!(r.target().unwrap().id, "phone");
        r.select(SYSTEM_DEFAULT);
        assert_eq!(r.target().unwrap().id, "bt");
    }

    #[test]
    fn tablet_and_empty_inventory_never_offer_nonexistent_routes() {
        let mut r = CommunicationRouting::new(false);
        r.refresh(vec![device("speaker", AudioRoute::Speaker)], None);
        assert_eq!(r.target().unwrap().id, "speaker");
        assert!(!r.select("earpiece"));
        assert_eq!(r.picker_devices().len(), 2);
        assert!(r.picker_devices()[1].1.contains("linked"));
        r.refresh(vec![], None);
        assert_eq!(r.target(), None);
    }
    #[test]
    fn failed_route_falls_back_without_forging_ack_and_can_be_retried() {
        let mut r = CommunicationRouting::new(false);
        let devices = vec![
            device("phone", AudioRoute::Earpiece),
            device("bt", AudioRoute::Bluetooth),
        ];
        r.refresh(devices.clone(), Some("phone".into()));
        r.select("bt");
        r.fail("bt");
        assert_eq!(r.target().unwrap().id, "phone");
        assert_eq!(r.applied(), Some("phone"));
        assert_eq!(r.requested(), Some("bt"));
        r.refresh(devices, Some("phone".into()));
        assert_eq!(r.target().unwrap().id, "phone");
        r.select("bt");
        assert_eq!(r.target().unwrap().id, "bt");
    }
    #[test]
    fn picker_rows_have_unique_nonempty_ids_and_one_default() {
        let mut r = CommunicationRouting::new(false);
        r.refresh(
            vec![
                device("phone", AudioRoute::Earpiece),
                device("bt", AudioRoute::Bluetooth),
            ],
            Some("phone".into()),
        );
        let rows = r.picker_devices();
        assert!(rows.iter().all(|(id, _, _)| !id.is_empty()));
        assert_eq!(
            rows.iter()
                .map(|(id, _, _)| id)
                .collect::<std::collections::HashSet<_>>()
                .len(),
            rows.len()
        );
        assert_eq!(rows.iter().filter(|(_, _, default)| *default).count(), 1);
    }
}

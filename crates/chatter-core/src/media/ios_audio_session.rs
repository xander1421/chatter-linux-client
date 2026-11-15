//! Own the iOS audio session while CPAL input/output streams use it.
//! Recording initialization runs on the recorder worker, never the UI thread.
use std::sync::{Mutex, mpsc};
use std::time::Duration;

use block2::RcBlock;
use objc2_avf_audio::{
    AVAudioSession, AVAudioSessionCategoryOptions, AVAudioSessionCategoryPlayAndRecord,
    AVAudioSessionCategoryPlayback, AVAudioSessionModeDefault, AVAudioSessionRecordPermission,
    AVAudioSessionSetActiveOptions,
};

use super::audio_recorder::MICROPHONE_PERMISSION_DENIED;

static USERS: Mutex<usize> = Mutex::new(0);

pub(crate) struct AudioSession;

impl AudioSession {
    pub(crate) fn request_recording_permission() -> Result<(), String> {
        let session = unsafe { AVAudioSession::sharedInstance() };
        #[allow(deprecated)] // iOS 16 remains supported.
        let permission = unsafe { session.recordPermission() };
        if permission == AVAudioSessionRecordPermission::Granted {
            return Ok(());
        }
        if permission == AVAudioSessionRecordPermission::Denied {
            return Err(MICROPHONE_PERMISSION_DENIED.into());
        }
        let (tx, rx) = mpsc::sync_channel(1);
        let completion = RcBlock::new(move |granted: objc2::runtime::Bool| {
            let _ = tx.try_send(granted.as_bool());
        });
        #[allow(deprecated)]
        unsafe {
            session.requestRecordPermission(&completion)
        };
        if rx.recv_timeout(Duration::from_secs(120)).unwrap_or(false) {
            Ok(())
        } else {
            Err(MICROPHONE_PERMISSION_DENIED.into())
        }
    }

    /// Ask only for recordings. Playback never prompts for microphone access.
    pub(crate) fn acquire(recording: bool) -> Result<Self, String> {
        // AVAudioSession is a process-wide, thread-safe singleton. All category
        // changes and reference counts are serialized below; permission waits
        // happen outside the lock so an alert cannot block playback cleanup.
        let session = unsafe { AVAudioSession::sharedInstance() };
        if recording {
            Self::request_recording_permission()?;
        }
        let mut users = USERS.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            if recording {
                session.setCategory_mode_options_error(
                    AVAudioSessionCategoryPlayAndRecord
                        .ok_or("Recording audio category unavailable")?,
                    AVAudioSessionModeDefault.ok_or("Default audio mode unavailable")?,
                    AVAudioSessionCategoryOptions::DefaultToSpeaker,
                )
            } else if *users == 0 {
                session.setCategory_mode_options_error(
                    AVAudioSessionCategoryPlayback.ok_or("Playback audio category unavailable")?,
                    AVAudioSessionModeDefault.ok_or("Default audio mode unavailable")?,
                    AVAudioSessionCategoryOptions::empty(),
                )
            } else {
                Ok(())
            }
            .map_err(|e| format!("Could not configure iPhone audio: {e}"))?;
            session
                .setActive_error(true)
                .map_err(|e| format!("Could not activate iPhone audio: {e}"))?;
        }
        *users += 1;
        Ok(Self)
    }
}

impl Drop for AudioSession {
    fn drop(&mut self) {
        let mut users = USERS.lock().unwrap_or_else(|e| e.into_inner());
        *users -= 1;
        if *users == 0 {
            // The owning CPAL stream must be dropped first. Release the audio
            // session only after the final input/output stream has closed.
            let result = unsafe {
                AVAudioSession::sharedInstance().setActive_withOptions_error(
                    false,
                    AVAudioSessionSetActiveOptions::NotifyOthersOnDeactivation,
                )
            };
            if let Err(error) = result {
                tracing::warn!(%error, "Could not release iPhone audio session");
            }
        }
    }
}

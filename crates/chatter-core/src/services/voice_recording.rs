//! Conversation-bound microphone recording. The non-Send CPAL stream stays on
//! one worker thread; dropping the handle cancels and discards the recording.
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

use crate::media::AudioRecorder;
use crate::state::{ActiveConversation, ChatState};

pub const MAX_RECORDING_SECONDS: u64 = 300;

pub enum RecordingEvent {
    Started,
    Elapsed(u64),
    Level(u8),
    Ready,
    Finished(Result<RecordedVoiceNote, String>),
    Cancelled,
}

enum Command {
    Finish,
    Cancel,
}

pub struct RecordingSession {
    commands: Sender<Command>,
}

pub struct RecordedVoiceNote {
    pub(crate) file: tempfile::NamedTempFile,
    pub(crate) conversation: ActiveConversation,
    pub(crate) waveform: Vec<u8>,
    pub(crate) duration_ms: u64,
}

impl RecordedVoiceNote {
    pub fn conversation(&self) -> ActiveConversation {
        self.conversation
    }
}

impl RecordingSession {
    pub fn start(
        chat: ChatState,
        conversation: ActiveConversation,
        cache: std::path::PathBuf,
    ) -> (Self, tokio::sync::mpsc::UnboundedReceiver<RecordingEvent>) {
        let (commands, receiver) = mpsc::channel();
        let (events, output) = tokio::sync::mpsc::unbounded_channel();
        std::thread::spawn(move || {
            if !recording_context_is_current(&chat, conversation) {
                let _ = events.send(RecordingEvent::Cancelled);
                return;
            }
            let result = record(receiver, &events, &chat, conversation, &cache);
            match result {
                Ok(Some(note)) => {
                    let _ = events.send(RecordingEvent::Finished(Ok(note)));
                }
                Ok(None) => {
                    let _ = events.send(RecordingEvent::Cancelled);
                }
                Err(error) => {
                    let _ = events.send(RecordingEvent::Finished(Err(error)));
                }
            }
        });
        (Self { commands }, output)
    }

    pub fn finish(&self) {
        let _ = self.commands.send(Command::Finish);
    }
}

impl Drop for RecordingSession {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Cancel);
    }
}

pub fn recording_context_is_current(chat: &ChatState, conversation: ActiveConversation) -> bool {
    conversation != ActiveConversation::None
        && chat.active_conversation() == conversation
        && chat.is_app_visible()
}

fn record(
    commands: Receiver<Command>,
    events: &tokio::sync::mpsc::UnboundedSender<RecordingEvent>,
    chat: &ChatState,
    conversation: ActiveConversation,
    cache: &std::path::Path,
) -> Result<Option<RecordedVoiceNote>, String> {
    let mut recorder = AudioRecorder::new().map_err(|e| e.to_string())?;
    recorder.start().map_err(|e| e.to_string())?;
    let _ = events.send(RecordingEvent::Started);
    let mut stopped = false;
    let mut elapsed = 0;
    loop {
        if !recording_context_is_current(chat, conversation) || events.is_closed() {
            recorder.cancel();
            return Ok(None);
        }
        match commands.recv_timeout(Duration::from_millis(40)) {
            Ok(Command::Cancel) | Err(RecvTimeoutError::Disconnected) => {
                recorder.cancel();
                return Ok(None);
            }
            Ok(Command::Finish) => break,
            Err(RecvTimeoutError::Timeout) => {}
        }
        if !stopped {
            let _ = events.send(RecordingEvent::Level(recorder.take_live_level()));
            let seconds = recorder.duration().unwrap_or_default().as_secs();
            if seconds != elapsed {
                elapsed = seconds;
                let _ = events.send(RecordingEvent::Elapsed(seconds));
            }
            if seconds >= MAX_RECORDING_SECONDS {
                recorder.stop().map_err(|e| e.to_string())?;
                stopped = true;
                let _ = events.send(RecordingEvent::Ready);
            }
        }
    }
    let prepare_start = std::time::Instant::now();
    if !stopped {
        recorder.stop().map_err(|e| e.to_string())?;
    }
    let file = tempfile::Builder::new()
        .prefix("voice-")
        .suffix(".wav")
        .tempfile_in(cache)
        .map_err(|e| e.to_string())?;
    let encode_start = std::time::Instant::now();
    recorder.save(file.path()).map_err(|e| e.to_string())?;
    tracing::info!(
        elapsed_ms = encode_start.elapsed().as_millis(),
        "voice recording: WAV encoded"
    );
    let waveform_start = std::time::Instant::now();
    let mut reader = hound::WavReader::open(file.path()).map_err(|e| e.to_string())?;
    let spec = reader.spec();
    let samples = reader
        .samples::<i16>()
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    let duration_ms =
        samples.len() as u64 * 1000 / (spec.sample_rate as u64 * spec.channels as u64);
    if duration_ms < 300 {
        return Err("Recording is too short".into());
    }
    let waveform = waveform(&samples);
    tracing::info!(
        elapsed_ms = waveform_start.elapsed().as_millis(),
        "voice recording: waveform ready"
    );
    tracing::info!(
        elapsed_ms = prepare_start.elapsed().as_millis(),
        "voice recording: preparation complete"
    );
    Ok(Some(RecordedVoiceNote {
        file,
        conversation,
        waveform,
        duration_ms,
    }))
}

fn waveform(samples: &[i16]) -> Vec<u8> {
    crate::media::waveform::waveform_from_pcm(samples)
}

#[cfg(test)]
mod tests {
    #[test]
    fn leaving_or_backgrounding_a_conversation_invalidates_capture() {
        use super::*;
        let chat = ChatState::new();
        let conversation = ActiveConversation::Channel(crate::domain::ChannelId::new());
        assert!(!recording_context_is_current(
            &chat,
            ActiveConversation::None
        ));
        chat.set_active_conversation(conversation);
        assert!(recording_context_is_current(&chat, conversation));
        chat.set_app_visible(false);
        assert!(!recording_context_is_current(&chat, conversation));
        chat.set_app_visible(true);
        chat.set_active_conversation(ActiveConversation::None);
        assert!(!recording_context_is_current(&chat, conversation));
    }

    #[test]
    fn quiet_recordings_retain_shape() {
        let samples: Vec<i16> = (0..32).map(|i| i * 2).collect();
        let bars = super::waveform(&samples);
        assert_eq!(bars[0], 13);
        assert_eq!(bars[31], 255);
        assert!(bars[8] < bars[16]);
    }

    #[test]
    fn waveform_handles_silence_short_input_and_negative_full_scale() {
        assert_eq!(super::waveform(&[]), vec![13; 32]);
        assert_eq!(super::waveform(&[0; 64]), vec![13; 32]);
        assert_eq!(super::waveform(&[i16::MIN; 64]), vec![255; 32]);
        assert_eq!(super::waveform(&[i16::MIN])[0], 255);
    }
}

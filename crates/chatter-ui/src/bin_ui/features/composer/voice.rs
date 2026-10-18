//! Thin microphone permission and recording presentation bridge.
use crate::{MainWindow, VoiceRecording};
use chatter_core::client::ChatterClient;
use chatter_core::services::voice_recording::{
    RecordedVoiceNote, RecordingEvent, RecordingSession,
};
use slint::{ComponentHandle, Model, ModelRc, VecModel};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

type ReadyRecording = Arc<Mutex<Option<(u64, Arc<RecordedVoiceNote>)>>>;
// 640 bars at a 5px pitch cover even the widest tablet recording bar.
const LIVE_LEVEL_BARS: usize = 640;

pub(super) fn wire(window: &MainWindow, client: &Arc<ChatterClient>) {
    let session: Arc<Mutex<Option<(u64, RecordingSession)>>> = Arc::new(Mutex::new(None));
    let ready: ReadyRecording = Arc::new(Mutex::new(None));
    let generation = Arc::new(AtomicU64::new(0));
    let weak = window.as_weak();
    let cancel_session = session.clone();
    let cancel_ready = ready.clone();
    let cancel_generation = generation.clone();
    window.global::<VoiceRecording>().on_cancel(move || {
        cancel_generation.fetch_add(1, Ordering::SeqCst);
        cancel_session.lock().unwrap().take();
        cancel_ready.lock().unwrap().take();
        if let Some(w) = weak.upgrade() {
            let g = w.global::<VoiceRecording>();
            g.set_active(false);
            g.set_busy(false);
        }
    });
    let weak = window.as_weak();
    let finish_session = session.clone();
    let finish_ready = ready.clone();
    let finish_generation = generation.clone();
    let finish_client = client.clone();
    window.global::<VoiceRecording>().on_finish(move || {
        let Some(w) = weak.upgrade() else { return };
        let g = w.global::<VoiceRecording>();
        if g.get_busy() {
            return;
        }
        let retry = finish_ready.lock().unwrap().clone();
        if let Some((ticket, note)) = retry {
            g.set_busy(true);
            g.set_status("Preparing voice message…".into());
            tokio::spawn(send_recording(
                weak.clone(),
                finish_client.clone(),
                finish_generation.clone(),
                ticket,
                finish_ready.clone(),
                note,
            ));
            return;
        }
        if let Some((_, recording)) = finish_session.lock().unwrap().take() {
            g.set_busy(true);
            g.set_status("Preparing voice message…".into());
            recording.finish();
        }
    });
    let weak = window.as_weak();
    let client = client.clone();
    window.global::<VoiceRecording>().on_start(move || {
        let Some(w) = weak.upgrade() else { return };
        let g = w.global::<VoiceRecording>();
        if g.get_active() {
            return;
        }
        let conversation = client.active_conversation();
        if !client.recording_context_is_current(conversation) {
            return;
        }
        let ticket = generation.fetch_add(1, Ordering::SeqCst) + 1;
        w.global::<crate::ChatPane>().set_composer_emoji_picker_visible(false);
        w.global::<crate::ChatPane>().set_composer_gif_picker_visible(false);
        w.invoke_release_text_focus();
        g.set_active(true);
        g.set_busy(true);
        g.set_elapsed("00:00".into());
        g.set_live_levels(ModelRc::new(VecModel::from(vec![0; LIVE_LEVEL_BARS])));
        g.set_live_head(-1);
        g.set_pulse_bright(false);
        g.set_status("Opening microphone…".into());
        let (weak, client, session, generation) = (
            weak.clone(),
            client.clone(),
            session.clone(),
            generation.clone(),
        );
        let ready = ready.clone();
        tokio::spawn(async move {
            let mut live_head = LIVE_LEVEL_BARS - 1;
            let mut level_frames = 0u64;
            #[cfg(target_os = "android")]
            {
                use crate::platform::android::permissions;
                if !permissions::ensure(&[permissions::RECORD_AUDIO]).await {
                    finish_ui(
                        weak,
                        generation,
                        ticket,
                        Err("Allow microphone access to record a voice message".into()),
                    );
                    return;
                }
                // The permission result can precede the activity's resume.
                for _ in 0..40 {
                    if client.is_app_visible() {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
            }
            if generation.load(Ordering::SeqCst) != ticket {
                return;
            }
            let (recording, mut events) = client.start_voice_recording(conversation);
            {
                let mut slot = session.lock().unwrap();
                if generation.load(Ordering::SeqCst) != ticket {
                    return;
                }
                *slot = Some((ticket, recording));
            }
            while let Some(event) = events.recv().await {
                if generation.load(Ordering::SeqCst) != ticket {
                    return;
                }
                match event {
                    RecordingEvent::Finished(note) => {
                        clear_session(&session, ticket);
                        match note {
                            Ok(note) => {
                                let note = Arc::new(note);
                                {
                                    let mut slot = ready.lock().unwrap();
                                    if generation.load(Ordering::SeqCst) != ticket { return; }
                                    *slot = Some((ticket, note.clone()));
                                }
                                send_recording(weak, client, generation, ticket, ready, note).await;
                            }
                            Err(error) => {
                                tracing::warn!(%error, "Voice recording failed");
                                finish_ui(weak, generation, ticket, Err("Couldn't record a voice message. Please try again.".into()));
                            }
                        }
                        return;
                    }
                    RecordingEvent::Cancelled => {
                        clear_session(&session, ticket);
                        finish_ui(weak, generation, ticket, Ok(()));
                        return;
                    }
                    event => {
                        let levels = if let RecordingEvent::Level(level) = event {
                            live_head = (live_head + 1) % LIVE_LEVEL_BARS;
                            level_frames += 1;
                            Some((live_head, i32::from(level), level_frames / 12 % 2 == 0))
                        } else {
                            None
                        };
                        let (weak, generation) = (weak.clone(), generation.clone());
                        let _ = slint::invoke_from_event_loop(move || {
                            if generation.load(Ordering::SeqCst) != ticket {
                                return;
                            }
                            if let Some(w) = weak.upgrade() {
                                let g = w.global::<VoiceRecording>();
                                match event {
                                    RecordingEvent::Started => {
                                        g.set_busy(false);
                                        g.set_status("".into());
                                    }
                                    RecordingEvent::Elapsed(seconds) => g.set_elapsed(
                                        format!("{:02}:{:02}", seconds / 60, seconds % 60).into(),
                                    ),
                                    RecordingEvent::Ready => {
                                        g.set_status("Recording ready (5 minute limit)".into());
                                    }
                                    RecordingEvent::Level(_) => {
                                        if let Some((head, level, pulse)) = levels {
                                            let live_model = g.get_live_levels();
                                            if let Some(model) = live_model.as_any().downcast_ref::<VecModel<i32>>() {
                                                model.set_row_data(head, level);
                                            }
                                            g.set_live_head(head as i32);
                                            g.set_pulse_bright(pulse);
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        });
                    }
                }
            }
        });
    });
}

fn finish_ui(
    weak: slint::Weak<MainWindow>,
    generation: Arc<AtomicU64>,
    ticket: u64,
    result: Result<(), String>,
) {
    let _ = slint::invoke_from_event_loop(move || {
        if generation.load(Ordering::SeqCst) != ticket {
            return;
        }
        if let Some(w) = weak.upgrade() {
            let g = w.global::<VoiceRecording>();
            g.set_active(false);
            g.set_busy(false);
            if let Err(error) = result {
                show_error(&w, &error);
            }
        }
    });
}

fn show_error(w: &MainWindow, text: &str) {
    w.set_toast_message(text.into());
    w.set_toast_variant("error".into());
    w.set_toast_visible(true);
}

fn clear_session(session: &Mutex<Option<(u64, RecordingSession)>>, ticket: u64) {
    let mut slot = session.lock().unwrap();
    if slot.as_ref().is_some_and(|(id, _)| *id == ticket) {
        slot.take();
    }
}

async fn send_recording(
    weak: slint::Weak<MainWindow>,
    client: Arc<ChatterClient>,
    generation: Arc<AtomicU64>,
    ticket: u64,
    ready: ReadyRecording,
    note: Arc<RecordedVoiceNote>,
) {
    let result = client.upload_voice_note(&note).await;
    let _ = slint::invoke_from_event_loop(move || {
        if generation.load(Ordering::SeqCst) != ticket {
            return;
        }
        let Some(w) = weak.upgrade() else { return };
        let g = w.global::<VoiceRecording>();
        g.set_busy(false);
        if !client.recording_context_is_current(note.conversation()) {
            g.invoke_cancel();
            return;
        }
        match result {
            Ok(pending) => {
                ready.lock().unwrap().take();
                g.set_active(false);
                client.add_pending_attachment(pending);
                w.global::<crate::ChatPane>().invoke_send("".into());
            }
            Err(error) => {
                tracing::warn!(%error, "Voice upload failed; recording retained for retry");
                g.set_status("Upload failed. Tap Send to retry.".into());
            }
        }
    });
}

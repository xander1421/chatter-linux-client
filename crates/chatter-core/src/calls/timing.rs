//! Privacy-safe, process-local timing; no participant, room or call identifiers.
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::Instant;

use parking_lot::Mutex;

static NEXT_ATTEMPT: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
pub(crate) struct CallTiming(Arc<Attempt>);
struct Attempt {
    id: u64,
    started: Instant,
    evidence: Mutex<Evidence>,
}
#[derive(Default)]
struct Evidence {
    retired: bool,
    first_frame: bool,
}
impl CallTiming {
    pub(crate) fn new(stage: &'static str) -> Self {
        let timing = Self(Arc::new(Attempt {
            id: NEXT_ATTEMPT.fetch_add(1, Ordering::Relaxed),
            started: Instant::now(),
            evidence: Mutex::new(Evidence::default()),
        }));
        timing.mark(stage);
        timing
    }
    /// Serializes retirement with the entire emission, not just its eligibility
    /// check. Called only by setup/event/decoder tasks, never audio callbacks.
    fn emit(&self, first_frame: bool, emit: impl FnOnce()) -> bool {
        let mut evidence = self.0.evidence.lock();
        if evidence.retired || (first_frame && evidence.first_frame) {
            return false;
        }
        if first_frame {
            evidence.first_frame = true;
        }
        emit();
        true
    }
    pub(crate) fn retire(&self) {
        self.0.evidence.lock().retired = true;
    }
    pub(crate) fn same_attempt(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
    fn log(&self, stage: &'static str) {
        tracing::info!(target: "call_timing", attempt = self.0.id, stage,
            elapsed_us = self.0.started.elapsed().as_micros().min(u64::MAX as u128) as u64,
            "Call timing milestone (local monotonic clock)");
    }
    pub(crate) fn mark(&self, stage: &'static str) -> bool {
        self.emit(false, || self.log(stage))
    }
    pub(crate) fn first_decoded_frame(&self) -> bool {
        // NativeAudioStream yields post-decoder PCM, including possible
        // silence/concealment. This is not authenticated packet receipt,
        // nonzero speech, output consumption, or physical audibility.
        self.emit(true, || self.log("first_remote_decoded_pcm_frame"))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cloned_observers_share_origin_and_new_attempts_do_not() {
        let first = CallTiming::new("test");
        let observer = first.clone();
        assert!(Arc::ptr_eq(&first.0, &observer.0));
        let before = first.0.started.elapsed();
        assert!(observer.0.started.elapsed() >= before);
        let next = CallTiming::new("test");
        assert_ne!(first.0.id, next.0.id);
        assert!(!Arc::ptr_eq(&first.0, &next.0));
    }
    #[test]
    fn decoded_frame_evidence_is_shared_and_does_not_leak_to_next_attempt() {
        let timing = CallTiming::new("test");
        timing.first_decoded_frame();
        assert!(!timing.clone().first_decoded_frame());
        assert!(CallTiming::new("test").first_decoded_frame());
    }
    #[test]
    fn concurrent_tracks_report_one_frame_per_attempt() {
        let timing = CallTiming::new("test");
        let mut workers = Vec::new();
        for _ in 0..8 {
            let timing = timing.clone();
            workers.push(std::thread::spawn(move || timing.first_decoded_frame()));
        }
        assert_eq!(
            workers
                .into_iter()
                .filter_map(|w| w.join().ok())
                .filter(|first| *first)
                .count(),
            1
        );
        assert!(CallTiming::new("test").first_decoded_frame());
    }
}

#[cfg(test)]
mod retirement_tests {
    use super::*;

    #[test]
    fn retirement_rejects_delayed_events_and_unclaimed_pcm_from_all_clones() {
        let timing = CallTiming::new("test");
        let detached = timing.clone();
        timing.retire();
        timing.retire(); // Teardown paths may overlap.
        assert!(!detached.mark("remote_audio_track_subscribed"));
        assert!(!detached.first_decoded_frame());
        assert!(!timing.0.evidence.lock().first_frame);
        let next = CallTiming::new("test");
        assert!(next.mark("remote_participant_present"));
        assert!(next.first_decoded_frame());
    }

    #[test]
    fn retirement_waits_for_emission_and_blocks_every_later_emission() {
        let timing = CallTiming::new("test");
        let observer = timing.clone();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let emission = std::thread::spawn(move || {
            observer.emit(true, || {
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            })
        });
        entered_rx.recv().unwrap();
        // The emission holds the same lock that retirement must acquire.
        assert!(timing.0.evidence.try_lock().is_none());
        let retiring = timing.clone();
        let retirement = std::thread::spawn(move || retiring.retire());
        release_tx.send(()).unwrap();
        assert!(emission.join().unwrap());
        retirement.join().unwrap();
        assert!(!timing.emit(false, || panic!("stale milestone emitted")));
        assert!(!timing.emit(true, || panic!("stale PCM emitted")));
    }
}

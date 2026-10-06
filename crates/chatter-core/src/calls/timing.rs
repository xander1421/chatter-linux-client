//! Privacy-safe, process-local timing; no participant, room or call identifiers.
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::Instant;

static NEXT_ATTEMPT: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
pub(crate) struct CallTiming(Arc<Attempt>);
struct Attempt {
    id: u64,
    started: Instant,
    first_frame: AtomicBool,
}
impl CallTiming {
    pub(crate) fn new(stage: &'static str) -> Self {
        let timing = Self(Arc::new(Attempt {
            id: NEXT_ATTEMPT.fetch_add(1, Ordering::Relaxed),
            started: Instant::now(),
            first_frame: AtomicBool::new(false),
        }));
        timing.mark(stage);
        timing
    }
    pub(crate) fn mark(&self, stage: &'static str) {
        tracing::info!(target: "call_timing", attempt = self.0.id, stage,
            elapsed_us = self.0.started.elapsed().as_micros().min(u64::MAX as u128) as u64,
            "Call timing milestone (local monotonic clock)");
    }
    pub(crate) fn first_decoded_frame(&self) {
        if claim_first(&self.0.first_frame) {
            // NativeAudioStream yields post-decoder PCM, including possible
            // silence/concealment. This is not authenticated packet receipt,
            // nonzero speech, output consumption, or physical audibility.
            self.mark("first_remote_decoded_pcm_frame");
        }
    }
}
fn claim_first(flag: &AtomicBool) -> bool {
    !flag.swap(true, Ordering::Relaxed)
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
        assert!(!claim_first(&timing.clone().0.first_frame));
        assert!(claim_first(&CallTiming::new("test").0.first_frame));
    }
    #[test]
    fn concurrent_tracks_report_one_frame_per_attempt() {
        let timing = CallTiming::new("test");
        let mut workers = Vec::new();
        for _ in 0..8 {
            let timing = timing.clone();
            workers.push(std::thread::spawn(move || {
                claim_first(&timing.0.first_frame)
            }));
        }
        assert_eq!(
            workers
                .into_iter()
                .filter_map(|w| w.join().ok())
                .filter(|first| *first)
                .count(),
            1
        );
        assert!(claim_first(&CallTiming::new("test").0.first_frame));
    }
}

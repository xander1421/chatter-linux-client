// Synthetic lifecycle test of the actual driver and native Android Choreographer.
// Only the main-loop waker is mocked; no app/account files are accessed.
extern crate ndk;
mod android_activity {
    #[derive(Clone)]
    pub struct AndroidAppWaker(pub std::sync::mpsc::Sender<std::time::Instant>);
    impl AndroidAppWaker {
        pub fn wake(&self) {
            let _ = self.0.send(std::time::Instant::now());
        }
    }
}
struct AndroidTraceSection;
impl AndroidTraceSection {
    fn new(_: &std::ffi::CStr) -> Self {
        Self
    }
}
#[path = "../../vsync.rs"]
mod vsync;
use std::time::{Duration, Instant};
fn main() {
    for cycle in 0..4 {
        let (tx, rx) = std::sync::mpsc::channel();
        let driver = vsync::VsyncDriver::new(android_activity::AndroidAppWaker(tx));
        let start = Instant::now();
        while !driver.is_driving() && start.elapsed() < Duration::from_secs(1) {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(driver.is_driving(), "native Choreographer unavailable");
        assert!(
            rx.recv_timeout(Duration::from_millis(40)).is_err(),
            "parked helper woke main"
        );
        driver.set_animating(true);
        let mut samples = Vec::new();
        for _ in 0..12 {
            samples.push(rx.recv_timeout(Duration::from_millis(500)).unwrap());
        }
        let mut intervals: Vec<_> = samples
            .windows(2)
            .map(|w| (w[1] - w[0]).as_secs_f64() * 1000.)
            .collect();
        intervals.sort_by(f64::total_cmp);
        assert!(
            (12.0..=22.0).contains(&intervals[5]),
            "unexpected S9 refresh cadence: {intervals:?}"
        );
        driver.set_animating(false);
        std::thread::sleep(Duration::from_millis(40));
        while rx.try_recv().is_ok() {}
        assert!(
            rx.recv_timeout(Duration::from_millis(60)).is_err(),
            "helper kept waking after idle"
        );
        driver.set_animating(true);
        let _ = rx.recv_timeout(Duration::from_millis(500)).unwrap();
        let start = Instant::now();
        drop(driver);
        assert!(
            start.elapsed() < Duration::from_millis(250),
            "active shutdown hung"
        );
        println!(
            "cycle={cycle} median_vsync_ms={} active_shutdown_ms={}",
            intervals[5],
            start.elapsed().as_secs_f64() * 1000.
        );
        let (tx, _) = std::sync::mpsc::channel();
        let parked = vsync::VsyncDriver::new(android_activity::AndroidAppWaker(tx));
        let start = Instant::now();
        drop(parked);
        assert!(
            start.elapsed() < Duration::from_millis(250),
            "startup shutdown hung"
        );
    }
    println!("VSYNC_LIFECYCLE_CHECK_COMPLETE");
}

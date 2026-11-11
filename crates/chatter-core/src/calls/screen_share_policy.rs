//! Pure screen-share link policy, independent of the SDK and capture platform.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Target {
    pub(crate) bitrate: u64,
    pub(crate) fps: f64,
    pub(crate) scale: f64,
}

pub(crate) struct Controller {
    native_budget: u64,
    native_fps: u32,
    budget: u64,
    healthy: u32,
}

impl Controller {
    pub(crate) fn new(width: u32, height: u32, fps: u32, metered: bool) -> Self {
        let native_budget = (width as u64 * height as u64 * 16).clamp(10_000_000, 150_000_000);
        Self {
            native_budget,
            native_fps: fps.max(1),
            budget: if metered { 2_000_000 } else { native_budget },
            healthy: 0,
        }
    }

    fn ceiling(&self, metered: bool) -> u64 {
        if metered {
            self.native_budget.min(6_000_000)
        } else {
            self.native_budget
        }
    }

    pub(crate) fn target(&self, metered: bool) -> Target {
        // Preserve motion first. The native budget is a quality ceiling, not
        // the bandwidth required for a smooth stream. Spending a smaller
        // budget entirely on native-resolution frames turned ordinary links
        // into 15fps shares. Scale pixels before reducing frame cadence.
        let base_scale: f64 = if metered { 1.5 } else { 1.0 };
        let budget = self.budget.min(self.ceiling(metered));
        let ratio = budget as f64 / self.native_budget as f64;
        let floor_fps = self.native_fps.min(15) as f64;
        let scale = (1.0 / ratio.sqrt()).clamp(base_scale, 4.0);
        let fps = (self.native_fps as f64 * budget as f64 / self.native_budget as f64
            * scale.powi(2))
        .clamp(floor_fps, self.native_fps as f64);
        Target {
            bitrate: budget,
            fps,
            scale,
        }
    }

    pub(crate) fn observe(
        &mut self,
        available: Option<u64>,
        bandwidth_limited: bool,
        metered: bool,
    ) -> Target {
        self.budget = self.budget.min(self.ceiling(metered));
        // Reserve room for audio, retransmissions and BWE estimation error.
        let desired =
            available.map(|bps| (bps.saturating_mul(4) / 5).clamp(150_000, self.ceiling(metered)));
        if let Some(desired) = desired.filter(|bps| *bps < self.budget) {
            self.budget = desired;
            self.healthy = 0;
        } else if bandwidth_limited && available.is_none() {
            // A sender bandwidth signal is useful even if candidate-pair BWE
            // is absent. Never ramp on missing/stale telemetry alone.
            self.budget = (self.budget * 4 / 5).max(150_000);
            self.healthy = 0;
        } else if let Some(desired) = desired {
            // The sender may still report bandwidth limitation while BWE has
            // headroom above our reduced cap. Blocking recovery on that flag
            // traps a share at its low target after a transient slowdown.
            self.healthy += 1;
            if self.healthy >= 3 {
                self.budget = (self.budget + (self.budget / 5).max(100_000)).min(desired);
                self.healthy = 0;
            }
        } else {
            self.healthy = 0;
        }
        self.target(metered)
    }

    /// MediaCodec rejects odd encoder sizes produced by arbitrary RTP scales.
    /// Android capture dimensions are multiples of eight; these exact divisors
    /// preserve even sizes and avoid restarting the codec on every BWE sample.
    #[cfg(any(target_os = "android", test))]
    pub(crate) fn android_target(&self, metered: bool) -> Target {
        let mut target = self.target(metered);
        target.scale = if target.scale <= 1.0 {
            1.0
        } else if target.scale <= 2.0 {
            2.0
        } else {
            4.0
        };
        target
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn android_scales_keep_hardware_encoder_dimensions_even() {
        // S9's 932x1920 probe becomes 928x1920 at capture allocation.
        // Include landscape and S25-like portrait dimensions as well.
        for (width, height) in [(928, 1920), (1920, 928), (880, 1920), (1080, 1920)] {
            let mut c = Controller::new(width, height, 60, false);
            for metered in [false, true] {
                for bps in (150_000..40_000_000).step_by(97_003) {
                    c.observe(Some(bps), false, metered);
                    let target = c.android_target(metered);
                    assert!([1.0, 2.0, 4.0].contains(&target.scale));
                    assert_eq!((width as f64 / target.scale) % 2.0, 0.0);
                    assert_eq!((height as f64 / target.scale) % 2.0, 0.0);
                    assert!(target.fps >= 15.0 && target.fps <= 60.0);
                }
            }
        }
    }

    #[test]
    fn preserves_motion_before_resolution_and_recovers_with_hysteresis() {
        let mut c = Controller::new(1920, 1080, 60, false);
        let t = c.observe(Some(c.native_budget / 2), false, false);
        assert!((t.fps - 60.0).abs() < 0.001);
        assert!(t.scale > 1.0);
        let low = c.observe(Some(500_000), true, false);
        assert_eq!(low.fps, 15.0);
        assert!(low.scale > 1.0);
        let budget = c.budget;
        for _ in 0..2 {
            c.observe(Some(100_000_000), false, false);
        }
        assert_eq!(c.budget, budget);
        c.observe(Some(100_000_000), false, false);
        assert!(c.budget > budget);
    }

    #[test]
    fn ordinary_desktop_and_mobile_links_keep_smooth_motion() {
        for (width, height, metered) in [(1920, 1080, false), (1080, 1920, true)] {
            let mut c = Controller::new(width, height, 60, metered);
            let t = c.observe(Some(3_000_000), true, metered);
            assert!(t.fps >= 55.0 && t.fps <= 60.0, "{t:?}");
            assert!(t.scale > 1.0 && t.scale <= 4.0);
            assert!(t.bitrate <= 2_400_000);
        }
    }

    #[test]
    fn bandwidth_flag_does_not_block_recovery_with_measured_headroom() {
        let mut c = Controller::new(1920, 1080, 60, false);
        c.observe(Some(500_000), true, false);
        let low = c.budget;
        for _ in 0..2 {
            c.observe(Some(10_000_000), true, false);
            assert_eq!(c.budget, low);
        }
        c.observe(Some(10_000_000), true, false);
        assert!(c.budget > low);
        for _ in 0..90 {
            c.observe(Some(10_000_000), true, false);
        }
        assert_eq!(c.budget, 8_000_000);
    }

    #[test]
    fn severely_limited_links_and_low_fps_sources_remain_bounded() {
        for fps in [1, 10, 30, 60, 144] {
            let mut c = Controller::new(5120, 1440, fps, false);
            let t = c.observe(Some(1), true, false);
            assert_eq!(t.bitrate, 150_000);
            assert!((1.0..=4.0).contains(&t.scale));
            assert!(t.fps >= fps.min(15) as f64 && t.fps <= fps as f64);
        }
    }

    #[test]
    fn metered_budget_is_bounded_even_after_network_switch() {
        let mut c = Controller::new(1920, 1080, 60, true);
        assert_eq!(c.budget, 2_000_000);
        for _ in 0..60 {
            c.observe(Some(200_000_000), false, true);
        }
        assert_eq!(c.budget, 6_000_000);
        assert!(c.target(true).scale >= 1.5);
        for _ in 0..60 {
            c.observe(Some(200_000_000), false, false);
        }
        assert!(c.budget > 6_000_000);
        c.observe(None, false, true);
        assert!(c.budget <= 6_000_000);
    }
    #[test]
    fn missing_telemetry_never_ramps_and_desktop_recovers_native_quality() {
        let mut c = Controller::new(5120, 1440, 144, false);
        let original = c.target(false);
        assert_eq!(original.fps, 144.0);
        assert_eq!(original.scale, 1.0);
        c.observe(None, true, false);
        let budget = c.budget;
        for _ in 0..10 {
            c.observe(None, false, false);
        }
        assert_eq!(c.budget, budget);
        for _ in 0..15 {
            c.observe(Some(200_000_000), false, false);
        }
        assert_eq!(c.target(false), original);
    }
}

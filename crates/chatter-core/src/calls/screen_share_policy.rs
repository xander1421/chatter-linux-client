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
        // Preserve text at native resolution down to 15fps before scaling.
        // Metered links explicitly start and remain at reduced resolution.
        let base_scale: f64 = if metered { 1.5 } else { 1.0 };
        let budget = self.budget.min(self.ceiling(metered));
        let ratio = budget as f64 / self.native_budget as f64 * base_scale.powi(2);
        let floor_fps = self.native_fps.min(15) as f64;
        let fps = (self.native_fps as f64 * ratio).clamp(floor_fps, self.native_fps as f64);
        let scale = (base_scale
            * (floor_fps / (self.native_fps as f64 * ratio))
                .sqrt()
                .max(1.0))
        .clamp(base_scale, 4.0);
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
        } else if bandwidth_limited {
            self.healthy = 0;
        } else if let Some(desired) = desired {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_fps_before_resolution_and_recovers_with_hysteresis() {
        let mut c = Controller::new(1920, 1080, 60, false);
        let t = c.observe(Some(c.native_budget / 2), false, false);
        assert!(t.fps < 60.0 && t.fps >= 15.0);
        assert_eq!(t.scale, 1.0);
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

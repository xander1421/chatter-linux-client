//! When the radio scans, and how often we announce.
//!
//! A port of bitchat's `BLEScanDutyPolicy.swift` and `BLEMaintenancePolicy.swift`.
//!
//! Scanning is the expensive half of BLE — advertising costs comparatively little,
//! so it stays up continuously while scanning runs in bursts. The duty cycle here is
//! **a battery guarantee, not a tuning knob**: 5 s on / 10 s off normally, tightening
//! to 3 s on / 15 s off once the mesh is dense enough that missing an advert costs
//! nothing, because someone else already has that peer.
//!
//! It also knows when *not* to duty cycle, which matters more than the durations:
//!
//! - **Fewer than three peers.** A sparse node's whole job is finding more. Saving
//!   power by not looking is a false economy when the alternative is no mesh.
//! - **Traffic in the last 10 s.** Something is happening; be reachable for it.
//! - **App not in the foreground.** The platform is already throttling us.
//!
//! Sans-IO: no timers, no clock. The driver supplies `now_unix_millis` and acts on
//! the returned [`ScanCommand`]. Swift's `TimeInterval` seconds become `u64`
//! milliseconds throughout. Where bitchat jitters an announce, the randomness is an
//! injected closure — see [`should_send_announce`].

// ---------------------------------------------------------------------------
// Constants — `bitchat/Services/TransportConfig.swift`
// ---------------------------------------------------------------------------

/// `TransportConfig.bleDutyOnDuration` (5 s) — scan window in a sparse mesh.
pub const DUTY_ON_MS: u64 = 5_000;

/// `TransportConfig.bleDutyOffDuration` (10 s) — the corresponding rest.
pub const DUTY_OFF_MS: u64 = 10_000;

/// `TransportConfig.bleDutyOnDurationDense` (3 s) — scan window once the mesh is
/// dense. Shorter, because a peer we miss will be relayed to us anyway.
pub const DUTY_ON_DENSE_MS: u64 = 3_000;

/// `TransportConfig.bleDutyOffDurationDense` (15 s) — the corresponding rest.
pub const DUTY_OFF_DENSE_MS: u64 = 15_000;

/// `TransportConfig.bleHighDegreeThreshold` (6) — the connected-peer count at which
/// the mesh counts as dense. Shared with the adaptive-TTL and relay policies, which
/// is why it is one constant rather than a per-call parameter.
pub const HIGH_DEGREE_THRESHOLD: usize = 6;

/// Literal `connectedCount <= 2` in `BLEScanDutyPolicy` — at or below this we always
/// scan continuously, whatever else is true. Finding peers is the priority.
pub const FORCE_CONTINUOUS_MAX_PEERS: usize = 2;

/// `TransportConfig.bleRecentTrafficForceScanSeconds` (10 s) — how recently traffic
/// must have been seen to pin scanning on. The caller evaluates the window; this is
/// the value it should use.
pub const RECENT_TRAFFIC_FORCE_SCAN_MS: u64 = 10_000;

/// `TransportConfig.bleMaintenanceInterval` (5 s) — how often the driver should run
/// a maintenance cycle and call [`maintenance_plan`].
pub const MAINTENANCE_INTERVAL_MS: u64 = 5_000;

/// `TransportConfig.bleMaintenanceLeewaySeconds` (1 s) — acceptable slack on that
/// timer, so the OS can coalesce wakeups.
pub const MAINTENANCE_LEEWAY_MS: u64 = 1_000;

/// The literal `5.0` window `BLEService.performMaintenance` uses for its own
/// "recent traffic" question. Deliberately tighter than
/// [`RECENT_TRAFFIC_FORCE_SCAN_MS`]: keeping the radio awake is worth a looser
/// window than emitting an extra announce.
pub const MAINTENANCE_RECENT_TRAFFIC_MS: u64 = 5_000;

/// `TransportConfig.bleAnnounceIntervalSeconds` (4 s) — announce cadence while we
/// have no connected peers. Fast, because this is the discovery path.
pub const ANNOUNCE_INTERVAL_MS: u64 = 4_000;

/// `TransportConfig.bleAnnounceMinInterval` (1 s) — floor between announces from any
/// cause. The driver checks [`announce_floor_passed`] before every announce.
pub const ANNOUNCE_MIN_INTERVAL_MS: u64 = 1_000;

/// `TransportConfig.bleConnectedAnnounceBaseSecondsSparse` (15 s).
pub const CONNECTED_ANNOUNCE_BASE_SPARSE_MS: u64 = 15_000;

/// `TransportConfig.bleConnectedAnnounceBaseSecondsDense` (30 s) — a dense mesh
/// needs far fewer announces per node to stay coherent, and each one costs every
/// neighbour a receive.
pub const CONNECTED_ANNOUNCE_BASE_DENSE_MS: u64 = 30_000;

/// `TransportConfig.bleConnectedAnnounceJitterSparse` (±4 s).
pub const CONNECTED_ANNOUNCE_JITTER_SPARSE_MS: u64 = 4_000;

/// `TransportConfig.bleConnectedAnnounceJitterDense` (±8 s) — spread, so that a room
/// full of devices that all booted together does not announce in lockstep forever.
pub const CONNECTED_ANNOUNCE_JITTER_DENSE_MS: u64 = 8_000;

/// Literal `10.0` in `shouldSendAnnounce` — with traffic flowing, an announce this
/// old is refreshed regardless of the cadence below.
pub const TRAFFIC_ANNOUNCE_MIN_ELAPSED_MS: u64 = 10_000;

/// Literal `cycle.isMultiple(of: 3)` — cleanup runs every third maintenance cycle,
/// i.e. roughly every 15 s.
pub const MAINTENANCE_CLEANUP_EVERY: u32 = 3;

/// Literal `!cycle.isMultiple(of: 2)` — the directed spool is flushed on odd cycles.
pub const MAINTENANCE_SPOOL_FLUSH_EVERY: u32 = 2;

/// Literal `cycle >= 6` — the cycle counter wraps here, ~30 s.
pub const MAINTENANCE_COUNTER_RESET_AT: u32 = 6;

// ---------------------------------------------------------------------------
// Scan duty cycle
// ---------------------------------------------------------------------------

/// Whether the mesh is dense enough to relay for us.
pub fn is_dense(connected_count: usize) -> bool {
    connected_count >= HIGH_DEGREE_THRESHOLD
}

/// How scanning should be run right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanDutyPlan {
    /// Scan without interruption. Costs battery; buys discovery.
    Continuous,
    /// Alternate between scanning and resting.
    DutyCycle { on_ms: u64, off_ms: u64 },
}

impl ScanDutyPlan {
    /// The sparse-mesh cycle: 5 s on, 10 s off.
    pub const SPARSE: Self = Self::DutyCycle {
        on_ms: DUTY_ON_MS,
        off_ms: DUTY_OFF_MS,
    };

    /// The dense-mesh cycle: 3 s on, 15 s off.
    pub const DENSE: Self = Self::DutyCycle {
        on_ms: DUTY_ON_DENSE_MS,
        off_ms: DUTY_OFF_DENSE_MS,
    };

    /// Fraction of wall-clock time the radio spends scanning, as a percentage.
    /// 100 for continuous, 33 sparse, 16 dense.
    pub fn scan_percentage(&self) -> u64 {
        match *self {
            Self::Continuous => 100,
            Self::DutyCycle { on_ms, off_ms } => {
                // A zero-length period is not a real plan; report full scanning
                // rather than dividing by zero.
                (on_ms * 100).checked_div(on_ms + off_ms).unwrap_or(100)
            }
        }
    }
}

/// Choose a duty plan.
///
/// `duty_enabled` is the local kill switch (bitchat keeps it on); `app_is_active` is
/// false when the platform has backgrounded us — on Linux there is no such state, so
/// pass `true`. `has_recent_traffic` should be evaluated over
/// [`RECENT_TRAFFIC_FORCE_SCAN_MS`].
pub fn scan_duty_plan(
    duty_enabled: bool,
    app_is_active: bool,
    connected_count: usize,
    has_recent_traffic: bool,
) -> ScanDutyPlan {
    // Two independent reasons to stay awake: too few peers to stop looking for
    // more, or something actively happening.
    let force_continuous =
        connected_count <= FORCE_CONTINUOUS_MAX_PEERS || has_recent_traffic;
    let should_duty_cycle =
        duty_enabled && app_is_active && connected_count > 0 && !force_continuous;

    if !should_duty_cycle {
        return ScanDutyPlan::Continuous;
    }

    if is_dense(connected_count) {
        ScanDutyPlan::DENSE
    } else {
        ScanDutyPlan::SPARSE
    }
}

/// What the radio should do about scanning, right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanCommand {
    /// Leave the radio as it is.
    Unchanged,
    /// Call `MeshLink::set_scanning(true)`.
    StartScanning,
    /// Call `MeshLink::set_scanning(false)`.
    StopScanning,
}

/// The running duty cycle: which phase we are in and when it ends.
///
/// Drive it two ways, exactly as `BLEService` does:
///
/// - [`update`](Self::update) on every maintenance tick, when peer counts and
///   traffic may have changed. Re-plans, and restarts the on-phase if the plan moved.
/// - [`poll`](Self::poll) at [`next_transition_at`](Self::next_transition_at) to flip
///   phase on time.
///
/// Both return a [`ScanCommand`]; acting on it is the driver's job.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScanDutyCycle {
    plan: ScanDutyPlan,
    scanning: bool,
    phase_started_ms: u64,
}

impl Default for ScanDutyCycle {
    fn default() -> Self {
        Self::new()
    }
}

impl ScanDutyCycle {
    /// A cycle that has not started scanning yet. The first
    /// [`update`](Self::update) is what turns the radio on.
    pub fn new() -> Self {
        Self {
            plan: ScanDutyPlan::Continuous,
            scanning: false,
            phase_started_ms: 0,
        }
    }

    /// Is the radio meant to be scanning at this instant?
    pub fn should_scan(&self) -> bool {
        self.scanning
    }

    pub fn plan(&self) -> ScanDutyPlan {
        self.plan
    }

    pub fn is_duty_cycling(&self) -> bool {
        matches!(self.plan, ScanDutyPlan::DutyCycle { .. })
    }

    /// When [`poll`](Self::poll) will next change something, or `None` while
    /// scanning continuously — in which case there is nothing to wake up for.
    pub fn next_transition_at(&self) -> Option<u64> {
        match self.plan {
            ScanDutyPlan::Continuous => None,
            ScanDutyPlan::DutyCycle { on_ms, off_ms } => {
                Some(self.phase_started_ms + if self.scanning { on_ms } else { off_ms })
            }
        }
    }

    /// Re-plan from current conditions.
    ///
    /// A changed plan — including sparse↔dense, which changes both durations —
    /// restarts the cycle in its on-phase, so a mesh that just got sparser starts
    /// looking for peers immediately instead of finishing an off-phase first. An
    /// unchanged plan simply advances the current one.
    pub fn update(
        &mut self,
        duty_enabled: bool,
        app_is_active: bool,
        connected_count: usize,
        has_recent_traffic: bool,
        now_ms: u64,
    ) -> ScanCommand {
        let plan = scan_duty_plan(
            duty_enabled,
            app_is_active,
            connected_count,
            has_recent_traffic,
        );

        if plan == self.plan {
            return self.poll(now_ms);
        }

        self.plan = plan;
        self.phase_started_ms = now_ms;
        self.set_scanning(true)
    }

    /// Advance the phase if it has run its course.
    pub fn poll(&mut self, now_ms: u64) -> ScanCommand {
        let (on_ms, off_ms) = match self.plan {
            // Continuous means one permanent on-phase.
            ScanDutyPlan::Continuous => return self.set_scanning(true),
            ScanDutyPlan::DutyCycle { on_ms, off_ms } => (on_ms, off_ms),
        };

        let phase_ms = if self.scanning { on_ms } else { off_ms };
        if now_ms.saturating_sub(self.phase_started_ms) < phase_ms {
            return ScanCommand::Unchanged;
        }

        // A poll that arrives several phases late flips once and restarts the phase
        // from now, rather than trying to catch up on the ones it slept through —
        // the same as a coalesced timer, and it keeps the on-phase full length.
        self.phase_started_ms = now_ms;
        self.set_scanning(!self.scanning)
    }

    /// The adapter went away. Forget the phase and stop scanning; the next
    /// [`update`](Self::update) will start it again.
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    fn set_scanning(&mut self, on: bool) -> ScanCommand {
        if self.scanning == on {
            return ScanCommand::Unchanged;
        }
        self.scanning = on;
        if on {
            ScanCommand::StartScanning
        } else {
            ScanCommand::StopScanning
        }
    }
}

// ---------------------------------------------------------------------------
// Maintenance
// ---------------------------------------------------------------------------

/// The periodic work a maintenance cycle should do, decided in one place so the
/// cadences stay legible instead of being scattered across the service.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MaintenancePlan {
    /// Broadcast our identity announce.
    pub should_send_announce: bool,
    /// Make sure the peripheral role is still advertising. Checked only when we know
    /// no peers at all — if nobody can see us, advertising is the first suspect.
    pub should_ensure_advertising: bool,
    /// Expire stale fragments, peers, dedup records. Every third cycle (~15 s).
    pub should_run_cleanup: bool,
    /// Retry directed packets spooled for peers that were briefly unreachable.
    /// Odd cycles, so it alternates with the cleanup-heavy ones.
    pub should_flush_directed_spool: bool,
    /// Wrap the cycle counter.
    pub should_reset_counter: bool,
}

/// A jitter source that always returns zero, for callers that want a deterministic
/// cadence and for tests.
pub fn no_jitter(_bound_ms: u64) -> i64 {
    0
}

/// Decide one maintenance cycle.
///
/// `cycle` counts from 1 and wraps at [`MAINTENANCE_COUNTER_RESET_AT`].
/// `has_recent_traffic` is evaluated over [`MAINTENANCE_RECENT_TRAFFIC_MS`] here —
/// note that this is a *different, tighter* window than the one the scan duty cycle
/// uses. `jitter` is described on [`should_send_announce`].
pub fn maintenance_plan(
    cycle: u32,
    connected_count: usize,
    peer_registry_is_empty: bool,
    elapsed_since_last_announce_ms: u64,
    has_recent_traffic: bool,
    jitter: impl FnOnce(u64) -> i64,
) -> MaintenancePlan {
    MaintenancePlan {
        should_send_announce: should_send_announce(
            connected_count,
            elapsed_since_last_announce_ms,
            has_recent_traffic,
            jitter,
        ),
        should_ensure_advertising: peer_registry_is_empty,
        should_run_cleanup: cycle % MAINTENANCE_CLEANUP_EVERY == 0,
        should_flush_directed_spool: cycle % MAINTENANCE_SPOOL_FLUSH_EVERY != 0,
        should_reset_counter: cycle >= MAINTENANCE_COUNTER_RESET_AT,
    }
}

/// Is it time to announce ourselves again?
///
/// Three regimes, in order:
///
/// 1. **Traffic is flowing** and our last announce is over
///    [`TRAFFIC_ANNOUNCE_MIN_ELAPSED_MS`] old — refresh it, so peers actively talking
///    to us do not age us out.
/// 2. **No connected peers** — every [`ANNOUNCE_INTERVAL_MS`]. This is discovery;
///    nobody can answer an announce they never heard.
/// 3. **Connected** — every 15 s sparse, 30 s dense, each with jitter.
///
/// `jitter` is called with the bound in milliseconds and must return an offset in
/// `[-bound, +bound]`; bitchat draws it uniformly. It exists because devices that
/// power on together would otherwise announce in lockstep forever, colliding on air
/// every cycle. Pass [`no_jitter`] for a deterministic cadence.
pub fn should_send_announce(
    connected_count: usize,
    elapsed_since_last_announce_ms: u64,
    has_recent_traffic: bool,
    jitter: impl FnOnce(u64) -> i64,
) -> bool {
    if has_recent_traffic && elapsed_since_last_announce_ms >= TRAFFIC_ANNOUNCE_MIN_ELAPSED_MS {
        return true;
    }

    if connected_count == 0 {
        return elapsed_since_last_announce_ms >= ANNOUNCE_INTERVAL_MS;
    }

    let dense = is_dense(connected_count);
    let base_ms = if dense {
        CONNECTED_ANNOUNCE_BASE_DENSE_MS
    } else {
        CONNECTED_ANNOUNCE_BASE_SPARSE_MS
    };
    let jitter_bound_ms = if dense {
        CONNECTED_ANNOUNCE_JITTER_DENSE_MS
    } else {
        CONNECTED_ANNOUNCE_JITTER_SPARSE_MS
    };

    let target_ms = base_ms as i64 + jitter(jitter_bound_ms);
    elapsed_since_last_announce_ms as i64 >= target_ms
}

/// Whether an announce may go out now, whatever triggered it.
///
/// Every cause shares one floor of [`ANNOUNCE_MIN_INTERVAL_MS`]: the cadence, a new
/// link, and an explicit request. Without it each inbound connection buys one
/// signed broadcast to every neighbour, so a peer cycling connections makes this
/// device flood the mesh. A clock that stepped backwards (`now < last`) passes,
/// rather than silencing the device until the clock catches up.
pub fn announce_floor_passed(last_announce_ms: u64, now_ms: u64) -> bool {
    now_ms < last_announce_ms || now_ms - last_announce_ms >= ANNOUNCE_MIN_INTERVAL_MS
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_announce_floor_is_one_second_from_any_cause() {
        use super::{ANNOUNCE_MIN_INTERVAL_MS, announce_floor_passed};
        assert_eq!(ANNOUNCE_MIN_INTERVAL_MS, 1_000);
        let last = 50_000;
        assert!(!announce_floor_passed(last, last));
        assert!(!announce_floor_passed(last, last + 999));
        assert!(announce_floor_passed(last, last + 1_000));
        // Never announced: `last` is 0.
        assert!(announce_floor_passed(0, 10_000));
        // A clock that stepped backwards does not silence the device.
        assert!(announce_floor_passed(last, last - 1));
    }

    use super::*;

    // -- Duty plan selection ---------------------------------------------------

    #[test]
    fn scanning_stays_continuous_whenever_duty_cycling_would_cost_us_peers() {
        // Kill switch off.
        assert_eq!(
            scan_duty_plan(false, true, 4, false),
            ScanDutyPlan::Continuous
        );
        // Backgrounded: the platform throttles us anyway.
        assert_eq!(
            scan_duty_plan(true, false, 4, false),
            ScanDutyPlan::Continuous
        );
        // No peers at all.
        assert_eq!(
            scan_duty_plan(true, true, 0, false),
            ScanDutyPlan::Continuous
        );
        // Traffic in the last 10 s.
        assert_eq!(
            scan_duty_plan(true, true, 4, true),
            ScanDutyPlan::Continuous
        );
    }

    #[test]
    fn the_sparse_mesh_boundary_is_three_peers() {
        // At or below two, finding more peers outranks saving power.
        assert_eq!(
            scan_duty_plan(true, true, 1, false),
            ScanDutyPlan::Continuous
        );
        assert_eq!(
            scan_duty_plan(true, true, 2, false),
            ScanDutyPlan::Continuous
        );
        assert_eq!(scan_duty_plan(true, true, 3, false), ScanDutyPlan::SPARSE);
    }

    #[test]
    fn the_dense_boundary_is_six_peers() {
        assert_eq!(scan_duty_plan(true, true, 5, false), ScanDutyPlan::SPARSE);
        assert_eq!(scan_duty_plan(true, true, 6, false), ScanDutyPlan::DENSE);
        assert_eq!(scan_duty_plan(true, true, 7, false), ScanDutyPlan::DENSE);
    }

    #[test]
    fn the_ported_durations_are_five_ten_and_three_fifteen() {
        assert_eq!(
            ScanDutyPlan::SPARSE,
            ScanDutyPlan::DutyCycle {
                on_ms: 5_000,
                off_ms: 10_000
            }
        );
        assert_eq!(
            ScanDutyPlan::DENSE,
            ScanDutyPlan::DutyCycle {
                on_ms: 3_000,
                off_ms: 15_000
            }
        );
        // The battery guarantee, stated as a number: a third of the time sparse,
        // a sixth dense.
        assert_eq!(ScanDutyPlan::Continuous.scan_percentage(), 100);
        assert_eq!(ScanDutyPlan::SPARSE.scan_percentage(), 33);
        assert_eq!(ScanDutyPlan::DENSE.scan_percentage(), 16);
    }

    #[test]
    fn density_is_read_from_the_shared_threshold() {
        assert!(!is_dense(HIGH_DEGREE_THRESHOLD - 1));
        assert!(is_dense(HIGH_DEGREE_THRESHOLD));
    }

    // -- Running the cycle -----------------------------------------------------

    #[test]
    fn a_fresh_cycle_is_off_until_told_otherwise() {
        let cycle = ScanDutyCycle::new();
        assert!(!cycle.should_scan());
        assert_eq!(cycle.plan(), ScanDutyPlan::Continuous);
        assert_eq!(cycle.next_transition_at(), None);
    }

    #[test]
    fn continuous_scanning_starts_once_and_never_flips() {
        let mut cycle = ScanDutyCycle::new();
        assert_eq!(cycle.update(true, true, 0, false, 1_000), ScanCommand::StartScanning);
        assert!(cycle.should_scan());
        assert_eq!(cycle.next_transition_at(), None);

        assert_eq!(cycle.poll(999_000), ScanCommand::Unchanged);
        assert_eq!(
            cycle.update(true, true, 1, false, 999_000),
            ScanCommand::Unchanged
        );
        assert!(cycle.should_scan());
    }

    #[test]
    fn the_sparse_cycle_flips_exactly_on_its_boundaries() {
        let mut cycle = ScanDutyCycle::new();
        assert_eq!(
            cycle.update(true, true, 3, false, 10_000),
            ScanCommand::StartScanning
        );
        assert_eq!(cycle.plan(), ScanDutyPlan::SPARSE);
        assert_eq!(cycle.next_transition_at(), Some(15_000));

        // 5 s on.
        assert_eq!(cycle.poll(14_999), ScanCommand::Unchanged);
        assert!(cycle.should_scan());
        assert_eq!(cycle.poll(15_000), ScanCommand::StopScanning);
        assert!(!cycle.should_scan());
        assert_eq!(cycle.next_transition_at(), Some(25_000));

        // 10 s off.
        assert_eq!(cycle.poll(24_999), ScanCommand::Unchanged);
        assert_eq!(cycle.poll(25_000), ScanCommand::StartScanning);
        assert!(cycle.should_scan());
        assert_eq!(cycle.next_transition_at(), Some(30_000));

        // And it keeps going.
        assert_eq!(cycle.poll(30_000), ScanCommand::StopScanning);
        assert_eq!(cycle.poll(40_000), ScanCommand::StartScanning);
    }

    #[test]
    fn the_dense_cycle_uses_three_on_fifteen_off() {
        let mut cycle = ScanDutyCycle::new();
        cycle.update(true, true, 6, false, 0);
        assert_eq!(cycle.plan(), ScanDutyPlan::DENSE);

        assert_eq!(cycle.poll(2_999), ScanCommand::Unchanged);
        assert_eq!(cycle.poll(3_000), ScanCommand::StopScanning);
        assert_eq!(cycle.poll(17_999), ScanCommand::Unchanged);
        assert_eq!(cycle.poll(18_000), ScanCommand::StartScanning);
    }

    #[test]
    fn getting_denser_restarts_the_on_phase_with_the_tighter_cycle() {
        let mut cycle = ScanDutyCycle::new();
        cycle.update(true, true, 3, false, 0);
        assert_eq!(cycle.poll(5_000), ScanCommand::StopScanning);

        // Sixth peer arrives mid rest: the plan changes, so scanning resumes now.
        assert_eq!(
            cycle.update(true, true, 6, false, 9_000),
            ScanCommand::StartScanning
        );
        assert_eq!(cycle.plan(), ScanDutyPlan::DENSE);
        assert_eq!(cycle.next_transition_at(), Some(12_000));
        assert_eq!(cycle.poll(11_999), ScanCommand::Unchanged);
        assert_eq!(cycle.poll(12_000), ScanCommand::StopScanning);
    }

    #[test]
    fn an_update_with_an_unchanged_plan_does_not_disturb_the_phase() {
        let mut cycle = ScanDutyCycle::new();
        cycle.update(true, true, 3, false, 0);

        // Peer count moved but the plan did not; the on-phase must still end at 5 s
        // rather than being restarted by every maintenance tick.
        assert_eq!(cycle.update(true, true, 4, false, 4_000), ScanCommand::Unchanged);
        assert_eq!(cycle.next_transition_at(), Some(5_000));
        assert_eq!(cycle.update(true, true, 5, false, 5_000), ScanCommand::StopScanning);
    }

    #[test]
    fn traffic_mid_rest_wakes_the_radio_immediately() {
        // The reason the traffic override exists: mid off-phase we are deaf, and a
        // peer talking to us must not have to wait out ten seconds of rest.
        let mut cycle = ScanDutyCycle::new();
        cycle.update(true, true, 4, false, 0);
        assert_eq!(cycle.poll(5_000), ScanCommand::StopScanning);

        assert_eq!(
            cycle.update(true, true, 4, true, 6_000),
            ScanCommand::StartScanning
        );
        assert_eq!(cycle.plan(), ScanDutyPlan::Continuous);
        assert_eq!(cycle.next_transition_at(), None);
    }

    #[test]
    fn losing_peers_drops_back_to_continuous_scanning() {
        let mut cycle = ScanDutyCycle::new();
        cycle.update(true, true, 4, false, 0);
        assert!(cycle.is_duty_cycling());

        assert_eq!(
            cycle.update(true, true, 2, false, 1_000),
            ScanCommand::Unchanged,
            "already scanning; only the plan changed"
        );
        assert!(!cycle.is_duty_cycling());
        assert!(cycle.should_scan());
        assert_eq!(cycle.poll(60_000), ScanCommand::Unchanged);
    }

    #[test]
    fn a_late_poll_flips_once_and_restarts_the_phase_in_full() {
        let mut cycle = ScanDutyCycle::new();
        cycle.update(true, true, 3, false, 0);

        // Three whole periods late: one flip, and the off-phase gets its full 10 s
        // from now rather than being cut short by catch-up arithmetic.
        assert_eq!(cycle.poll(50_000), ScanCommand::StopScanning);
        assert_eq!(cycle.next_transition_at(), Some(60_000));
    }

    #[test]
    fn reset_stops_scanning_and_forgets_the_phase() {
        let mut cycle = ScanDutyCycle::new();
        cycle.update(true, true, 3, false, 0);
        cycle.reset();

        assert!(!cycle.should_scan());
        assert_eq!(cycle.plan(), ScanDutyPlan::Continuous);
        assert_eq!(
            cycle.update(true, true, 3, false, 100_000),
            ScanCommand::StartScanning
        );
        assert_eq!(cycle.next_transition_at(), Some(105_000));
    }

    #[test]
    fn a_full_sparse_period_scans_a_third_of_the_time() {
        let mut cycle = ScanDutyCycle::new();
        cycle.update(true, true, 3, false, 0);

        let mut scanning_ms = 0u64;
        let mut now = 0u64;
        while now < 90_000 {
            if cycle.should_scan() {
                scanning_ms += 100;
            }
            now += 100;
            cycle.poll(now);
        }
        assert_eq!(scanning_ms, 30_000, "5 s in every 15 s, over six periods");
    }

    // -- Maintenance cadence ---------------------------------------------------

    #[test]
    fn an_isolated_node_announces_every_four_seconds() {
        assert!(!should_send_announce(0, 3_999, false, no_jitter));
        assert!(should_send_announce(0, 4_000, false, no_jitter));
    }

    #[test]
    fn a_connected_sparse_node_announces_every_fifteen_seconds() {
        assert!(!should_send_announce(2, 14_999, false, no_jitter));
        assert!(should_send_announce(2, 15_000, false, no_jitter));
        assert!(!should_send_announce(5, 14_999, false, no_jitter));
        assert!(should_send_announce(5, 15_000, false, no_jitter));
    }

    #[test]
    fn a_connected_dense_node_announces_every_thirty_seconds() {
        assert!(!should_send_announce(6, 29_999, false, no_jitter));
        assert!(should_send_announce(6, 30_000, false, no_jitter));
        // Still sparse cadence one peer below the threshold.
        assert!(should_send_announce(5, 16_000, false, no_jitter));
        assert!(!should_send_announce(6, 16_000, false, no_jitter));
    }

    #[test]
    fn jitter_is_injected_and_shifts_the_target_both_ways() {
        // The bound handed to the closure is the one for the current density, and a
        // negative offset brings the announce forward, a positive one delays it.
        let mut seen_bound = 0u64;
        assert!(should_send_announce(2, 11_000, false, |bound| {
            seen_bound = bound;
            -4_000
        }));
        assert_eq!(seen_bound, CONNECTED_ANNOUNCE_JITTER_SPARSE_MS);

        assert!(!should_send_announce(2, 18_000, false, |_| 4_000));
        assert!(should_send_announce(2, 19_000, false, |_| 4_000));

        let mut seen_bound = 0u64;
        assert!(should_send_announce(6, 22_000, false, |bound| {
            seen_bound = bound;
            -8_000
        }));
        assert_eq!(seen_bound, CONNECTED_ANNOUNCE_JITTER_DENSE_MS);
    }

    #[test]
    fn the_isolated_cadence_takes_no_jitter_at_all() {
        // The jitter closure must not even be consulted with no peers — an isolated
        // node's 4 s discovery cadence is not something to spread out.
        let mut consulted = false;
        assert!(should_send_announce(0, 4_000, false, |_| {
            consulted = true;
            0
        }));
        assert!(!consulted);
    }

    #[test]
    fn traffic_overrides_the_cadence_after_ten_seconds() {
        // Dense nodes would otherwise wait 30 s; with traffic flowing, 10 s.
        assert!(!should_send_announce(6, 9_999, true, no_jitter));
        assert!(should_send_announce(6, 10_000, true, no_jitter));
        // Below 10 s it falls through to the ordinary cadence, which says no.
        assert!(!should_send_announce(2, 9_000, true, no_jitter));
    }

    #[test]
    fn the_maintenance_cadence_stages_its_periodic_work() {
        let plan = |cycle| maintenance_plan(cycle, 1, false, 0, false, no_jitter);

        // Cleanup every third cycle, spool flush on odd ones: they interleave so no
        // single cycle carries all the work.
        let flags: Vec<(bool, bool, bool)> = (1..=6)
            .map(|c| {
                let p = plan(c);
                (
                    p.should_run_cleanup,
                    p.should_flush_directed_spool,
                    p.should_reset_counter,
                )
            })
            .collect();

        assert_eq!(
            flags,
            vec![
                (false, true, false),  // 1
                (false, false, false), // 2
                (true, true, false),   // 3
                (false, false, false), // 4
                (false, true, false),  // 5
                (true, false, true),   // 6 — cleanup, then wrap
            ]
        );
    }

    #[test]
    fn advertising_is_re_checked_only_when_we_know_nobody() {
        assert!(maintenance_plan(1, 0, true, 0, false, no_jitter).should_ensure_advertising);
        assert!(!maintenance_plan(1, 0, false, 0, false, no_jitter).should_ensure_advertising);
    }

    #[test]
    fn a_discovery_mode_cycle_announces_and_republishes_the_advert() {
        let plan = maintenance_plan(1, 0, true, ANNOUNCE_INTERVAL_MS, false, no_jitter);
        assert!(plan.should_send_announce);
        assert!(plan.should_ensure_advertising);
        assert!(plan.should_flush_directed_spool);
        assert!(!plan.should_run_cleanup);
        assert!(!plan.should_reset_counter);
    }
}

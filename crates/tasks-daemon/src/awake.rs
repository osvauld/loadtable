//! Keeping the machine awake while the daemon has work (R-607). A laptop that idle-sleeps in a
//! minute would otherwise drop a peer's job, or never be reachable for one. The hold is taken
//! while any job runs or is queued and let go a little after the last one ends, so back-to-back
//! jobs do not flap it. It changes no setting: the OS drops it when the daemon exits.

use crate::api::PowerView;
use crate::machine_policy::MachinePolicy;
use crate::scheduler::{Scheduler, lock};
use tasks_platform::{InhibitGuard, SleepGuard};

/// How often a refused hold is tried again, ms.
const RETRY_MS: i64 = 60_000;

/// The hold and the bookkeeping around it. Time is passed in, so tests drive a fake clock.
#[derive(Default)]
pub(crate) struct Keeper {
    guard: Option<InhibitGuard>,
    /// Since when no job needed the hold, ms.
    idle_since_ms: Option<i64>,
    /// Jobs that needed the hold at the last update.
    jobs: u32,
    /// Why the hold could not be taken, until it can.
    error: Option<String>,
    /// When a refused hold may be tried again, ms.
    retry_at_ms: i64,
}

/// What the keeper holds now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HoldView {
    pub holding: bool,
    pub jobs: u32,
    /// Milliseconds until a lingering hold is released.
    pub linger_left_ms: Option<i64>,
    pub error: Option<String>,
}

impl Keeper {
    /// Takes or releases the hold for `jobs` jobs at `now_ms`. `linger_ms` is how long the hold
    /// outlasts the last job; `enabled` false releases at once.
    pub(crate) fn update(
        &mut self,
        sleep: &dyn SleepGuard,
        now_ms: i64,
        jobs: u32,
        enabled: bool,
        linger_ms: i64,
    ) {
        self.jobs = jobs;
        if !enabled {
            self.guard = None;
            self.idle_since_ms = None;
            self.error = None;
            return;
        }
        if jobs > 0 {
            self.idle_since_ms = None;
            if self.guard.is_none() && now_ms >= self.retry_at_ms {
                let why = "loadtabled has jobs running or queued";
                match sleep.keep_awake(why) {
                    Ok(g) => {
                        self.guard = Some(g);
                        self.error = None;
                    }
                    Err(e) => {
                        self.error = Some(e.to_string());
                        self.retry_at_ms = now_ms.saturating_add(RETRY_MS);
                    }
                }
            }
        } else if self.guard.is_some() {
            let since = *self.idle_since_ms.get_or_insert(now_ms);
            if now_ms.saturating_sub(since) >= linger_ms {
                self.guard = None;
                self.idle_since_ms = None;
            }
        } else {
            // nothing to hold and nothing held: a past refusal no longer matters
            self.error = None;
        }
    }

    pub(crate) fn view(&self, now_ms: i64, linger_ms: i64) -> HoldView {
        HoldView {
            holding: self.guard.is_some(),
            jobs: self.jobs,
            linger_left_ms: self
                .idle_since_ms
                .filter(|_| self.guard.is_some())
                .map(|s| (linger_ms - now_ms.saturating_sub(s)).max(0)),
            error: self.error.clone(),
        }
    }
}

impl Scheduler {
    /// Reads the power source once per tick; an unreadable source keeps the last reading.
    pub(crate) fn refresh_power(&self) {
        if let Ok(p) = self.platform.power.power() {
            lock(&self.state).power = Some(p);
        }
    }

    /// True while jobs from peers must wait because this machine is on battery.
    pub(crate) fn battery_blocks_peer_jobs(&self, policy: &MachinePolicy) -> bool {
        !policy.peer_jobs_on_battery() && lock(&self.state).power.is_some_and(|p| p.on_battery)
    }

    /// Takes or releases the stay-awake hold for the jobs running or about to start.
    /// `started` jobs were counted in the pass's demand and now run.
    pub(crate) fn keep_awake(&self, policy: &MachinePolicy, now_ms: i64, started: usize) {
        let running = self.running_count();
        let mut st = lock(&self.state);
        let queued = (st.awake_demand as usize).saturating_sub(started);
        let jobs = u32::try_from(running + queued).unwrap_or(u32::MAX);
        let linger =
            i64::try_from(policy.keep_awake_linger_secs().saturating_mul(1000)).unwrap_or(i64::MAX);
        st.awake.update(
            &*self.platform.sleep,
            now_ms,
            jobs,
            policy.keep_awake(),
            linger,
        );
    }

    /// Power source and stay-awake hold as the status shows them.
    pub(crate) fn power_view(&self) -> PowerView {
        let policy = self.policy();
        let now = self.clock.now_ms();
        let linger =
            i64::try_from(policy.keep_awake_linger_secs().saturating_mul(1000)).unwrap_or(i64::MAX);
        let st = lock(&self.state);
        let hold = st.awake.view(now, linger);
        let power = st.power;
        PowerView {
            on_battery: power.is_some_and(|p| p.on_battery),
            battery_pct: power.and_then(|p| p.percent),
            awake_jobs: hold.jobs,
            holding: hold.holding,
            linger_secs: hold
                .linger_left_ms
                .map(|ms| u64::try_from(ms / 1000).unwrap_or(0)),
            guard_error: hold.error,
            battery_held: u32::try_from(st.battery_held.len()).unwrap_or(u32::MAX),
            peer_jobs_on_battery: policy.peer_jobs_on_battery(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tasks_platform::fake::FakePower;

    const LINGER: i64 = 120_000;

    fn step(k: &mut Keeper, p: &FakePower, now: i64, jobs: u32) {
        k.update(p, now, jobs, true, LINGER);
    }

    #[test]
    fn r607_the_hold_is_taken_with_work_and_kept_through_the_linger() {
        let (mut k, p) = (Keeper::default(), FakePower::default());
        step(&mut k, &p, 0, 0);
        assert_eq!(p.held(), 0);
        step(&mut k, &p, 1_000, 2);
        assert_eq!(p.held(), 1);
        // more ticks with work do not take it again
        step(&mut k, &p, 2_000, 1);
        assert_eq!(p.acquired(), 1);
        // the last job ends: still held through the linger
        step(&mut k, &p, 3_000, 0);
        step(&mut k, &p, 3_000 + LINGER - 1, 0);
        assert_eq!(p.held(), 1);
        let v = k.view(3_000 + LINGER - 1, LINGER);
        assert_eq!((v.holding, v.linger_left_ms), (true, Some(1)));
        // and released once it has passed
        step(&mut k, &p, 3_000 + LINGER, 0);
        assert_eq!(p.held(), 0);
        assert!(!k.view(0, LINGER).holding);
    }

    #[test]
    fn r607_back_to_back_jobs_do_not_flap_the_hold() {
        let (mut k, p) = (Keeper::default(), FakePower::default());
        step(&mut k, &p, 0, 1);
        step(&mut k, &p, 10_000, 0);
        step(&mut k, &p, 40_000, 1);
        step(&mut k, &p, 50_000, 0);
        step(&mut k, &p, 100_000, 0);
        assert_eq!((p.acquired(), p.held()), (1, 1));
        // the linger restarts from the later idle moment
        step(&mut k, &p, 50_000 + LINGER, 0);
        assert_eq!(p.held(), 0);
    }

    #[test]
    fn r607_a_refused_hold_is_reported_and_retried_later() {
        let (mut k, p) = (Keeper::default(), FakePower::default());
        p.refuse_with(Some("not available"));
        step(&mut k, &p, 0, 1);
        assert!(k.view(0, LINGER).error.is_some());
        p.refuse_with(None);
        // too soon to try again
        step(&mut k, &p, 1_000, 1);
        assert_eq!(p.held(), 0);
        step(&mut k, &p, RETRY_MS, 1);
        assert_eq!(p.held(), 1);
        assert_eq!(k.view(RETRY_MS, LINGER).error, None);
    }

    #[test]
    fn r607_turning_it_off_releases_at_once() {
        let (mut k, p) = (Keeper::default(), FakePower::default());
        step(&mut k, &p, 0, 1);
        k.update(&p, 1, 1, false, LINGER);
        assert_eq!(p.held(), 0);
    }
}

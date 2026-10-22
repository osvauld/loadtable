//! The thin layer that wires [`decide`](crate::decide) to the platform traits.

use crate::gates::{Readings, check_gates};
use crate::{
    AdmissionError, AdmissionInput, Decision, DeviceState, Lane, SHORT_LANE_BASE, SlotState,
    Thresholds, WaitReason, Waiter, decide,
};
use std::collections::BTreeMap;
use std::path::PathBuf;
use tasks_core::{DeviceId, JobId, SlotKind};
use tasks_platform::{Disk, Memory, Pressure, PressureThresholds, SlotGuard, SlotLocks};

/// Which slots exist on this machine and who is known to hold them.
#[derive(Debug, Clone, Default)]
pub struct Layout {
    /// Slot count per kind (devices are listed in `devices`).
    pub counts: BTreeMap<SlotKind, u32>,
    /// Device lock index per device.
    pub devices: BTreeMap<DeviceId, u32>,
    /// Known holders from the ledger; unknown holders show as external.
    pub holders: BTreeMap<(SlotKind, u32), JobId>,
}

impl Layout {
    /// Compile slots clamped to 1..=8 (R-029), four run slots (R-030), four offload slots (R-031).
    pub fn standard(compile_slots: u32) -> Layout {
        Layout {
            counts: BTreeMap::from([
                (SlotKind::Compile, compile_slots.clamp(1, 8)),
                (SlotKind::Run, 4),
                (SlotKind::Offload, 4),
            ]),
            ..Layout::default()
        }
    }
}

/// Result of trying to start a job.
#[derive(Debug)]
pub enum Attempt {
    /// The slot is held by this guard; start the job and keep the guard for its lifetime.
    Admitted(SlotGuard),
    /// Not now; the lock was released.
    Wait(WaitReason),
}

/// Reads the machine and takes slot locks. Holds no policy of its own.
pub struct Admitter<'a> {
    /// Memory reader.
    pub memory: &'a dyn Memory,
    /// Pressure reader.
    pub pressure: &'a dyn Pressure,
    /// Disk reader.
    pub disk: &'a dyn Disk,
    /// Slot locks.
    pub locks: &'a dyn SlotLocks,
    /// Filesystem path where cache output lands.
    pub cache_path: PathBuf,
    /// Filesystem path where state lives.
    pub state_path: PathBuf,
    /// Machine policy, passed in.
    pub thresholds: Thresholds,
    /// PSI percentages that map to a memory [`Level`](tasks_platform::Level), passed in.
    pub psi: PressureThresholds,
}

impl Admitter<'_> {
    /// Measures the machine now. A failed reading is an error, so admission fails closed.
    pub fn readings(&self) -> Result<Readings, AdmissionError> {
        Ok(Readings {
            pressure: self.pressure.snapshot(&self.psi)?,
            available_memory: self.memory.snapshot()?.available_bytes,
            cache_free: self.disk.free_bytes(&self.cache_path)?,
            state_free: self.disk.free_bytes(&self.state_path)?,
        })
    }

    fn probe(&self, kind: SlotKind, index: u32) -> Result<bool, AdmissionError> {
        Ok(self.locks.try_acquire(kind, index)?.is_some())
    }

    fn slot_state(&self, kind: SlotKind, n: u32, l: &Layout) -> Result<SlotState, AdmissionError> {
        let mut st = SlotState::default();
        for i in 0..n {
            if self.probe(kind, i)? {
                st.free.push(i);
            } else {
                st.busy.push((i, l.holders.get(&(kind, i)).copied()));
            }
        }
        Ok(st)
    }

    /// Decides all waiters against the live machine. Probing releases each lock at once, so
    /// nothing is held afterwards; use [`Admitter::try_admit`] to take a slot.
    pub fn plan(
        &self,
        waiters: Vec<Waiter>,
        layout: &Layout,
    ) -> Result<Vec<Decision>, AdmissionError> {
        let r = self.readings()?;
        let mut slots = BTreeMap::new();
        for (kind, n) in &layout.counts {
            slots.insert(*kind, self.slot_state(*kind, *n, layout)?);
        }
        let mut short_lane = SlotState::default();
        for i in 0..self.thresholds.short_job_lane_slots {
            if self.probe(SlotKind::Compile, SHORT_LANE_BASE + i)? {
                short_lane.free.push(i);
            } else {
                short_lane.busy.push((i, None));
            }
        }
        let mut devices = BTreeMap::new();
        for (id, index) in &layout.devices {
            let free = self.probe(SlotKind::Device, *index)?;
            let holder = layout.holders.get(&(SlotKind::Device, *index)).copied();
            devices.insert(
                id.clone(),
                DeviceState {
                    index: *index,
                    free,
                    holder,
                },
            );
        }
        Ok(decide(&AdmissionInput {
            slots,
            short_lane,
            devices,
            pressure: r.pressure,
            available_memory: r.available_memory,
            cache_free: r.cache_free,
            state_free: r.state_free,
            thresholds: self.thresholds.clone(),
            waiters,
        }))
    }

    /// Takes `slot` and re-reads pressure, memory and disk under the lock. If a gate now
    /// blocks, the lock is released and the answer is `Wait`, never an error.
    pub fn try_admit(
        &self,
        waiter: &Waiter,
        slot: (SlotKind, u32),
        lane: Lane,
    ) -> Result<Attempt, AdmissionError> {
        if lane == Lane::Short && !matches!(slot.0, SlotKind::Compile | SlotKind::Run) {
            return Err(AdmissionError::NoShortLane(slot.0));
        }
        let Some(guard) = self.locks.try_acquire(slot.0, slot.1)? else {
            return Ok(Attempt::Wait(WaitReason::SlotTaken(slot.0, slot.1)));
        };
        let r = self.readings()?;
        match check_gates(slot.0, lane, waiter.new_output, &r, &self.thresholds) {
            Some(reason) => {
                drop(guard);
                Ok(Attempt::Wait(reason))
            }
            None => Ok(Attempt::Admitted(guard)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Verdict;
    use crate::testkit::Fake;
    use crate::testkit::{psi, thresholds, waiter};
    use std::sync::Mutex;
    use tasks_platform::{Level, PlatformError, PressureSnapshot, PressureThresholds};

    fn admitter<'a>(f: &'a Fake, p: &'a dyn Pressure) -> Admitter<'a> {
        Admitter {
            memory: f,
            pressure: p,
            disk: f,
            locks: f,
            cache_path: "/cache".into(),
            state_path: "/state".into(),
            thresholds: thresholds(),
            psi: psi(),
        }
    }

    fn fake() -> Fake {
        Fake::new()
    }

    fn calm() -> PressureSnapshot {
        PressureSnapshot {
            memory: Level::Normal,
            memory_some: None,
            memory_full: None,
            cpu: Some(5.0),
            io: Some(5.0),
        }
    }

    /// Returns the queued readings in order, then repeats the last.
    struct Script(Mutex<Vec<PressureSnapshot>>);
    impl Pressure for Script {
        fn snapshot(&self, _: &PressureThresholds) -> Result<PressureSnapshot, PlatformError> {
            let mut v = self
                .0
                .lock()
                .map_err(|_| PlatformError::parse("script", "poisoned"))?;
            if v.len() > 1 {
                Ok(v.remove(0))
            } else {
                v.first()
                    .copied()
                    .ok_or(PlatformError::parse("script", "empty"))
            }
        }
    }

    #[test]
    fn standard_layout_bounds_counts_r029_r030() {
        assert_eq!(Layout::standard(0).counts[&SlotKind::Compile], 1);
        assert_eq!(Layout::standard(99).counts[&SlotKind::Compile], 8);
        assert_eq!(Layout::standard(3).counts[&SlotKind::Run], 4);
        assert_eq!(Layout::standard(3).counts[&SlotKind::Offload], 4);
    }

    #[test]
    fn plan_admits_then_names_the_holder() {
        let fake = fake();
        let a = admitter(&fake, &fake);
        let mut layout = Layout::standard(1);
        layout.holders.insert((SlotKind::Compile, 0), JobId(4278));
        let held = fake.try_acquire(SlotKind::Compile, 0).unwrap();
        let d = a
            .plan(vec![waiter(1, SlotKind::Compile, 0)], &layout)
            .unwrap();
        assert!(matches!(&d[0].verdict, Verdict::Wait(r) if r.to_string().contains("#4278")));
        drop(held);
        let d = a
            .plan(vec![waiter(1, SlotKind::Compile, 0)], &layout)
            .unwrap();
        assert!(matches!(
            d[0].verdict,
            Verdict::Admit {
                slot: (SlotKind::Compile, 0),
                lane: Lane::Normal
            }
        ));
    }

    #[test]
    fn pressure_is_rechecked_after_lock_and_recovers_r055() {
        let fake = fake();
        let hot = PressureSnapshot {
            cpu: Some(90.0),
            ..calm()
        };
        let script = Script(Mutex::new(vec![calm(), hot, calm()]));
        let a = admitter(&fake, &script);
        let w = waiter(1, SlotKind::Compile, 0);
        let plan = a.plan(vec![w.clone()], &Layout::standard(1)).unwrap();
        let Verdict::Admit { slot, lane } = plan[0].verdict.clone() else {
            panic!("expected admit");
        };
        // pressure rose while we were deciding: back off, no error, no lock kept
        match a.try_admit(&w, slot, lane).unwrap() {
            Attempt::Wait(r) => assert!(r.to_string().contains("CPU contended")),
            Attempt::Admitted(_) => panic!("must wait"),
        }
        assert!(
            fake.try_acquire(slot.0, slot.1).unwrap().is_some(),
            "lock released"
        );
        // recovery admits the same waiter without any replay
        assert!(matches!(
            a.try_admit(&w, slot, lane).unwrap(),
            Attempt::Admitted(_)
        ));
    }

    #[test]
    fn waiting_on_pressure_holds_no_slot_r039() {
        let mut fake = fake();
        fake.pressure.cpu = Some(99.0);
        let a = admitter(&fake, &fake);
        let w = waiter(1, SlotKind::Compile, 0);
        assert!(matches!(
            a.try_admit(&w, (SlotKind::Compile, 0), Lane::Normal)
                .unwrap(),
            Attempt::Wait(_)
        ));
        assert!(fake.try_acquire(SlotKind::Compile, 0).unwrap().is_some());
    }

    #[test]
    fn disk_reserve_waits_and_never_errors_then_proceeds_r062() {
        let mut fake = fake();
        fake.disk_free = 1 << 30;
        let mut w = waiter(1, SlotKind::Compile, 0);
        w.new_output = true;
        {
            let a = admitter(&fake, &fake);
            let Attempt::Wait(r) = a
                .try_admit(&w, (SlotKind::Compile, 0), Lane::Normal)
                .unwrap()
            else {
                panic!("must wait");
            };
            assert!(
                r.to_string()
                    .starts_with("disk: cache has 1 GiB free, reserve 20 GiB")
            );
        }
        fake.disk_free = 100 << 30;
        let a = admitter(&fake, &fake);
        assert!(matches!(
            a.try_admit(&w, (SlotKind::Compile, 0), Lane::Normal)
                .unwrap(),
            Attempt::Admitted(_)
        ));
    }

    #[test]
    fn taken_slot_is_a_wait_not_an_error() {
        let fake = fake();
        let a = admitter(&fake, &fake);
        let _g = fake.try_acquire(SlotKind::Run, 2).unwrap();
        let w = waiter(1, SlotKind::Run, 0);
        assert!(matches!(
            a.try_admit(&w, (SlotKind::Run, 2), Lane::Normal).unwrap(),
            Attempt::Wait(WaitReason::SlotTaken(SlotKind::Run, 2))
        ));
    }

    #[test]
    fn short_lane_only_for_compile_and_run() {
        let fake = fake();
        let a = admitter(&fake, &fake);
        let w = waiter(1, SlotKind::Offload, 0);
        assert!(matches!(
            a.try_admit(&w, (SlotKind::Offload, 0), Lane::Short),
            Err(AdmissionError::NoShortLane(SlotKind::Offload))
        ));
    }

    #[test]
    fn plan_uses_the_short_lane_when_cpu_blocks() {
        let mut fake = fake();
        fake.pressure.cpu = Some(99.0);
        fake.pressure.io = Some(0.0);
        let a = admitter(&fake, &fake);
        let mut w = waiter(1, SlotKind::Compile, 0);
        w.predicted_secs = Some(10.0);
        let d = a.plan(vec![w.clone()], &Layout::standard(1)).unwrap();
        assert_eq!(
            d[0].verdict,
            Verdict::Admit {
                slot: (SlotKind::Compile, SHORT_LANE_BASE),
                lane: Lane::Short
            }
        );
        let Verdict::Admit { slot, lane } = d[0].verdict.clone() else {
            return;
        };
        assert!(matches!(
            a.try_admit(&w, slot, lane).unwrap(),
            Attempt::Admitted(_)
        ));
    }
}

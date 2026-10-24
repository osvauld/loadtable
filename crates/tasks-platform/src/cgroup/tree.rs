//! The cgroup tree under the daemon's delegated cgroup.
//!
//! ```text
//! <own>/                 the service cgroup systemd delegated to us; controllers enabled here
//!   daemon/              the daemon moves itself here (no-internal-processes rule)
//!   tasks.slice/         controllers enabled here too
//!     job-<id>/          one per job
//!     transfer/          sync and fetch work
//! ```

use super::fs::file;
use super::{
    CgroupFs, CgroupName, CgroupStatus, Cgroups, WANTED_CONTROLLERS, parse_cpu_usage,
    parse_io_stat, parse_psi_totals, parse_single_u64,
};
use crate::PlatformError;
use std::io;
use std::path::{Path, PathBuf};
use tasks_core::{Attribution, JobFigures};

/// The leaf the daemon's own processes live in.
const DAEMON_LEAF: &str = "daemon";
/// The slice holding job and transfer cgroups.
const SLICE: &str = "tasks.slice";

/// Job and transfer cgroups under one delegated cgroup.
pub struct CgroupTree {
    fs: Box<dyn CgroupFs>,
    slice: PathBuf,
    controllers: Vec<String>,
}

fn io_err(path: &Path, e: io::Error) -> PlatformError {
    PlatformError::io(path, e)
}

/// The text of a control file, or `None` when it does not exist (controller not enabled).
fn read_opt(fs: &dyn CgroupFs, path: &Path) -> Result<Option<String>, PlatformError> {
    match fs.read(path) {
        Ok(t) => Ok(Some(t)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io_err(path, e)),
    }
}

/// The unified (v2) path from `/proc/self/cgroup`: the line `0::/some/path`.
pub fn parse_self_cgroup(text: &str) -> Option<String> {
    text.lines()
        .find_map(|l| l.strip_prefix("0::"))
        .map(|p| p.trim().trim_end_matches(" (deleted)").to_string())
}

/// The delegated cgroup for a process whose own cgroup is `rel`. With `DelegateSubgroup=daemon`
/// systemd already starts the service in its `daemon` leaf, so the delegated cgroup is the
/// parent of that leaf.
pub fn delegated_root(rel: &str) -> &str {
    let rel = rel.trim_end_matches('/');
    match rel.strip_suffix("/daemon") {
        Some(parent) if !parent.is_empty() => parent,
        _ => rel,
    }
}

impl CgroupTree {
    /// Sets the tree up under `own`, the cgroup this process runs in and may write to:
    ///
    /// 1. creates `tasks.slice` (the first write, so a read-only or foreign cgroup fails here,
    ///    before anything moved),
    /// 2. moves process `pid` into the `daemon` leaf, because a cgroup with processes of its own
    ///    cannot enable controllers for children (the no-internal-processes rule),
    /// 3. enables the wanted controllers that `own` offers, in `own` and then in `tasks.slice`,
    /// 4. creates `tasks.slice/transfer`.
    ///
    /// A controller the kernel or systemd does not offer (or refuses) is left out; its figures
    /// are then absent, not an error.
    pub fn prepare(
        fs: Box<dyn CgroupFs>,
        own: &Path,
        pid: u32,
    ) -> Result<CgroupTree, PlatformError> {
        let slice = own.join(SLICE);
        fs.mkdir(&slice).map_err(|e| io_err(&slice, e))?;
        let leaf = own.join(DAEMON_LEAF);
        fs.mkdir(&leaf).map_err(|e| io_err(&leaf, e))?;
        let procs = file(&leaf, "cgroup.procs");
        fs.write(&procs, &pid.to_string())
            .map_err(|e| io_err(&procs, e))?;
        let offered = read_opt(&*fs, &file(own, "cgroup.controllers"))?.unwrap_or_default();
        let mut enabled = Vec::new();
        for c in WANTED_CONTROLLERS {
            if !offered.split_whitespace().any(|o| o == c) {
                continue;
            }
            let own_ctl = file(own, "cgroup.subtree_control");
            if fs.write(&own_ctl, &format!("+{c}")).is_err() {
                continue;
            }
            // the slice sees the controller only once `own` passed it down
            let slice_ctl = file(&slice, "cgroup.subtree_control");
            if fs.write(&slice_ctl, &format!("+{c}")).is_ok() {
                enabled.push(c.to_string());
            }
        }
        let transfer = slice.join(CgroupName::Transfer.dir_name());
        fs.mkdir(&transfer).map_err(|e| io_err(&transfer, e))?;
        Ok(CgroupTree {
            fs,
            slice,
            controllers: enabled,
        })
    }

    fn dir(&self, name: CgroupName) -> PathBuf {
        self.slice.join(name.dir_name())
    }
}

impl Cgroups for CgroupTree {
    fn status(&self) -> CgroupStatus {
        CgroupStatus::Delegated {
            slice: self.slice.clone(),
            controllers: self.controllers.clone(),
        }
    }

    fn create(&self, name: CgroupName) -> Result<PathBuf, PlatformError> {
        let dir = self.dir(name);
        self.fs.mkdir(&dir).map_err(|e| io_err(&dir, e))?;
        Ok(file(&dir, "cgroup.procs"))
    }

    fn set_cpu_weight(&self, name: CgroupName, weight: u32) -> Result<bool, PlatformError> {
        if !self.controllers.iter().any(|c| c == "cpu") {
            return Ok(false);
        }
        let path = file(&self.dir(name), "cpu.weight");
        self.fs
            .write(&path, &weight.to_string())
            .map_err(|e| io_err(&path, e))?;
        Ok(true)
    }

    fn pids(&self, name: CgroupName) -> Result<Vec<i32>, PlatformError> {
        let path = file(&self.dir(name), "cgroup.procs");
        Ok(read_opt(&*self.fs, &path)?
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.trim().parse::<i32>().ok())
            .collect())
    }

    fn read(&self, name: CgroupName) -> Result<JobFigures, PlatformError> {
        let dir = self.dir(name);
        let get = |f: &str| read_opt(&*self.fs, &file(&dir, f));
        let mut fig = JobFigures::empty(Attribution::Cgroup);
        if let Some(t) = get("io.stat")? {
            let io = parse_io_stat(&t)?;
            fig.io_read_bytes = Some(io.rbytes);
            fig.io_write_bytes = Some(io.wbytes);
            fig.io_read_ops = Some(io.rios);
            fig.io_write_ops = Some(io.wios);
        }
        // `memory.peak` needs kernel 5.19; older kernels fall back to the current usage
        let peak = match get("memory.peak")? {
            Some(t) => parse_single_u64("memory.peak", &t)?,
            None => None,
        };
        fig.mem_peak_bytes = match peak {
            Some(p) => Some(p),
            None => match get("memory.current")? {
                Some(t) => parse_single_u64("memory.current", &t)?,
                None => None,
            },
        };
        if let Some(t) = get("memory.current")? {
            fig.mem_now_bytes = parse_single_u64("memory.current", &t)?;
        }
        if let Some(t) = get("cpu.stat")? {
            fig.cpu_usec = Some(parse_cpu_usage(&t)?);
        }
        if let Some(t) = get("io.pressure")? {
            let p = parse_psi_totals(&t)?;
            fig.io_some_us = Some(p.some_us);
            fig.io_full_us = p.full_us;
        }
        if let Some(t) = get("memory.pressure")? {
            let p = parse_psi_totals(&t)?;
            fig.mem_some_us = Some(p.some_us);
            fig.mem_full_us = p.full_us;
        }
        if let Some(t) = get("cpu.pressure")? {
            fig.cpu_some_us = Some(parse_psi_totals(&t)?.some_us);
        }
        Ok(fig)
    }

    fn kill(&self, name: CgroupName) -> Result<(), PlatformError> {
        let path = file(&self.dir(name), "cgroup.kill");
        self.fs.write(&path, "1").map_err(|e| io_err(&path, e))
    }

    fn remove(&self, name: CgroupName) -> Result<(), PlatformError> {
        let dir = self.dir(name);
        match self.fs.rmdir(&dir) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            // EBUSY: processes (or child cgroups) remain
            Err(e) if e.kind() == io::ErrorKind::ResourceBusy || e.raw_os_error() == Some(16) => {
                Err(PlatformError::CgroupBusy(dir))
            }
            Err(e) => Err(io_err(&dir, e)),
        }
    }

    fn list_jobs(&self) -> Result<Vec<i64>, PlatformError> {
        let names = self
            .fs
            .subdirs(&self.slice)
            .map_err(|e| io_err(&self.slice, e))?;
        Ok(names
            .iter()
            .filter_map(|n| match CgroupName::from_dir_name(n) {
                Some(CgroupName::Job(id)) => Some(id),
                _ => None,
            })
            .collect())
    }
}

/// Finds this process's own cgroup and prepares the tree there. `Err` is the reason the daemon
/// falls back to process groups, written for an operator.
#[cfg(target_os = "linux")]
pub(super) fn setup_native() -> Result<CgroupTree, String> {
    const ROOT: &str = "/sys/fs/cgroup";
    if !Path::new(ROOT).join("cgroup.controllers").exists() {
        return Err(format!("cgroup v2 is not mounted at {ROOT}"));
    }
    let text = std::fs::read_to_string("/proc/self/cgroup")
        .map_err(|e| format!("cannot read /proc/self/cgroup: {e}"))?;
    let rel = parse_self_cgroup(&text)
        .ok_or_else(|| "/proc/self/cgroup has no cgroup v2 entry".to_string())?;
    let own = Path::new(ROOT).join(delegated_root(&rel).trim_start_matches('/'));
    CgroupTree::prepare(Box::new(super::RealFs), &own, std::process::id()).map_err(|e| {
        format!(
            "{e}; run tasks2d as a systemd user service with Delegate=yes (see deploy/v2/worker-unit.md)"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::super::fs::fake::DirFs;
    use super::*;

    fn own(tag: &str, controllers: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("tasks-cg-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        for f in super::super::fs::fake::FILES {
            std::fs::write(p.join(f), "").unwrap();
        }
        std::fs::write(p.join("cgroup.controllers"), controllers).unwrap();
        p
    }

    fn tree(tag: &str, controllers: &str) -> (CgroupTree, PathBuf, DirFs) {
        let own = own(tag, controllers);
        let fs = DirFs::default();
        let t = CgroupTree::prepare(Box::new(fs.clone()), &own, 4242).unwrap();
        (t, own, fs)
    }

    fn put(dir: &Path, f: &str, text: &str) {
        std::fs::write(dir.join(f), text).unwrap();
    }

    // R-324
    #[test]
    fn r324_a_service_started_in_its_daemon_leaf_delegates_the_parent() {
        assert_eq!(
            delegated_root("/a/b/tasks2d.service/daemon"),
            "/a/b/tasks2d.service"
        );
        assert_eq!(
            delegated_root("/a/b/tasks2d.service"),
            "/a/b/tasks2d.service"
        );
        assert_eq!(delegated_root("/daemon"), "/daemon");
    }

    #[test]
    fn self_cgroup_line_is_found() {
        let text = "0::/user.slice/user-1000.slice/user@1000.service/app.slice/tasks2d.service\n";
        assert_eq!(
            parse_self_cgroup(text).as_deref(),
            Some("/user.slice/user-1000.slice/user@1000.service/app.slice/tasks2d.service")
        );
        assert_eq!(parse_self_cgroup("1:name=x:/a\n"), None);
    }

    // R-324
    #[test]
    fn r324_the_daemon_moves_to_a_leaf_before_controllers_are_enabled() {
        let (t, own, fs) = tree("leaf", "cpuset cpu io memory pids\n");
        let procs = std::fs::read_to_string(own.join("daemon/cgroup.procs")).unwrap();
        assert_eq!(procs.trim(), "4242");
        let own_ctl = std::fs::read_to_string(own.join("cgroup.subtree_control")).unwrap();
        assert_eq!(own_ctl, "+io\n+memory\n+cpu\n");
        let slice_ctl =
            std::fs::read_to_string(own.join("tasks.slice/cgroup.subtree_control")).unwrap();
        assert_eq!(slice_ctl, "+io\n+memory\n+cpu\n");
        assert!(own.join("tasks.slice/transfer").is_dir());
        // the move came before the first controller write
        let log = fs.log.lock().unwrap().clone();
        let move_at = log
            .iter()
            .position(|l| l.contains("daemon/cgroup.procs"))
            .unwrap();
        let first_ctl = log
            .iter()
            .position(|l| l.contains("subtree_control"))
            .unwrap();
        assert!(move_at < first_ctl, "{log:?}");
        let CgroupStatus::Delegated { controllers, .. } = t.status() else {
            panic!("not delegated");
        };
        assert_eq!(controllers, ["io", "memory", "cpu"]);
    }

    // R-325
    #[test]
    fn r325_a_missing_controller_is_left_out_and_its_figures_are_none() {
        let (t, own, _) = tree("noio", "cpu memory\n");
        let CgroupStatus::Delegated { controllers, .. } = t.status() else {
            panic!("not delegated");
        };
        assert_eq!(controllers, ["memory", "cpu"]);
        let job = CgroupName::Job(5);
        t.create(job).unwrap();
        let dir = own.join("tasks.slice/job-5");
        // the kernel has no io.stat or io.pressure without the io controller
        std::fs::remove_file(dir.join("io.stat")).unwrap();
        std::fs::remove_file(dir.join("io.pressure")).unwrap();
        put(&dir, "memory.peak", "4096\n");
        put(&dir, "cpu.stat", "usage_usec 77\nuser_usec 70\n");
        put(
            &dir,
            "memory.pressure",
            "some avg10=0 avg60=0 avg300=0 total=11\nfull avg10=0 avg60=0 avg300=0 total=5\n",
        );
        put(
            &dir,
            "cpu.pressure",
            "some avg10=0 avg60=0 avg300=0 total=3\n",
        );
        let f = t.read(job).unwrap();
        assert_eq!(f.quality, Attribution::Cgroup);
        assert_eq!(f.io_read_bytes, None);
        assert_eq!(f.io_some_us, None);
        assert_eq!(f.mem_peak_bytes, Some(4096));
        assert_eq!(f.cpu_usec, Some(77));
        assert_eq!(
            (f.mem_some_us, f.mem_full_us, f.cpu_some_us),
            (Some(11), Some(5), Some(3))
        );
    }

    // R-325
    #[test]
    fn r325_figures_parse_io_memory_and_cpu_and_peak_falls_back_to_current() {
        let (t, own, _) = tree("fig", "io memory cpu\n");
        let job = CgroupName::Job(9);
        t.create(job).unwrap();
        let dir = own.join("tasks.slice/job-9");
        put(
            &dir,
            "io.stat",
            "8:0 rbytes=1000 wbytes=2000 rios=4 wios=5 dbytes=0 dios=0\n",
        );
        put(
            &dir,
            "io.pressure",
            "some avg10=1 avg60=1 avg300=1 total=2500\nfull avg10=0 avg60=0 avg300=0 total=900\n",
        );
        std::fs::remove_file(dir.join("memory.peak")).unwrap();
        put(&dir, "memory.current", "777\n");
        put(&dir, "cpu.stat", "usage_usec 5\n");
        let f = t.read(job).unwrap();
        assert_eq!(
            (f.io_read_bytes, f.io_write_bytes),
            (Some(1000), Some(2000))
        );
        assert_eq!((f.io_read_ops, f.io_write_ops), (Some(4), Some(5)));
        assert_eq!((f.io_some_us, f.io_full_us), (Some(2500), Some(900)));
        assert_eq!(f.mem_peak_bytes, Some(777));
    }

    // R-574
    #[test]
    fn r574_the_current_memory_is_read_apart_from_the_peak() {
        let (t, own, _) = tree("memnow", "io memory cpu\n");
        let job = CgroupName::Job(10);
        t.create(job).unwrap();
        let dir = own.join("tasks.slice/job-10");
        put(&dir, "memory.peak", "900\n");
        put(&dir, "memory.current", "300\n");
        let f = t.read(job).unwrap();
        assert_eq!((f.mem_peak_bytes, f.mem_now_bytes), (Some(900), Some(300)));
    }

    // R-323
    #[test]
    fn r323_each_job_gets_its_own_cgroup_and_its_procs_file() {
        let (t, own, _) = tree("place", "io memory cpu\n");
        let a = t.create(CgroupName::Job(1)).unwrap();
        let b = t.create(CgroupName::Job(2)).unwrap();
        assert_eq!(a, own.join("tasks.slice/job-1/cgroup.procs"));
        assert_eq!(b, own.join("tasks.slice/job-2/cgroup.procs"));
        // creating twice reuses it
        assert_eq!(t.create(CgroupName::Job(1)).unwrap(), a);
        let mut jobs = t.list_jobs().unwrap();
        jobs.sort();
        assert_eq!(jobs, [1, 2]);
        put(&own.join("tasks.slice/job-1"), "cgroup.procs", "10\n11\n");
        assert_eq!(t.pids(CgroupName::Job(1)).unwrap(), [10, 11]);
        assert!(t.pids(CgroupName::Job(99)).unwrap().is_empty());
    }

    // R-710
    #[test]
    fn r710_the_cpu_weight_of_a_job_is_written_when_the_cpu_controller_is_enabled() {
        let (t, own, fs) = tree("weight", "io memory cpu\n");
        let job = CgroupName::Job(8);
        t.create(job).unwrap();
        assert!(t.set_cpu_weight(job, 50).unwrap());
        let f = own.join("tasks.slice/job-8/cpu.weight");
        assert_eq!(std::fs::read_to_string(f).unwrap().trim(), "50");
        let log = fs.log.lock().unwrap().join("\n");
        assert!(log.contains("job-8/cpu.weight: 50"), "{log}");
    }

    // R-710
    #[test]
    fn r710_without_the_cpu_controller_no_weight_is_written_and_the_caller_is_told() {
        let (t, own, _) = tree("noweight", "io memory\n");
        let job = CgroupName::Job(9);
        t.create(job).unwrap();
        assert!(!t.set_cpu_weight(job, 200).unwrap());
        let f = own.join("tasks.slice/job-9/cpu.weight");
        assert_eq!(std::fs::read_to_string(f).unwrap().trim(), "");
        assert!(
            !crate::NoCgroups::new("none")
                .set_cpu_weight(job, 200)
                .unwrap()
        );
    }

    // R-329
    #[test]
    fn r329_an_empty_job_cgroup_is_removed_and_a_populated_one_is_busy() {
        let (t, own, _) = tree("rm", "io memory cpu\n");
        let job = CgroupName::Job(3);
        t.create(job).unwrap();
        put(&own.join("tasks.slice/job-3"), "cgroup.procs", "55\n");
        assert!(matches!(t.remove(job), Err(PlatformError::CgroupBusy(_))));
        assert!(own.join("tasks.slice/job-3").is_dir());
        put(&own.join("tasks.slice/job-3"), "cgroup.procs", "");
        t.remove(job).unwrap();
        assert!(!own.join("tasks.slice/job-3").exists());
        // already gone is fine
        t.remove(job).unwrap();
    }

    // R-328
    #[test]
    fn r328_kill_writes_one_to_cgroup_kill() {
        let (t, own, fs) = tree("kill", "io memory cpu\n");
        t.create(CgroupName::Job(4)).unwrap();
        t.kill(CgroupName::Job(4)).unwrap();
        let log = fs.log.lock().unwrap().clone();
        assert!(
            log.iter().any(
                |l| l == &format!("{}: 1", own.join("tasks.slice/job-4/cgroup.kill").display())
            ),
            "{log:?}"
        );
    }

    // R-326
    #[test]
    fn r326_without_write_access_setup_fails_before_anything_moves() {
        struct ReadOnly;
        impl CgroupFs for ReadOnly {
            fn read(&self, _: &Path) -> io::Result<String> {
                Ok(String::new())
            }
            fn write(&self, _: &Path, _: &str) -> io::Result<()> {
                Err(io::Error::from(io::ErrorKind::PermissionDenied))
            }
            fn mkdir(&self, _: &Path) -> io::Result<()> {
                Err(io::Error::from(io::ErrorKind::PermissionDenied))
            }
            fn rmdir(&self, _: &Path) -> io::Result<()> {
                Err(io::Error::from(io::ErrorKind::PermissionDenied))
            }
            fn subdirs(&self, _: &Path) -> io::Result<Vec<String>> {
                Ok(Vec::new())
            }
        }
        let err = CgroupTree::prepare(Box::new(ReadOnly), Path::new("/x"), 1)
            .err()
            .unwrap();
        assert!(err.to_string().contains("/x/tasks.slice"), "{err}");
    }

    #[test]
    fn names_round_trip_through_directory_names() {
        for n in [CgroupName::Job(12), CgroupName::Transfer] {
            assert_eq!(CgroupName::from_dir_name(&n.dir_name()), Some(n));
        }
        assert_eq!(CgroupName::from_dir_name("job-x"), None);
        assert_eq!(CgroupName::from_dir_name("daemon"), None);
    }
}

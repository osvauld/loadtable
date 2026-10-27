# Trial feedback: safety/CLI fix validation

- Agent: safety-update agent
- Shared jobs: 25 (final compiler validation), 21 (new-client summary against old
  daemon), 29 (post-promotion new-worker pressure/cancellation check).
- Outcome: source checks and bounded real checks passed; full admitted-load
  validation remains unavailable under current headroom, not fabricated/skipped
  as though it passed.
- Evidence: 94 Rust tests passed, none failed/ignored, clippy/build exit 0;
  /tmp/task-manager-safety-final-validation.log. Four Python clock tests passed.
- Injected PSI/memory models exercise each refusal, recovery and post-lock recheck.
  No synthetic CPU/RAM/I/O stress was generated on the active desktop.
- Real private and shared new-worker smoke: actual low-memory gate queued work,
  logs identified the reason, own cancellation completed without workload exec.
  Shared job 29. Logs plain text equalled --json log text.
- CLI feedback resolved: final stdout job/result/exit/owner summary survives tail;
  logs default to text with explicit --json; legacy cpu_simulation label documented
  as generic and distinct from scheduling resource, preserving history/schema.
- Promoted only after idle lease/all-terminal checks; 28 prior rows preserved.
  Existing active jobs, old v1 daemon, device sessions, outputs and data untouched.
- Current host limitation: about 16 GiB allocated in RAM-backed /tmp, no swap,
  available memory below new 8 GiB reserve. New jobs intentionally queue; operator
  review is required, not hidden admission bypass or automatic data deletion.
- Suggestion: read-only, bounded ownership-aware /tmp inventory and clearer
  operator headroom guidance. Acceptance: identify candidate-owned versus unknown
  data without touching active handles/artifacts, make no deletion inference, and
  verify real bounded workload execution after headroom is restored.

See SAFETY-UPDATE.md for exact policy, limits and evidence paths. Default two Cargo
jobs is cooperative, not a cgroup/linker-thread cap. Trusted scripts can override
it; no universal sandbox or guarantee against future freezes is claimed.

# Resource-safety and CLI feedback update

## Motivation and evidence

The freeze investigation traced core job 13 through the NEW native Rust path,
not Python buildlock. Its env-wrapped Cargo invocation compiled many test targets.
The generic sanitizer explicitly set CARGO_BUILD_JOBS to CPU-count minus two.
Several simultaneous linkers were observed with ~1.1–1.2 GiB RSS each; host PSI
full I/O avg10 reached ~84%, with memory stalls and no swap. This is evidence of
resource contention, not proof that job 13 alone caused the entire freeze.
Background Java/browser processes also used memory. `/tmp` is tmpfs with about
16 GiB allocated; no cleanup was performed or authorized by these changes.

## Deployed policy

- Generic sanitized commands now default to **CARGO_BUILD_JOBS=2**, not CPU count.
- Direct Cargo and transparent `env NAME=value cargo ...` commands reject explicit
  job counts outside 1–2, including env overrides. Transparent env-wrapped Cargo
  tests now get compile/no-run then run staging, preserving the env assignments.
- Admission requires **8 GiB MemAvailable**, CPU some avg10 <=40%, memory some/full
  avg10 <=1%/0.5%, and I/O some/full avg10 <=20%/10%. Missing/malformed metrics
  refuse admission. Checks occur before and after lock acquisition.
- `tasks admission` adds the PSI metrics and pressure_blocker. Waiting logs name
  the blocker without flooding every sample. Waits remain cancellable and do not
  hold compile/run locks while pressure is excessive. Device work also needs its
  usual gated run slot; device authorization is unchanged.

Thresholds are conservative trial heuristics, NOT reservations/quotas. There is
no forced interruption of already-running jobs, hard linker-thread cap, cgroup
memory limit, universal shell/script parser or guarantee against a future freeze.
Trusted scripts can explicitly override environment or launch their own tools;
follow the trial policy. A Cargo -j cap limits simultaneous Cargo jobs, not each
linker's internal threads. Low headroom intentionally queues even small run jobs.
Do not weaken gates or bypass admission just to make a validation pass.

## CLI feedback addressed

- `tasks run` ends stdout with `job ID passed/failed/... (exit N) owner=OWNER`,
  after log output, even if that output had no trailing newline.
- `tasks logs ID` prints only log text. `tasks logs ID --json` preserves the full
  existing v2 response for scripts. Socket API/UI JSON contracts are unchanged.
- The existing `cost=cpu_simulation` value is a legacy generic local-command
  label, NOT actual resource classification or a claim of simulation. Scheduling
  uses recipe.resource and stage. It is documented instead of changing persisted
  enum values/history or breaking existing clients.

## Validation (no stress generation)

- `/tmp/task-manager-safety-final-validation.log`: **94 Rust tests passed,
  0 failed/ignored**, warnings-denied all-targets clippy and build exit 0.
  Commands ran via the installed shared queue, explicit Cargo -j 2.
- Injected PSI/memory samples cover invalid data, boundaries, each pressure
  refusal, cancellation without acquiring compile, and pressure rising between
  precheck and lock acquisition then recovering. Actual private flock files are
  exercised, but these are deterministic models, not load/production proof.
- `tests/safety_smoke.py` is bounded: eight-second observation, cancellation,
  independent CLI queries, plain/JSON log equality, owned daemon cleanup. If real
  host policy permits, only a shell with CPU/address-space limits runs. If not,
  it verifies that no workload marker is created. It never fakes host pressure.
  `/tmp/task-manager-safety-bounded-smoke.json`: PASSED waiting/cancellation,
  NO workload execution because the host was below 8 GiB. Fixture
  `~/build-cache/tm-fixtures/safety-smoke-bht66c9p`.
- New CLI against the existing pre-update daemon: real bounded `true` shared job
  **21** passed, final summary verified; job 11 plain logs exactly matched --json.
  `/tmp/task-manager-safety-cli-summary.log`, `...-plain-log.txt`, `...-json-log.json`.
  This verifies CLI compatibility, NOT new-worker resource policy execution.
- Updated native_commands.py/self_suite.py use explicit --json; native_commands
  adds cap, summary and text-log assertions. The complete native-command, crash
  E2E and sealed self-suite have NOT been rerun for this candidate. An unborn
  standalone repo still doesn't satisfy the historical sealed self-suite adapter.

## Deployment

Only promote an immutable copied release while the shared worker/cleanup lease
is idle and no queued/active jobs exist. Do not interrupt another agent's job or
replace Cargo outputs being executed. Preserve shared history, stable lock inodes,
old releases, old v1 service and all device sessions. No automatic cache cleanup.

Promotion waited while jobs 27/28 were active, then acquired the idle shared
worker/cleanup lease and confirmed all jobs terminal before replacing only the
shared native daemon. **28 existing job rows were preserved**; old v1/device
sessions were untouched. No commits, staging, pushes or cache deletion.

Installed binary SHA-256:
`cef8acfdbb2f7afc32bfdc053066f5b66642c9a86d35235fe9b724283367d0f3`.
Release identity/source fingerprint: `/tmp/task-manager-native-deployment.json`.
Promotion log: `/tmp/task-manager-safety-promotion.log`.

Post-promotion bounded SHARED smoke passed: `/tmp/task-manager-safety-shared-check.json`.
The new shared worker waited on real low-memory pressure, logged why, and honoured
cancellation without running the test workload. Plain/JSON logs matched. This is
real denial/cancellation evidence, not successful admitted-load validation.

At rollout the host had roughly 6 GiB available, below the 8 GiB reserve, so new
jobs intentionally wait. Query `tasks admission` (pressure_blocker) and job logs.
Restoring headroom requires operator action/review; no old /tmp data or Java/
browser process was deleted/killed, and the threshold wasn't lowered for tests.

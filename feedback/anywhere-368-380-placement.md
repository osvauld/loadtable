# Feedback: `tasks run --anywhere` placement (jobs 368–380)

Owner `anywhere`, worktree `/home/abe/task-manager-worktrees/anywhere`, branch
feature/anywhere from d5a7502.

## What was built
- `src/placement.rs` (pure, unit-tested) decides between this machine and the
  LAN worker from two cluster snapshots. It also strips Cargo parallelism and
  rewrites the worker command as a `tasks-offload` client.
- `src/bin/tasks/anywhere.rs`: reads the local snapshot from the daemon and the
  worker's over ssh (`tasks status --json`, killed after 8 s), plus
  `~/.config/tasks/offload`; `tasks place` dry run.
- `--anywhere` in `tasks run|enqueue`. It conflicts with --run/--offload/--device.

## Jobs
| job | command | result |
|-----|---------|--------|
| 368 | cargo test --lib placement | failed: E0507 (moved `Phase`, not Copy) |
| 369 | cargo test --lib placement | passed, 8 placement tests |
| 370 | cargo test | passed (193 lib + bins + integration; 2+1+1 ignored as before) |
| 371 | cargo clippy --all-targets | passed, no warnings |
| 372, 374 | cargo build --release --bin tasks | passed |
| 373 | cargo test --lib placement::tests | passed, 8 tests after the no-worker fix |
| 375 | real `tasks run --anywhere -- cargo check -j 2 --bin tasks` | wrongly placed on the worker (worker #127, queued behind #126). Client stopped by my `timeout 900`; the job is still live and was not cancelled |
| 376 | cargo test -j 2 --lib placement::tests | failed: the old tie assertion depended on the bug |
| 377–379 | placement tests (9), full suite (194 lib + bins), clippy, all with -j 2 on the laptop | passed |
| 380 | cargo build -j 2 --release --bin tasks | passed |

## Bug found live (fixed)
With no history for the command kind on either machine, the laptop's guess was
600 s ×4 = 40 min and the worker's was 10 min. Job 375 therefore went to a worker
with a 25 min wait (35 min in total) rather than to the idle laptop (40 min). Now
both machines get the same guess when nothing is known, so the wait decides. A
tie now means equal totals. Regression test:
`idle_laptop_beats_a_held_worker_slot_for_a_small_unknown_crate`. Live re-check:
`cargo check --bin tasks` → kurukkan (~0s wait + ~10m) vs nixos (~19m + ~10m).

Process: builds and tests for this crate belong on the laptop with plain
`tasks run -- cargo … -j 2`. Offloading a small crate to a busy worker was the
wrong call (coordinator direction).

## Observations
- Under load, the worker's memory PSI gate is set almost whenever it builds
  (7.6 GiB). The laptop shows memory PSI at times too: 22 GiB swap is in use.
  "Exclude a blocked machine" therefore often became "both blocked → score both".
  Treating a blocker as extra wait, instead of excluding the machine, may be the
  better rule.
- The 8:2 jobs ratio is a crude conversion. One worker p2pcore run (47 min) scales
  to 187 min on the laptop. Real laptop runs of that kind will replace the guess
  once they exist (median of passed runs of the same kind).
- Laptop offload runs (`[offload] …` kind) give the worker estimate when the
  worker's own 50-job finished window has no run of that kind.
- Placement is advisory and runs once at submit. A job that has been placed
  never migrates, even if the other machine frees up first.
- `tasks place` from zsh needs `${=cmd}` when the command is in a variable;
  otherwise the whole command is one argv word and the -j strip finds no `cargo`.

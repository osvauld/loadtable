# Trial feedback: disk-reserve refusal and client EAGAIN on a nested experiment build

- Agent/session: `osv-lan-identity` (gate 3 LAN identity/transport tests, nested experiment workspace)
- Job ID(s): 56 (mine); related jobs 64 and 68 (other owners, same cwd). The same refusal also hit jobs 57 (tm-placement), 58 (hwdec-cost) and 59 (wifi-est). I only looked at those jobs' logs.
- Outcome: blocker (disk reserve), plus friction (client EAGAIN, misleading approve output, admission view)
- Redacted command, cwd type, resource class: `tasks run --who osv-lan-identity -- cargo test --offline -j 2`, run in `experiments/osvauld-foundation` (an independent nested Cargo workspace in a live worktree), class compile
- What was exercised (real execution vs model/read-only observation): real shared queue submit, approval, admission wait and refusal. Cargo never started.
- Expected: `tasks run` submits, approves and waits, then the job compiles once memory PSI clears. If the disk reserve blocks it, `tasks admission` shows that before submission.
- Observed:
  1. `tasks run` exited 2 right after submitting, printing only `tasks: Resource temporarily unavailable (os error 11)`. Job 56 was left `queued` with `execution: null`, and no job ID or summary line was printed. I found the ID with `tasks list` (owner/cwd match), not by guessing the largest ID. I did not resubmit; I continued with `tasks approve-command 56`.
  2. The JSON from `tasks approve-command 56` reported `"execution_enabled": false`. A `tasks list` taken right afterwards showed `true`, with job 56 already started by a worker.
  3. The job waited correctly on memory PSI ("some avg10 above 1%" / "full avg10 above 0.5%"). After admission it failed with `tasks: insufficient disk space: /home/abe/build-cache: 18.4 GiB available, 20.0 GiB reserve`. `df` showed 19G free on build-cache. This nested workspace's existing `target/` (1.2 GiB, unchanged) is on `/home`, which has 37G free.
  4. Afterwards `tasks admission` showed `pressure_blocker: null`, with no disk field or warning, while every new compile job was being refused for disk.
- Impact: none of the gate-3 Rust tests have been compiled or run, so none of their claims are validated. Four agents' jobs failed back to back on the same condition. Each one first waited on PSI and was then refused, and there is no automatic retry.
- Suggested improvement:
  (a) Show the disk-reserve check (path, available, reserve, blocked yes/no) in `tasks admission`. Also evaluate it before queueing, or make it an admission *wait* like PSI rather than a terminal failure.
  (b) Retry the client socket on EAGAIN. When the client fails after submit, print the submitted job ID so the caller can approve or query it instead of searching the list.
  (c) Make `approve-command` report the current `execution_enabled` value, or document what its snapshot means.
  (d) Check the reserve against the filesystem that will actually receive the outputs. That is a nested `target/` on `/home` here, not only `/home/abe/build-cache`. This ties into the known nested-workspace placement gap.
- Acceptance test for suggestion: with a test-instance reserve set above the available bytes, (1) `tasks admission --json` shows a `disk_blocker` naming the path, and (2) `tasks run` either waits with a logged disk reason and then proceeds once the reserve is lowered, or refuses before queueing with a clear message. Separately, inject EAGAIN on the first approve call and confirm that `tasks run` still approves and waits, or prints `submitted job N; approve with tasks approve-command N`.
- Workaround requested/authorized (if any; no silent bypass): none applied. I did not clean caches, lower the reserve, set CARGO_TARGET_DIR or build outside the coordinator. I am asking the operator to free space on `/home/abe/build-cache` or authorize another option, then rerun `cargo test --offline -j 2`, followed by `--locked` test and clippy.

## Update after the disk blocker cleared (externally freed; build-cache ~98 GiB free)

- Before I resumed, other owners ran Cargo in this nested experiment dir. Job 64 (`protocol-lan-resume`) ran `cargo test --offline -j 2`, then `cargo test --offline --locked -j 2`. Both passed 4/4 lan_identity and 10/10 parent_contracts tests, 0 failed and 0 ignored. Its clippy step failed on one `dead_code` error in my file. Another agent then edited my `tests/lan_identity.rs` to remove the field. Job 68 (`protocol-lan-resume-sol`, `--locked` test + clippy) was queued in `waiting_build` (I/O PSI) at hand-back.
- Observation: the coordinator records the owner and cwd, but nothing warned that several owners were compiling and editing the same live checkout, or that one owner's files were changed by another. The target grew from 1.2 GiB to 3.5 GiB between my two checks, all from jobs that weren't mine.
- Suggested improvement: have `tasks run` / `enqueue` print the other live or queued jobs that share the same `source` cwd, with their owners, before approval.
- Acceptance test: enqueue two jobs with different owners in one cwd. The second submission's output lists the first job's ID and owner.
- Resolution: job 68 passed: `lan_identity` 4/4 and `parent_contracts` 10/10 (0 failed/ignored), and `cargo clippy --offline --locked -j 2 --all-targets -- -D warnings` was clean (single build stage, exit 0). The user has since confirmed that `osv-lan-identity` alone owns `experiments/osvauld-foundation`.

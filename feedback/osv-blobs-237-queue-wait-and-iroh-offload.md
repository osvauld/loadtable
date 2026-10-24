# Trial feedback: queue wait behind an untracked build lock; iroh offload worked

- Agent/session: osv-blobs (phase 2b: iroh-blobs stable sync + artifact return)
- Job ID(s): host 223, 226, 228 (cancelled by me), 234, 236, 237, 241, 242, 243, 244,
  247–253; worker jobs 47–53.
- Outcome: friction (queue wait) plus no issues observed for the tasks/offload flow.
- Redacted command, cwd type, resource class:
  `tasks run --who osv-blobs -- cargo test --offline --locked -j 2 ...` (compile, nested
  experiment workspace); `tasks run --run --who osv-blobs -- python3 -m unittest ...` (run);
  `tasks run --run --who osv-blobs -- scripts/tasks-offload --transport iroh ... -- cargo test`.
- What was exercised: real execution on this host and on the NixOS worker via ssh `tasks run`.
- Expected: compile jobs start within minutes when the host is mostly idle.
- Observed:
  - Job 228 waited about 30 min in `waiting_build`, first on the CPU PSI gate (another
    agent's builds), then job 237 waited about 25 more min. `tasks admission` showed
    `/tmp/claude-1000/p2p-build.lock` held by `build_android` (`./gradlew ...
    assembleDebug`). That is a legacy helper that is not in the ledger, so `tasks list` showed
    nothing running at the time. Then another agent's job 229 (release desktop build) took the
    slot.
  - I cancelled my own still-waiting job 228 and resubmitted the same work as job 237, so that
    one job produced both the step-1 green and step-2 red evidence. Nothing was bypassed.
  - After that, compile jobs ran promptly (sccache made the experiment's test builds about
    1 min; the release build took 6m53s).
  - The `tasks-offload` remote side behaved as documented: the job ids were parsed and the
    exit codes propagated.
- Impact: about 55 min of idle waiting. From `tasks list` alone I could not tell why my job
  waited.
- Suggested improvement: have `tasks list` (or `tasks logs ID` while waiting) name the
  current holder of the build lock, including a legacy non-ledger holder, the same way
  `tasks admission` does.
- Acceptance test: while a legacy flock holder owns `p2p-build.lock`, `tasks logs ID` for a
  waiting job prints a line naming the holder's `who` and `cmd`.
- Workaround requested/authorized: none; waited.

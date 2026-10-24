# Trial feedback: desktop VP8 encode batch (cargo test/clippy/check, non-cargo script)

- Agent/session: vp8enc-core (worktree desktop-vp8-encode)
- Job ID(s): 9 (python3 scripts/build_libvpx.py, passed), 10 (cargo test -p video, vp8, passed),
  12 (cargo test -p proto, passed), 13 and 17 (cargo test -p p2pcore, passed), 14 (same, debug
  profile, failed), 15 (cargo test --release -p p2pcore --test video_calls, passed), 16 (cargo check
  aarch64-linux-android, passed), 18 (cargo clippy, reported failed)
- Outcome: friction (minor)
- Redacted command, cwd type, resource class: `tasks run --who vp8enc-core -- [env VPX_DIR=... ] cargo ...`
  from the live worktree; default admission (compile); job 9 was a plain Python script.
- What was exercised (real execution vs model/read-only observation): real execution of all jobs above.
- Expected: job 14 fails only on a real defect; job 18 reflects clippy's result.
- Observed: job 14 failed on two timing assertions (frames per 3 s) in a debug build of software
  AV1; the same tests passed in job 15 (release), so it was the known debug-timing limit, not a
  task-manager issue. Job 18 shows "failed", most likely because I piped the client output into
  `head` and the pipe closed (unverified; clippy printed only warnings that predate my change).
  Foreground `tasks run` over the harness's 120 s tool timeout was moved to the background by the
  harness; the job kept running and finished normally. The list output for job 16 embeds my whole
  PATH in the stored argv (long, includes unrelated tool dirs).
- Impact: small. Needed a manual look to tell "failed because of the test" from "failed because of
  my pipe".
- Suggested improvement: record in the job whether the client detached/lost its stdout (SIGPIPE) so
  a closed pipe is not shown as a job failure; optionally redact/trim env values in `tasks list`.
- Acceptance test for suggestion: run `tasks run -- clippy ... | head -1`; the job phase must match
  clippy's real exit status, and `tasks list` must show a trimmed env for a 1 KB PATH.
- Workaround requested/authorized (if any; no silent bypass): none.

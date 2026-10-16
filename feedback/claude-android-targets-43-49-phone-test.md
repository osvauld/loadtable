# Trial feedback: phone/emulator targets and first real phone test

- Agent/session: claude-android-targets (Claude Code), worktree branch
  `android-phone-emulator-targets`, 2026-10-16.
- Job ID(s): 43 (cargo test), 44 (clippy, failed on my own lint), 45 (cargo
  build), 46 (clippy rerun), 47 (typed Android trial, phone/arm64-v8a build),
  49 (device-locked install + launch on Pixel 3a `9BWAY1LSDC`).
- Outcome: no defects observed; one friction point (progress visibility).
- Redacted command, cwd type, resource class:
  `tasks run --who claude-android-targets -- cargo test|clippy|build --offline --locked -j 2`
  (live worktree, compile slot); `target/debug/tasks phone-test --who claude-android-targets
  /home/abe/p2p_chat --device 9BWAY1LSDC` (job 47: snapshot of p2p_chat `dev`
  `66566cb`, compile slot; job 49: device lock only).
- What was exercised: real execution. 99 Rust tests passed, 0 failed/ignored;
  clippy `-D warnings` clean after fixing one `cloned_ref_to_slice_refs` lint in
  a new test. Read-only `tasks devices` against one real phone. Job 47 built the
  first arm64-only APK through the task manager: build stage 604.6 s observed,
  Gradle reported `BUILD SUCCESSFUL in 9m 3s`, APK 166,253,574 bytes, SHA-256
  `829ca2b5f0b18d9e189098afbc174c1feb042ef64adf61f9009728ef30bcd3ab`. Job 49
  ran `adb install -r` (Success) and a launcher intent (`Events injected: 1`)
  in 10.0 s. Whether the app behaved correctly after launch was NOT checked.
- Expected: phone-only build skips x86_64; install waits for the device lock,
  not the compile slot.
- Observed: as expected. Memory-PSI admission blocker was present at submission
  and cleared without intervention. The install job's argv[0] resolved to the
  per-checkout cache target path rather than `./target/debug/tasks`, which is
  correct but surprising in the log.
- Impact: `phone-test` prints only phase changes for ~10 minutes, then dumps the
  whole ~650-line build log at the end. Hard to tell libaom vs Cargo vs Gradle
  progress while waiting.
- Suggested improvement: while a job is Building, print the current Android
  step name (already recorded in `android-steps.json`) on change, and print only
  a bounded tail of the log at completion unless `--full-log` is given.
- Acceptance test: a fake multi-step Android job yields one stderr line per step
  transition during the wait and at most N log lines at completion; `--full-log`
  restores today's output.
- Workaround requested/authorized: none. User authorized the initial commit,
  worktree, build/test runs and install/launch on the connected Pixel.

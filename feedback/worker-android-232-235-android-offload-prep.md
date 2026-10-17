# Trial feedback: preparing p2p_chat Android builds on the LAN worker

- Agent/session: worker-android (feature/worker-android)
- Job ID(s): host 232 / worker 41 (`cargo test --lib worker::tests`), host 235 / worker 43
  (`cargo clippy --all-targets -D warnings && cargo test`), both via `tasks-offload`.
  No Android build submitted: the worker has no Android SDK yet (licence step pending).
- Outcome: friction (two offload source-policy gaps for Gradle projects); offload itself worked.
- Redacted command, cwd type, resource class: `tasks run --run --who worker-android --
  tasks-offload --worker abe@WORKER --ssh-key KEY [--cargo-fetch] -- CMD`, git worktree, host run
  slot + worker compile slot.
- What was exercised: real offload of this worktree (119 files, 1.48 MB) to the NixOS worker;
  `--dry-run` of a local p2p_chat clone (1261 files, 23.6 MB). Read-only nix evaluation on the worker.
- Expected: tracked project files reach the worker; Gradle can find its SDK.
- Observed:
  1. `worker_source` excludes the tracked `android/gradle.properties` (dry run: `excluded:
     android/gradle.properties`). It holds no secrets in p2p_chat (`org.gradle.jvmargs=-Xmx3g`,
     `android.useAndroidX`, `android.nonTransitiveRClass`, `rustRelease`). Credentials live in
     the user-level `~/.gradle/gradle.properties`, which is never in a checkout.
  2. `local.properties` is git-ignored and also excluded, so a worker checkout never has
     `sdk.dir`; the fix-disk auto-create (not yet deployed on the worker) reads only the
     daemon's ANDROID_HOME/ANDROID_SDK_ROOT. A job that passes `env ANDROID_HOME=... ./gradlew`
     in argv would still be failed before admission by that check when the daemon has none.
  3. Offload jobs 41/43 ran cleanly; clippy + full test (135+23+11+... tests, 1 ignored) passed
     on the worker in 354 s run time, while the host compile slot was busy with another owner's
     Gradle build, so offload avoided queueing on the host.
- Impact: (1) an offloaded Gradle build silently runs with a 512 MiB default Gradle heap and
  differs from the host build; must be compensated with `-Dorg.gradle.jvmargs=-Xmx3g
  -Pandroid.useAndroidX=true -Pandroid.nonTransitiveRClass=true` on the command line.
  (2) needs the daemon unit to set ANDROID_HOME, now passed through (this branch).
- Suggested improvement: (1) exclude `gradle.properties` only outside a Gradle project root,
  or only when it contains `*PASSWORD*`/`*STORE_FILE*` keys; report it as a warning instead.
  (2) `ensure_android_local_properties` should also honour `env ANDROID_HOME=/abs` assignments
  in the argv before failing.
- Acceptance test: (1) a dry run of a checkout with a tracked `android/gradle.properties`
  without secret keys syncs it; one with `RELEASE_STORE_PASSWORD=` is still excluded.
  (2) unit test: argv `env ANDROID_HOME=<existing dir> ./gradlew assembleDebug` with an empty
  daemon env creates local.properties from that value instead of erroring.
- Workaround requested/authorized: none; command-line `-D`/`-P` flags are the planned workaround.

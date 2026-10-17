# Shared task-manager trial — agent instructions

This is an experimental facility. Agents must use it for new direct heavy
workloads and help evaluate it, not silently work around problems. Authoritative
source: `/home/abe/task-manager`, a separate repository outside Tinline.

## Deployed safety update

The safety release was promoted in an idle window, preserving 28 existing jobs.
Cargo defaults to two jobs; **keep explicit `-j 2` in Cargo builds/tests** for
clarity and don't increase it. Existing jobs weren't interrupted or duplicated.
Read SAFETY-UPDATE.md for policy and measured validation limits. Runtime identity
is recorded in `/tmp/task-manager-native-deployment.json`; source builds alone
are not deployments.

The deployed worker defaults Cargo to two jobs and requires 8 GiB available memory plus
CPU/memory/I/O PSI headroom. This may deliberately queue work on this host; inspect
`tasks admission`/job logs, report the blocker, and do not bypass or lower gates.
No cgroup quota or guaranteed prevention of future resource exhaustion is claimed.

## Use

```sh
# Inspect first; all agents use the same default shared state.
tasks list
tasks admission
# From YOUR intended live checkout:
tasks run --who YOUR_UNIQUE_AGENT -- cargo check --offline --locked -j 2 -p PACKAGE
# Run-only jobs must not compile:
tasks run --who YOUR_UNIQUE_AGENT --run -- /absolute/path/to/built-test
# Another agent can query while this runs:
tasks logs JOB_ID
# Logs are plain text by default; scripts should use:
tasks logs JOB_ID --json
```

`tasks run` explicitly submits, approves, starts and waits for YOUR command.
Separate approval: `tasks enqueue ...`, capture response.submitted_id, then
`tasks approve-command ID` and `tasks start-worker`. Never infer your ID from the
largest listed job. Ctrl-C detaches, not cancels. Query the printed job ID before
retrying. `tasks cancel ID` is for your own work only; wait for cleanup.

Commands run with your user rights in a LIVE cwd, not sealed source snapshots.
Keep the checkout stable. Do not put credentials in argv or feedback. Caller
shell environment isn't automatically copied; pass needed NON-SECRET inputs
explicitly, not cache-policy overrides. Do not override RUSTC_WRAPPER,
CARGO_TARGET_DIR or GRADLE_USER_HOME or share worktree Cargo targets.

Default state remains `~/.local/state/tinline-task-manager-native`; leave
TASK_MANAGER_STATE_DIR unset for shared work. Existing XDG state overrides still
apply: if your shared view differs, report that rather than submitting twice.

Do not nest buildlock.py, build_android.py or an internally admitted executor
inside `tasks run`. Legacy helpers still share the same flock files and may
finish existing jobs; their jobs are NOT imported into the durable task ledger.
If your workload needs a legacy internally admitted helper, explain that
limitation in feedback rather than nesting it or pretending it was tracked.
Hardware operations need separate explicit authorization; a device resource flag
is NOT permission to install, launch, wipe, sign or publish.

## Broad `cargo test` policy (source branch feature/log-search, not deployed)

Once deployed, a `cargo test` with a name filter but no target selector is refused at submit
(exit 2, nothing queued) because it builds every test target of the package. Name the targets
the refusal suggests (`--lib`, `--test NAME`); pass `--broad-test` only when you really need
every target, and say why in your feedback. Do not edit `~/.config/tasks/policy.huml` or set
`TASKS_BROAD_TEST=warn` to get around a refusal: that is the operator's setting.

## Feedback is required

After meaningful use in each agent session:

1. Include a short **Task-manager trial feedback** section in your task report.
2. Create your own unique `feedback/<agent>-<job-id>-<topic>.md` using
   `feedback/TEMPLATE.md`. Never append to another agent's file. Include actual
   IDs, observation, impact, proposed improvement and a concrete acceptance test.
3. If it worked, record "No issues observed" with what you exercised. Don't
   manufacture improvements, durations, coverage or success claims.
4. For a blocker: inspect list/admission/logs; record evidence and ask for an
   authorized workaround. Don't bypass locks, kill another agent, reset the
   daemon, replay mutations or duplicate the job to make progress.

The stored local.command `cost=cpu_simulation` is a legacy generic label, NOT a
simulation claim or scheduling class. Recipe resource/stage selects actual locks;
changing this enum/history is not necessary to fix the misleading terminology.

Useful feedback: discovery/help, enqueue/approval flow, queue fairness, compile/
run classification, environment surprises, concurrency, cancellation, recovery,
query/log readability, UI, result accuracy, build-cache placement. Distinguish
observations from hypotheses and models from real execution.

Feedback is source-controlled review material, NOT private diagnostics. Use a
redacted command and small relevant excerpts, not complete logs, environment,
identity material, private source or credentials. Full logs/state remain outside
Git. Do not stage, commit or push feedback/code without authorization.

## Operational boundaries

No sandbox, hard resource quotas, global FIFO against legacy clients, automatic
cleanup, historical legacy-job import or zero-downtime upgrade is promised.
The current immutable self-suite/Android contracts are Tinline-specific adapters;
normal source-capsule core/desktop catalog entries remain proposals. Live Cargo
commands are available separately. No normal Android rollout or new device
validation is implied by extracting this repository.

This trial does not authorize changes to other agents' worktrees/services/data.
Existing sessions don't automatically reload instructions; reread this document
before the next workload and keep an honest record of adoption limitations.

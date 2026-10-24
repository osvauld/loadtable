# Trial feedback: reroute feature, all heavy checks offloaded to the NixOS worker

- Agent/session: reroute (resumed session; feature/reroute worktree)
- Job ID(s): laptop 523, 541, 567, 571, 573, 575, 578, 580, 581, 582
  (worker-side 158, 164, 168, 176, 179, 181, 183 and others)
- Outcome: friction (long queue on the worker's compile slot); no defect in the offload path
- Redacted command, cwd type, resource class:
  `tasks run --offload --who reroute -- tasks-offload --worker abe@WORKER --who reroute
  --ssh-key KEY -- cargo clippy|test --locked -j 6 ...`, own worktree, offload slot locally,
  compile slot on the worker.
- What was exercised: real execution of clippy and the full test suite on the worker,
  including the hermetic two-daemon tests in tests/peer.rs.
- Expected: the offload starts quickly (the lead said the worker was idle).
- Observed:
  - The first two offloads (523, 541) sat in the worker's WaitingBuild for about 30 minutes
    each, then ran (42m wall). Later ones went through in minutes. Locally the job only shows
    `running`. The worker-side queue position and holder show up only as one
    `tasks: job N WaitingBuild` line in `tasks logs`.
  - A foreground `tasks run` capped at the 10-minute tool budget detaches (as documented),
    so every check needed a separate poll loop on `tasks list`. Nothing was lost or duplicated.
  - Pre-existing and unrelated to this branch: `crates/peer` `sync_checkout` fails on the
    worker (mode 600 where 644 is expected; looks like umask). Under worker load
    `tests/peer.rs` `cancel_on_either_side_propagates` and `source_sync_...` timed out once
    each.
  - Worker host PSI (I/O/memory) put both fixture daemons under the 10m pressure blocker.
    That is visible in the reroute log line (`~20m wait` instead of `~10m`).
- Impact: about 1h of wall time spent waiting behind other agents on one worker compile slot,
  which is the situation this feature addresses for the laptop.
- Suggested improvement: in the local job's `tasks list`/`tasks logs`, show the worker-side
  queue position and holder, e.g. `worker: waiting_build, position 3, slot held by #157`.
- Acceptance test for suggestion: with the worker's compile slot held, a local offload job's
  `tasks list --brief` row shows the remote phase and position, not just `running`.
- Workaround requested/authorized: none. No lock bypass and nothing cancelled.

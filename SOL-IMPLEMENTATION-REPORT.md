# Sol implementation report — bounded Linux-worker groundwork

## Scope / ownership

Worked only in `/home/abe/task-manager-worktrees/osvauld-foundation`, branch
`feature/osvauld-foundation`, HEAD unborn. No commits/staging or main/parent
source changes. Verified instructions, status, tasks list/admission first.

Added:
- `scripts/setup-linux-worker.sh` (executable), `scripts/linux_worker_setup.py`
- `scripts/worker_source.py`
- `tests/test_linux_worker_contracts.py`
- `scripts/README-linux-worker.md`
- `docs/design/linux-worker-local-contracts.md`
- `feedback/sol-linux-worker-41-local-contracts.md`
- This report.

Edited only `REMOTE-WORKER-HANDOVER.md` and
`docs/design/osvauld-foundation.md` for status/validation. All 60 original
baseline files matched `/tmp/task-manager-osvauld-foundation-baseline.json`.
Because HEAD is unborn, empty `git diff` is not evidence of unchanged files.

## Implemented

- Bounded JSON setup inspect/plan; Ubuntu 24.04/Debian 12 x86_64/aarch64
  inspection scope. Unsupported hosts report clearly and exit 2. Apply always
  refuses. No tools/shims invoked: PATH metadata is evidence, versions unknown.
  RAM/swap/CPU, disk/cache and KVM/DRI metadata do not imply build/emulator/GPU
  runtime readiness. Unknown package pins/checksums/download budgets block apply.
- Explicit individual-file, no-HEAD source capture: dirty/untracked inputs,
  normalized bounded paths/count/bytes/depth, Linux no-follow ancestor opens,
  links/hardlinks/special-file rejection, known credential/cache/output denial.
  Two inventories detect ordinary edits; no atomic snapshot/security sandbox
  or secret-scanning claim. Caller updates the selected set for additions/removals.
- Frozen retained bytes, versioned SHA-256 receipt, path/mode-aware diff and
  deduplicated missing-content plan/retrieval bound to the expected receipt.
  Renames reuse bytes, mode changes affect manifests, deletions affect only
  new manifests. No workspace reread for retrieval. No disk blob store,
  materializer, wire protocol, authenticated possession or execution approval.
  Rust snapshot framing/pattern reused with a distinct domain; existing closed
  HEAD/layout-specific adapters and SHA-256 receipts untouched. Iroh hashes absent.

## Exact validation

All shared jobs used installed `tasks`, unique owner `sol-linux-worker`, `--run`;
no Cargo invocation or nested admission. Source stayed stable during jobs.

| Job | Check | Result |
| --- | --- | --- |
| 37 | `env PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s tests -p test_linux_worker_contracts.py -v` before modules | Expected red: 1 import/loader error, job exit 1 |
| 38 | Same command after implementation | 10 passed, 0 failed/skipped, job exit 0 |
| 40 | Same command after expanding tests | 14 run, 5 new credential/state-name subtest errors (before exclusions), job exit 1 |
| 41 | `env PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s tests -p 'test_*.py' -v` | **18 passed**, 0 failed/skipped; 14 new + 4 existing wait-budget, exit 0 |
| 42 | `python3 -B -c …` subprocess/JSON assertions for setup `--inspect`, `--plan`, `--apply` | Passed, job exit 0; EndeavourOS x86_64 correctly unsupported; each CLI exit 2 |

Job 42 client timed out after 20 seconds during admission, then queried by its
printed ID and confirmed `phase=passed`, stage exit 0. No duplicate or cancel.
Logs retained via `tasks logs ID`; local JSON evidence in
`/tmp/sol-linux-worker-job{38,40,41,42}.json` and
`/tmp/sol-linux-worker-host-cli.json` (not Git).

Additional light checks: `sh -n scripts/setup-linux-worker.sh`, AST parse and
trailing-whitespace checks on the three owned Python files, and SHA-256
comparison of all 60 original baseline files: passed. `git diff --check` exited
0 but is not meaningful for this untracked baseline.

Not run: Rust tests/clippy/build, worker E2E/self-suite/native-command suites,
real supported-distro installation/build, KVM ioctl, GPU/emulator execution,
remote auth/transfer/execution or distributed cancellation. No ignored/skipped
Python tests; unrun suites are not passes. Supported-distro/KVM permission
coverage uses synthetic fixtures only.

## Blockers / next step

Nested experiment target placement remains blocked, explicitly recorded before
validation in the local contract. Existing ~1.2 GiB target preserved; no build,
move/delete, cache sharing or environment-policy override attempted. Any fix
needs separate ownership/review and non-destructive direct/compound-command,
existing-output and checkout-isolation tests before more heavy Cargo work.

Review the local contracts first, then separately select worker/package
provenance and setup pins (apply still gated), or address target placement.
Authenticated iroh/iroh-blobs, LAN peer confinement with public discovery AND
relays disabled, independent installation identity/linking, per-task approval,
durable remote attempts/cancellation and Android runtime remain future gates.
Same mnemonic/device key and account-membership limitations are unchanged.
No security/product policy was silently settled beyond this local milestone.

## Task-manager trial feedback

Admission worked for bounded run-only checks alongside another agent's compile.
Jobs 40/42 honored real memory PSI waits; timeout reconciliation worked without
resubmission. Existing nested-output placement and full-history query verbosity
remain follow-ups; no bypass, restart, deployment replacement or unrelated job
cancellation. Required unique feedback is
`feedback/sol-linux-worker-41-local-contracts.md`.

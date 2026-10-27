# Alpha launch todo

What stands between `dev` and LoadTable 0.1.0, the first public alpha.

## 1. Name and code

- [x] Merge `rename-loadtable` (binaries `loadtable`/`loadtabled`, moves the tasks2 install on first start) and `release-ci` (CI, release builds, R2 publishing) into v2 and dev.
- [ ] Install the renamed build on the laptop, nixos and the Mac (`deploy/v2/rename-loadtable.md`), each while idle; rename the MCP entry in `~/.claude.json` to `loadtable`. (owner; Mac: friend)
- [ ] Laptop: remove the stale `~/.cargo/bin/tasks2` (24 Oct), which comes before `~/.local/bin` on PATH. (owner)
- [x] Version stamp: the git commit in `--version`, `status` and peer status, so every machine shows the build it runs.
- [x] Status text: "starts in ~0s" when the blocker has no estimate; "is using the remote copy" says too little.
- [x] Flaky tests in the gate: r345, r364 (also r609, r569, r568) fail in full runs and pass alone. CI will hit them too.

## 2. Before the repository goes public

- [x] The personal phone's serial is compiled in (`DENIED_DEVICES` in `crates/tasks-core/src/job.rs`): make denied devices a machine-policy setting and drop the serial from code and tests.
- [ ] LAN addresses and machine names in samples and tests (`192.168.1.7`, `nixos`, `kurukkan`): private-range and generic, left as they are unless you want them gone. (owner)
- [ ] The git history keeps all of the above. Decide: publish the history as is, or publish a squashed history. (owner)
- [ ] `feedback/`, `deploy/v2/` and the internal parts of `docs/` (agent feedback, machine notes): keep out of the public repo or move to a private one. (owner)
- [x] Root `README.md` (what it is, install, link to the site) and a `LICENSE` file (Cargo says MIT; there is no file).
- [x] MCP tool descriptions mention "the personal phone": reword for any user.

## 3. GitHub

- [ ] Rename `osvauld/task-manager` to `osvauld/loadtable` (GitHub redirects the old URL) and make it public; the site's clone link expects that name. (owner)
- [ ] Settings → Environments → `release`: secrets `R2_ACCESS_KEY_ID`, `R2_SECRET_ACCESS_KEY`; variables `R2_BUCKET`, `R2_ENDPOINT`; required reviewers; deploys from `v*` tags only. (owner)
- [ ] Protect `main`: require the CI checks to pass. (owner)
- [ ] First CI run on GitHub: fix whatever the hosted runners break (the tests have only run on our machines).
- [ ] Issues on, with a short bug template (OS, `loadtable --version`, `loadtable status`).

## 4. First release

- [ ] Fast-forward `main` to `dev`, tag `v0.1.0`, push the tag. (owner)
- [ ] Test the drafted binaries: Linux x86-64 on the laptop, arm64 if any, the Mac. Then publish the draft; the R2 job copies them to repo.osvauld.com/loadtable. (owner)
- [ ] Release notes: what works, the known gaps below, and that it is for a handful of personal machines you trust.
- [ ] The installer on a clean account on Linux and macOS: `curl -fsSL https://loadtable.osvauld.com/install.sh | sh`, `loadtabled install`, pair two machines, run a job across them.

## 5. Website

- [ ] Merge `cloudflare-hosting`, `no-trailing-slash`, then `install-script` into `main` in loadtable_landing; push `install-script` only after the release is on R2. (owner)
- [ ] `main` there still has the Netlify-based installer commit `77e6c51`: move it back to `e9751a5` first.
- [x] Docs: a "Security model" page (a paired machine runs commands as your user; pairing is the trust boundary; what crosses the network).
- [x] Docs: the status page lists the known gaps of 0.1.0.
- [ ] Commit the LoadTable card on the osvauld landing site. (owner approval)

## Known gaps, for the release notes

- The broad-test rule reads a libtest option's value as a filter: `cargo test -- --test-threads 32` is refused (`--test-threads=32` works).

- R-802: a sleeping Mac is not handled.
- R-797 part 2: a waiting job assigned to a peer is not moved when another machine frees up.
- Cancelling a job after it passed marks it cancelled (job 738).
- `policy show` does not print `[placement]`.
- MCP `get` does not show the target peer before a job is forwarded.
- `cancel` refusals do not suggest `--who`.
- Placement has no GPU or file capability requirements.
- Cache `gc`/`relink` is not designed yet; idle caches are emptied by hand.
- Release builds have no desktop window (`loadtable ui`) and macOS builds are not signed or notarized.

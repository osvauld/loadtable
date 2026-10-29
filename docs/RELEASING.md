# Releasing LoadTable

Three workflows in `.github/workflows`:

- `ci.yml`: every push to main, dev or v2 and every pull request. Linux: fmt, clippy with warnings as
  errors, every test (with the MCP server). macOS: clippy, with and without the iroh transport.
- `release.yml`: a pushed `vX.Y.Z` tag. Builds `loadtable` and `loadtabled` for
  `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu` (both on Ubuntu 22.04, glibc 2.35),
  `aarch64-apple-darwin` and `x86_64-apple-darwin`, and drafts the GitHub release with them.
- `publish-r2.yml`: publishing that release (or a manual run with its tag). Copies the binaries to
  `https://repo.osvauld.com/loadtable/` and moves `loadtable/latest` forward.

The installer on the website (`curl -fsSL https://loadtable.osvauld.com/install.sh | sh`, in the
loadtable_landing repository) reads `loadtable/latest` and downloads from that prefix.

## One-time GitHub configuration

repo.osvauld.com is the R2 bucket that also serves Tinline (see the Tinline repository's
`docs/r2-publishing.md`). LoadTable writes only under `loadtable/`.

The repository is `osvauld/loadtable` (renamed from `osvauld/task-manager`; GitHub redirects the
old URL, so existing clones keep pushing). CI needs no secrets. Publishing needs, in Settings →
Environments → `release`:

| Kind | Name | Value |
|---|---|---|
| Secret | `R2_ACCESS_KEY_ID` | access key of an R2 API token with Object Read & Write on the bucket |
| Secret | `R2_SECRET_ACCESS_KEY` | that token's secret |
| Variable | `R2_BUCKET` | the bucket behind repo.osvauld.com (the same as Tinline's `R2_BUCKET`) |
| Variable | `R2_ENDPOINT` | `https://<account-id>.r2.cloudflarestorage.com` (the same as Tinline's) |

GitHub never shows a secret again, so the values come from Cloudflare (R2 → Manage API tokens), not
from Tinline's settings: reuse Tinline's token or, better, create one for LoadTable so either can be
revoked alone. R2 tokens are per bucket, not per prefix, so the workflow itself keeps to
`loadtable/` and refuses to overwrite a published file. Tinline's APT signing secrets are not
needed: LoadTable publishes archives, not packages.

Add required reviewers to the environment so a publish waits for approval, and under deployment
branches and tags allow only tags matching `v*`.

## Cutting a release

1. Set `[workspace.package] version` in `Cargo.toml` to the new `X.Y.Z` and merge it to main. The
   tag must match it; the workflow refuses otherwise.
2. `git tag vX.Y.Z && git push origin vX.Y.Z`.
3. Wait for Release builds; it drafts "LoadTable vX.Y.Z" with four archives, their `.sha256` files
   and `SHA256SUMS`.
4. Download an archive for each machine you have and try it: `loadtable --version`, then
   `loadtabled install` and a job on a spare machine.
5. Publish the draft. Approve the `release` environment if it asks. The R2 job checks the public
   URLs at the end.
6. Check the installer: `curl -fsSL https://loadtable.osvauld.com/install.sh | sh` on a clean
   account, then `~/.local/bin/loadtable --version`.

Published files are never replaced: rerunning the publish for a tag uploads the same bytes or stops.
To fix a bad release, release a new version. `latest` only moves forward, so republishing an old tag
does not downgrade new installs.

## What release builds leave out

- The desktop window (`loadtable ui`): it links graphics libraries that headless workers lack. Build
  from source with `--features ui,mcp` for it.
- Code signing and notarization on macOS. `curl` does not set the quarantine flag, so the installer's
  binaries run; a browser download of the archive needs `xattr -d com.apple.quarantine` on each
  binary.
- Packages (APT, Homebrew) and self-update.

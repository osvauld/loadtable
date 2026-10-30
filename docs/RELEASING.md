# Releasing LoadTable

Three workflows in `.github/workflows`:

- `ci.yml`: every push to main, dev or v2 and every pull request. Linux: fmt, clippy with warnings as
  errors, every test (with the MCP server and the desktop window). macOS: clippy with and without
  the iroh transport, and the confinement profile run under the real `sandbox-exec`.
- `release.yml`: every push to main, and a pushed `vX.Y.Z` tag. On main it stops at once when the
  workspace version is already tagged; otherwise it builds `loadtable` (with the MCP server and the desktop
  window) and `loadtabled` for `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu` (both on
  Ubuntu 22.04, glibc 2.35), `aarch64-apple-darwin` and `x86_64-apple-darwin`, tags the commit it
  built as `vX.Y.Z` (on main), and drafts the GitHub release with them and the notes in
  `docs/releases/vX.Y.Z.md`. A manual run on a branch only builds.
- `publish-r2.yml`: publishing that release (or a manual run with its tag). Copies the binaries to
  LoadTable's R2 bucket, served at `https://dl.loadtable.osvauld.com`, and moves `latest` forward.

The installer on the website (`curl -fsSL https://loadtable.osvauld.com/install.sh | sh`, in the
loadtable_landing repository) reads `https://dl.loadtable.osvauld.com/latest` and downloads
`releases/vX.Y.Z/loadtable-X.Y.Z-<target>.tar.gz` from there.

## One-time configuration

The bucket is LoadTable's own, with `dl.loadtable.osvauld.com` connected as its custom domain
(Cloudflare: R2 → the bucket → Settings → Custom Domains). Its `r2.dev` address stays off.

The repository is `osvauld/loadtable` (renamed from `osvauld/task-manager`; GitHub redirects the
old URL, so existing clones keep pushing). CI needs no secrets. Publishing needs, in Settings →
Environments → `release`:

| Kind | Name | Value |
|---|---|---|
| Secret | `R2_ACCESS_KEY_ID` | Access Key ID of an R2 API token with Object Read & Write on the bucket only |
| Secret | `R2_SECRET_ACCESS_KEY` | that token's Secret Access Key |
| Variable | `R2_BUCKET` | the bucket's name |
| Variable | `R2_ENDPOINT` | `https://<account-id>.r2.cloudflarestorage.com`, shown with the token |

Create the token in Cloudflare under R2 → Manage API tokens → Create API token; the keys are shown
once. The workflow refuses to overwrite a published file, so a rerun can never change a release.

Add required reviewers to the environment so a publish waits for approval, and under deployment
branches and tags allow only tags matching `v*`.

## Cutting a release

1. On dev, set `[workspace.package] version` in `Cargo.toml` to the new `X.Y.Z` (and the workspace
   crates in `Cargo.lock`) and write the release notes in `docs/releases/vX.Y.Z.md`.
2. Merge dev to main by pull request. Nobody tags: Release builds sees a version with no tag,
   builds it, tags that commit `vX.Y.Z` and drafts "LoadTable vX.Y.Z" with four archives, their
   `.sha256` files and `SHA256SUMS`. Without the notes it fails and says which file to add.
   Later merges with the same version find the tag and do nothing, so bump the version to release
   again.
3. Wait for Release builds to finish.
4. Download an archive for each machine you have and try it: `loadtable --version`, then
   `loadtabled install` and a job on a spare machine.
5. Publish the draft. Approve the `release` environment if it asks. The R2 job checks the public
   URLs at the end.
6. Check the installer: `curl -fsSL https://loadtable.osvauld.com/install.sh | sh` on a clean
   account, then `~/.local/bin/loadtable --version`.

Pushing a tag by hand still works (`git tag vX.Y.Z && git push origin vX.Y.Z`); it must match the
version and have its notes. A wrong tag stops main from releasing that version: delete it
(`git push origin :refs/tags/vX.Y.Z`, and its draft release) and rerun Release builds on main.

Published files are never replaced: rerunning the publish for a tag uploads the same bytes or stops.
To fix a bad release, release a new version. `latest` only moves forward, so republishing an old tag
does not downgrade new installs.

## What release builds leave out

- Code signing and notarization on macOS. `curl` does not set the quarantine flag, so the installer's
  binaries run; a browser download of the archive needs `xattr -d com.apple.quarantine` on each
  binary.
- Packages (APT, Homebrew) and self-update.

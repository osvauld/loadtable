#!/usr/bin/env bash
# Build p2p_chat's desktop binary (p2p-desktop) on the NixOS LAN worker through tasks-offload,
# make it portable (standard /lib64 loader, no /nix/store RUNPATH, static libaom/libvpx),
# fetch it and check that it loads on this host. See deploy/worker/README.md
# ("Portable desktop builds from the worker").
#
# Run from a p2p_chat checkout root (one no other agent is editing), under a host offload slot
# (own lock pool, no CPU PSI gate; use --run instead with a `tasks` that rejects --offload):
#   tasks run --offload --who OWNER -- /abs/path/scripts/offload-portable-desktop.sh --who OWNER
#
#   --who OWNER        tasks/tasks-offload owner (required except with --check-only)
#   --worker U@H       default abe@192.168.1.7
#   --ssh-key PATH     default ~/.ssh/id_ed25519_nixos_worker
#   --jobs N           cargo/cmake jobs on the worker, 1..8 (default 4; `sh -c` is not capped)
#   --preflight        only probe the worker's build deps (remote run slot, no compile) and
#                      name every missing tool/library; exit 1 if anything is missing
#   --check-only BIN   skip the build; only run the local portability checks on BIN
#
# The smoke start passes an unknown argument: p2p-desktop rejects it before it creates a data
# dir, opens a window, touches the keyring or starts the network. LD_BIND_NOW=1 makes the loader
# resolve every imported symbol first, so a missing library or symbol version fails here.
set -euo pipefail

who="" worker="abe@192.168.1.7" key="$HOME/.ssh/id_ed25519_nixos_worker" jobs=4
check_only="" preflight=""
while (($#)); do
  case "$1" in
    --who) who="$2"; shift 2 ;;
    --worker) worker="$2"; shift 2 ;;
    --ssh-key) key="$2"; shift 2 ;;
    --jobs) jobs="$2"; shift 2 ;;
    --preflight) preflight=1; shift ;;
    --check-only) check_only="$2"; shift 2 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
[[ "$jobs" =~ ^[1-8]$ ]] || { echo "--jobs must be 1..8" >&2; exit 2; }

REL=target/portable/p2p-desktop

# Build-time deps of `cargo build -p desktop` on Linux (from p2p_chat's build scripts):
#   pkg-config alsa   alsa-sys (cpal) links libasound: the only required pkg-config library
#   meson, ninja      webrtc-audio-processing-sys "bundled"; abseil comes from its meson wrap,
#                     downloaded and linked statically when pkg-config absl_base is absent
#   libclang          bindgen in webrtc-audio-processing-sys (LIBCLANG_PATH on NixOS)
#   cmake             opusic-sys "bundled" and scripts/build_libaom.py
#   nasm              rav1d (nasm-rs) and libaom
#   cc, c++, python3  cc crates, scripts/build_libaom.py and build_libvpx.py
# Not needed at build time: X11, xkbcommon, wayland, EGL and Vulkan are dlopened (x11-dl,
# tiny-xlib, x11rb dl-libxcb, wayland-sys "dlopen", xkbcommon-dl, khronos-egl/ash libloading);
# D-Bus (tray, keyring, notifications, portal) is pure Rust zbus.
# Network: the abseil wrap and the pinned libaom/libvpx tarballs are fetched on first build.
read -r -d '' PROBE <<'EOF' || true
missing=""
for c in cc c++ python3 pkg-config cmake ninja meson nasm cargo rustc patchelf readelf; do
  command -v "$c" >/dev/null || missing="$missing $c"
done
pkg-config --exists alsa || missing="$missing pkg-config:alsa(alsa-lib.dev)"
if [ -z "${LIBCLANG_PATH:-}" ]; then
  echo "preflight: note: LIBCLANG_PATH unset; the build recipe sets it from nixpkgs clang.cc.lib"
elif ! ls "$LIBCLANG_PATH"/libclang.so* >/dev/null 2>&1; then
  missing="$missing libclang(no libclang.so in $LIBCLANG_PATH)"
fi
if [ -n "$missing" ]; then echo "preflight: MISSING:$missing" >&2; exit 1; fi
echo "preflight: ok (alsa $(pkg-config --modversion alsa), meson $(meson --version), $(cmake --version | head -n1))"
EOF

# Before the module has meson/patchelf/alsa, take the same channel's packages (user-level
# nix-build, no root). libclang (bindgen) is never on a search path on NixOS.
read -r -d '' SETUP <<'EOF' || true
set -eu
jobs=$1
export NIX_PATH="${NIX_PATH:-nixpkgs=/nix/var/nix/profiles/per-user/root/channels/nixos}"
nixpath() { nix-build '<nixpkgs>' --no-out-link -A "$1"; }
command -v meson >/dev/null || PATH="$(nixpath meson)/bin:$PATH"
command -v patchelf >/dev/null || PATH="$(nixpath patchelf)/bin:$PATH"
pkg-config --exists alsa || PKG_CONFIG_PATH="$(nixpath alsa-lib.dev)/lib/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
export PATH PKG_CONFIG_PATH
export LIBCLANG_PATH="${LIBCLANG_PATH:-$(nixpath clang.cc.lib)/lib}"
EOF

read -r -d '' BUILD <<'EOF' || true
t=$(rustc -vV | sed -n 's/^host: //p' | tr - _)
aom=$(python3 scripts/build_libaom.py --target host --jobs "$jobs" | tail -n1)
# build_libvpx.py's configure flags differ between branches (decoder-only vs. encoder) but its
# install prefix doesn't, so a shared VPX_CACHE would be rebuilt in place under other jobs.
# Key a private cache by the script's content instead.
vpx_key=$(sha256sum scripts/build_libvpx.py | cut -c1-12)
vpx=$(VPX_CACHE="$HOME/build-cache/p2p_chat/libvpx-portable-$vpx_key" \
  python3 scripts/build_libvpx.py --target host --jobs "$jobs" | tail -n1)
# CMake's GNUInstallDirs installs libaom into lib64/ on NixOS; crates/video/build.rs wants lib/.
# Point it at a shim prefix in this checkout's target dir (the shared cache is left as is).
if [ ! -f "$aom/lib/libaom.a" ] && [ -f "$aom/lib64/libaom.a" ]; then
  mkdir -p target/portable/aom-prefix
  ln -sfn "$aom/include" target/portable/aom-prefix/include
  ln -sfn "$aom/lib64" target/portable/aom-prefix/lib
  aom=$PWD/target/portable/aom-prefix
fi
export "AOM_DIR_$t=$aom" "VPX_DIR_$t=$vpx" AOM_STATIC_REQUIRED=1 VPX_STATIC_REQUIRED=1
cargo build --locked --release -j "$jobs" -p desktop --target-dir target/distribution
mkdir -p target/portable
cp target/distribution/release/p2p-desktop target/portable/p2p-desktop.tmp
patchelf --set-interpreter /lib64/ld-linux-x86-64.so.2 --remove-rpath target/portable/p2p-desktop.tmp
if readelf -d target/portable/p2p-desktop.tmp | grep -E 'NEEDED.*lib(aom|vpx)|RPATH|RUNPATH'; then
  echo "portable: unexpected dynamic codec library or rpath" >&2; exit 1
fi
mv target/portable/p2p-desktop.tmp target/portable/p2p-desktop
readelf -l target/portable/p2p-desktop | grep 'program interpreter'
readelf -d target/portable/p2p-desktop | grep NEEDED
EOF

RECIPE="$SETUP"$'\n'"$PROBE"$'\n'"$BUILD"

check() {
  local bin=$1 fail=0
  echo "== portability checks: $bin"
  local interp; interp=$(readelf -l "$bin" | sed -n 's/.*Requesting program interpreter: \(.*\)]/\1/p')
  echo "interpreter: $interp"
  [[ "$interp" == /lib64/ld-linux-x86-64.so.2 ]] || { echo "FAIL: interpreter"; fail=1; }
  if readelf -d "$bin" | grep -E 'RPATH|RUNPATH|/nix/store'; then echo "FAIL: rpath or /nix/store in dynamic section"; fail=1; fi
  readelf -d "$bin" | sed -n 's/.*NEEDED.*\[\(.*\)\]/needed: \1/p'
  local l; l=$(ldd "$bin" 2>&1) || true
  echo "$l"
  if grep -E 'not found|/nix/store' <<<"$l"; then echo "FAIL: ldd"; fail=1; fi
  local host_glibc need_glibc host_cxx need_cxx
  host_glibc=$(ldd --version | sed -n '1s/.* \([0-9][0-9.]*\)$/\1/p')
  need_glibc=$(objdump -T "$bin" | grep -o 'GLIBC_[0-9][0-9.]*' | sed 's/GLIBC_//' | sort -uV | tail -n1)
  echo "glibc: needs <= $need_glibc, host has $host_glibc"
  [[ $(printf '%s\n%s\n' "$need_glibc" "$host_glibc" | sort -V | tail -n1) == "$host_glibc" ]] || { echo "FAIL: glibc"; fail=1; }
  need_cxx=$(objdump -T "$bin" | grep -o 'GLIBCXX_[0-9][0-9.]*' | sed 's/GLIBCXX_//' | sort -uV | tail -n1)
  if [[ -n "$need_cxx" ]]; then
    local cxx; cxx=$(ldd "$bin" | sed -n 's/.*libstdc++.so.6 => \([^ ]*\).*/\1/p')
    host_cxx=$(strings "$cxx" | grep -o '^GLIBCXX_[0-9][0-9.]*$' | sed 's/GLIBCXX_//' | sort -uV | tail -n1)
    echo "libstdc++: needs <= GLIBCXX_$need_cxx, host has GLIBCXX_$host_cxx"
    [[ $(printf '%s\n%s\n' "$need_cxx" "$host_cxx" | sort -V | tail -n1) == "$host_cxx" ]] || { echo "FAIL: GLIBCXX"; fail=1; }
  fi
  local out rc=0
  out=$(LD_BIND_NOW=1 "$bin" --portable-smoke-not-an-option 2>&1) || rc=$?
  echo "smoke (LD_BIND_NOW=1, unknown option): exit $rc: $out"
  [[ $rc == 1 && "$out" == *"usage: p2p-desktop"* ]] || { echo "FAIL: smoke start"; fail=1; }
  if ((fail == 0)); then echo "PORTABLE OK: $bin"; else echo "PORTABLE FAILED: $bin"; fi
  return $fail
}

if [[ -n "$check_only" ]]; then check "$check_only"; exit; fi
[[ -n "$who" ]] || { echo "--who is required" >&2; exit 2; }
[[ -f crates/desktop/Cargo.toml && -f scripts/build_libaom.py ]] || { echo "run from a p2p_chat checkout root" >&2; exit 2; }

if [[ -n "$preflight" ]]; then
  exec tasks-offload --worker "$worker" --who "$who" --ssh-key "$key" --remote-run -- sh -c "$PROBE" preflight
fi

dirty=""; git diff --quiet HEAD -- . || dirty=" (dirty)"
echo "source: $(git rev-parse HEAD)$dirty"
log=$(mktemp)
trap 'rm -f "$log"' EXIT
rc=0
tasks-offload --worker "$worker" --who "$who" --ssh-key "$key" --fetch "$REL" \
  -- sh -c "$RECIPE" portable-desktop "$jobs" | tee "$log" || rc=${PIPESTATUS[0]}
((rc == 0)) || { echo "offload failed (exit $rc)" >&2; exit "$rc"; }
dest=$(tail -n1 "$log" | python3 -c 'import json,sys; print(json.load(sys.stdin)["fetch_dest"])')
check "$dest/$REL"

#!/bin/sh
# Read-only inspector/plan entrypoint. No sourcing of profiles or tool shims.
set -eu
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
exec python3 -B "$SCRIPT_DIR/linux_worker_setup.py" "$@"

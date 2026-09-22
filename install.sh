#!/usr/bin/env bash
# Install-only entrypoint. Build separately with mise run build.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
exec bash "${SCRIPT_DIR}/scripts/install.sh" "$@"

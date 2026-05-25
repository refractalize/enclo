#!/usr/bin/env bash
set -euo pipefail

BIN_DIR="${HOME}/.local/bin"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TARGET_DIR="${SCRIPT_DIR}/../target/release"

install -Dm755 "${TARGET_DIR}/enclo-container-client" "${BIN_DIR}/enclo-container-client"
echo "installed: ${BIN_DIR}/enclo-container-client"

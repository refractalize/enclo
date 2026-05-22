#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Build all workspace members in release mode
cargo build --release --manifest-path "${SCRIPT_DIR}/Cargo.toml"

bash "${SCRIPT_DIR}/insectjar/install.sh"
bash "${SCRIPT_DIR}/insectjar-proxy/install.sh"
bash "${SCRIPT_DIR}/insectjar-proxy-store-pass/install.sh"

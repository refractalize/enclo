#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Build all workspace members in release mode
cargo build --release --manifest-path "${SCRIPT_DIR}/Cargo.toml"

bash "${SCRIPT_DIR}/enclo/install.sh"
bash "${SCRIPT_DIR}/enclo-proxy/install.sh"
bash "${SCRIPT_DIR}/enclo-secret-store-pass/install.sh"
bash "${SCRIPT_DIR}/enclo-secret-store-aws-vault/install.sh"
bash "${SCRIPT_DIR}/enclo-secrets/install.sh"
bash "${SCRIPT_DIR}/enclo-container-client/install.sh"
bash "${SCRIPT_DIR}/enclo-aws-credentials/install.sh"

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

if [[ "$(uname)" == "Darwin" ]]; then
    LINUX_BIN_DIR="${HOME}/.local/bin/linux"
    mkdir -p "${LINUX_BIN_DIR}"

    # Build Linux binaries natively inside a musl (Alpine) container.
    # rust:alpine is a musl-based image so cargo produces static binaries
    # without any cross-compilation toolchain setup.
    echo "building Linux binaries..."
    podman run --rm \
        --volume "${SCRIPT_DIR}:/workspace:z" \
        --volume "enclo-linux-build-cache:/root/.cargo/registry" \
        --volume "${LINUX_BIN_DIR}:/output:z" \
        --workdir /workspace \
        docker.io/library/rust:alpine \
        sh -c 'CARGO_TARGET_DIR=/tmp/build cargo build --release \
                   -p enclo-container-client \
                   -p enclo-aws-credentials && \
               install -Dm755 /tmp/build/release/enclo-container-client /output/ && \
               install -Dm755 /tmp/build/release/enclo-aws-credentials /output/'

    echo "installed: ${LINUX_BIN_DIR}/enclo-container-client"
    echo "installed: ${LINUX_BIN_DIR}/enclo-aws-credentials"
fi

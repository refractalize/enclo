#!/usr/bin/env bash
set -euo pipefail

PROJECT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# Keep artifacts at the path consumed by the host installer.
export CARGO_TARGET_DIR="${PROJECT_DIR}/target"
cargo build --release --workspace --manifest-path "${PROJECT_DIR}/Cargo.toml"

if [[ "$(uname)" == "Darwin" ]]; then
    echo "building enclo-secret-approve-macos..."
    swiftc -O \
        "${PROJECT_DIR}/enclo-secret-approve-macos/main.swift" \
        -framework AppKit \
        -framework LocalAuthentication \
        -o "${PROJECT_DIR}/target/release/enclo-secret-approve-macos"

    LINUX_BIN_DIR="${PROJECT_DIR}/target/release/linux"
    mkdir -p "${LINUX_BIN_DIR}"
    # Build Linux binaries natively inside a musl (Alpine) container.
    # rust:alpine is a musl-based image so cargo produces static binaries
    # without any cross-compilation toolchain setup.
    echo "building Linux binaries..."
    podman run --rm \
        --volume "${PROJECT_DIR}:/workspace:z" \
        --volume "enclo-linux-build-cache:/root/.cargo/registry" \
        --volume "${LINUX_BIN_DIR}:/output:z" \
        --workdir /workspace \
        docker.io/library/rust:alpine \
        sh -c 'CARGO_TARGET_DIR=/tmp/build cargo build --release \
                   -p enclo-container-bridge \
                   -p enclo-container-client \
                   -p enclo-aws-credentials && \
               install -Dm755 /tmp/build/release/enclo-container-bridge /output/ && \
               install -Dm755 /tmp/build/release/enclo-container-client /output/ && \
               install -Dm755 /tmp/build/release/enclo-aws-credentials /output/'

fi

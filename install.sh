#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Build all workspace members in release mode
cargo build --release --manifest-path "${SCRIPT_DIR}/Cargo.toml"

bash "${SCRIPT_DIR}/enclo/install.sh"
bash "${SCRIPT_DIR}/enclo-secret-store-pass/install.sh"
bash "${SCRIPT_DIR}/enclo-secret-store-aws-vault/install.sh"
bash "${SCRIPT_DIR}/enclo-secrets/install.sh"
bash "${SCRIPT_DIR}/enclo-container-client/install.sh"
bash "${SCRIPT_DIR}/enclo-aws-credentials/install.sh"

if [[ "$(uname)" == "Darwin" ]]; then
    bash "${SCRIPT_DIR}/enclo-secret-store-keychain/install.sh"

    echo "building enclo-secret-approve-macos..."
    swiftc -O \
        "${SCRIPT_DIR}/enclo-secret-approve-macos/main.swift" \
        -framework AppKit \
        -framework LocalAuthentication \
        -o "${HOME}/.local/bin/enclo-secret-approve-macos"
    echo "installed: ${HOME}/.local/bin/enclo-secret-approve-macos"

    # Install LaunchAgent so the daemon starts on login.
    LAUNCH_AGENTS_DIR="${HOME}/Library/LaunchAgents"
    mkdir -p "${LAUNCH_AGENTS_DIR}"
    PLIST_PATH="${LAUNCH_AGENTS_DIR}/org.enclo.plist"
    ENCLO_BIN="${HOME}/.local/bin/enclo"
    LOG_DIR="${HOME}/Library/Logs/enclo"
    mkdir -p "${LOG_DIR}"
    cat > "${PLIST_PATH}" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>org.enclo</string>
    <key>ProgramArguments</key>
    <array>
        <string>${ENCLO_BIN}</string>
        <string>serve</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>${LOG_DIR}/daemon.log</string>
    <key>StandardErrorPath</key>
    <string>${LOG_DIR}/daemon.log</string>
    <key>EnvironmentVariables</key>
    <dict>
        <key>PATH</key>
        <string>/opt/homebrew/bin:/opt/homebrew/sbin:/usr/local/bin:/usr/local/sbin:/usr/bin:/bin:/usr/sbin:/sbin:${HOME}/.local/bin</string>
    </dict>
</dict>
</plist>
PLIST
    echo "installed: ${PLIST_PATH}"

    # Load (or reload) the agent.
    if launchctl list org.enclo &>/dev/null; then
        launchctl unload "${PLIST_PATH}" 2>/dev/null || true
    fi
    launchctl load "${PLIST_PATH}"
    echo "loaded: org.enclo LaunchAgent"

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
                   -p enclo-container-bridge \
                   -p enclo-container-client \
                   -p enclo-aws-credentials && \
               install -Dm755 /tmp/build/release/enclo-container-bridge /output/ && \
               install -Dm755 /tmp/build/release/enclo-container-client /output/ && \
               install -Dm755 /tmp/build/release/enclo-aws-credentials /output/'

    echo "installed: ${LINUX_BIN_DIR}/enclo-container-bridge"
    echo "installed: ${LINUX_BIN_DIR}/enclo-container-client"
    echo "installed: ${LINUX_BIN_DIR}/enclo-aws-credentials"
fi

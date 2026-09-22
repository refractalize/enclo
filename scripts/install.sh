#!/usr/bin/env bash
set -euo pipefail

PROJECT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET_DIR="${PROJECT_DIR}/target/release"
BIN_DIR="${HOME}/.local/bin"
PLATFORM="$(uname)"

if [[ $# -ne 0 ]]; then
    echo "Usage: $0 (installs prebuilt target/release binaries)" >&2
    exit 2
fi

BINARIES=(
    enclo
    enclo-secret-store-pass
    enclo-secret-store-aws-vault
    enclo-secret-store-github
    enclo-secrets
    enclo-container-client
    enclo-aws-credentials
)
LINUX_BINARIES=()
case "$PLATFORM" in
    Linux) ;;
    Darwin)
        BINARIES+=(enclo-secret-store-keychain enclo-secret-approve-macos)
        LINUX_BINARIES+=(enclo-container-bridge enclo-container-client enclo-aws-credentials)
        ;;
    *) echo "Unsupported host platform: $PLATFORM" >&2; exit 1 ;;
esac

# Validate all artifacts before changing any installed files.
ARTIFACTS=("${BINARIES[@]}")
for binary in "${LINUX_BINARIES[@]}"; do
    ARTIFACTS+=("linux/${binary}")
done
for binary in "${ARTIFACTS[@]}"; do
    if [[ ! -x "${TARGET_DIR}/${binary}" ]]; then
        echo "Missing release binary: ${TARGET_DIR}/${binary}" >&2
        echo "Run mise run build in your build environment first." >&2
        exit 1
    fi
done

mkdir -p "${BIN_DIR}"
for binary in "${BINARIES[@]}"; do
    install -m755 "${TARGET_DIR}/${binary}" "${BIN_DIR}/${binary}"
    echo "installed: ${BIN_DIR}/${binary}"
done

if [[ "$PLATFORM" == "Linux" ]]; then
    SYSTEMD_USER_DIR="${HOME}/.config/systemd/user"
    DBUS_SERVICES_DIR="${HOME}/.local/share/dbus-1/services"
    mkdir -p "${SYSTEMD_USER_DIR}" "${DBUS_SERVICES_DIR}"
    install -m644 "${PROJECT_DIR}/systemd/enclo.service" "${SYSTEMD_USER_DIR}/enclo.service"
    install -m644 "${PROJECT_DIR}/systemd/org.enclo.service" "${DBUS_SERVICES_DIR}/org.enclo.service"
    systemctl --user daemon-reload
    systemctl --user enable enclo.service
    echo "enabled enclo.service (D-Bus activated)"
else
    mkdir -p "${BIN_DIR}/linux"
    for binary in "${LINUX_BINARIES[@]}"; do
        install -m755 "${TARGET_DIR}/linux/${binary}" "${BIN_DIR}/linux/${binary}"
        echo "installed: ${BIN_DIR}/linux/${binary}"
    done
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

fi

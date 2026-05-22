#!/usr/bin/env bash
set -euo pipefail

BIN_DIR="${HOME}/.local/bin"
SYSTEMD_USER_DIR="${HOME}/.config/systemd/user"
DBUS_SERVICES_DIR="${HOME}/.local/share/dbus-1/services"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Build release binary
cargo build --release --manifest-path "${SCRIPT_DIR}/Cargo.toml"

install -Dm755 "${SCRIPT_DIR}/target/release/insectjar" "${BIN_DIR}/insectjar"
echo "installed: ${BIN_DIR}/insectjar"

install -Dm644 "${SCRIPT_DIR}/systemd/insectjar.service" "${SYSTEMD_USER_DIR}/insectjar.service"
echo "installed: ${SYSTEMD_USER_DIR}/insectjar.service"

install -Dm644 "${SCRIPT_DIR}/systemd/org.insectjar.service" "${DBUS_SERVICES_DIR}/org.insectjar.service"
echo "installed: ${DBUS_SERVICES_DIR}/org.insectjar.service"

systemctl --user daemon-reload
systemctl --user enable insectjar.service
echo "enabled insectjar.service (D-Bus activated)"

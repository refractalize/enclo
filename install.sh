#!/usr/bin/env bash
set -euo pipefail

BIN_DIR="${HOME}/.local/bin"
SYSTEMD_USER_DIR="${HOME}/.config/systemd/user"
DBUS_SERVICES_DIR="${HOME}/.local/share/dbus-1/services"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Build release binary
cargo build --release --manifest-path "${SCRIPT_DIR}/Cargo.toml"

install -Dm755 "${SCRIPT_DIR}/target/release/safebox" "${BIN_DIR}/safebox"
echo "installed: ${BIN_DIR}/safebox"

install -Dm644 "${SCRIPT_DIR}/systemd/safebox.service" "${SYSTEMD_USER_DIR}/safebox.service"
echo "installed: ${SYSTEMD_USER_DIR}/safebox.service"

install -Dm644 "${SCRIPT_DIR}/systemd/org.safebox.service" "${DBUS_SERVICES_DIR}/org.safebox.service"
echo "installed: ${DBUS_SERVICES_DIR}/org.safebox.service"

systemctl --user daemon-reload
systemctl --user enable safebox.service
echo "enabled safebox.service (D-Bus activated)"

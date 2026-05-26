#!/usr/bin/env bash
set -euo pipefail

BIN_DIR="${HOME}/.local/bin"
SYSTEMD_USER_DIR="${HOME}/.config/systemd/user"
DBUS_SERVICES_DIR="${HOME}/.local/share/dbus-1/services"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TARGET_DIR="${SCRIPT_DIR}/../target/release"

install -Dm755 "${TARGET_DIR}/enclo" "${BIN_DIR}/enclo"
echo "installed: ${BIN_DIR}/enclo"

if [[ "$(uname)" != "Darwin" ]]; then
  install -Dm644 "${SCRIPT_DIR}/../systemd/enclo.service" "${SYSTEMD_USER_DIR}/enclo.service"
  echo "installed: ${SYSTEMD_USER_DIR}/enclo.service"

  install -Dm644 "${SCRIPT_DIR}/../systemd/org.enclo.service" "${DBUS_SERVICES_DIR}/org.enclo.service"
  echo "installed: ${DBUS_SERVICES_DIR}/org.enclo.service"

  systemctl --user daemon-reload
  systemctl --user enable enclo.service
  echo "enabled enclo.service (D-Bus activated)"
fi

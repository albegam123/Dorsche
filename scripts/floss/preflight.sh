#!/usr/bin/env bash
set -uo pipefail

root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"
state="${DORSCHE_FLOSS_STATE:-$root/.cache/floss}"
failures=0

ok() { printf '[ OK ] %s\n' "$*"; }
warn() { printf '[WARN] %s\n' "$*"; }
fail() { printf '[FAIL] %s\n' "$*"; failures=$((failures + 1)); }

[[ "$(uname -s)" == Linux ]] && ok 'Linux host' || fail 'Linux is required'
[[ "$(uname -m)" == x86_64 ]] \
  && ok 'x86_64 host supported by upstream build.py' \
  || fail 'upstream host build.py currently requires x86_64'

[[ -S /run/dbus/system_bus_socket ]] \
  && ok 'system D-Bus is available' \
  || fail 'system D-Bus socket is missing'

if [[ -f /etc/dbus-1/system.d/org.chromium.bluetooth.conf \
   || -f /usr/share/dbus-1/system.d/org.chromium.bluetooth.conf ]]; then
  ok 'Floss system-D-Bus policy is installed'
else
  fail 'Floss D-Bus policy is absent; run scripts/floss/install-runtime.sh after building'
fi

if [[ -x /usr/libexec/bluetooth/dorsche-btadapterd-launch ]]; then
  ok 'virtual/real HCI systemd launcher is installed'
else
  fail 'virtual/real HCI launcher is absent; run scripts/floss/install-runtime.sh'
fi

if compgen -G '/sys/class/bluetooth/hci*' >/dev/null; then
  for adapter in /sys/class/bluetooth/hci*; do
    ok "Bluetooth controller $(basename "$adapter")"
  done
else
  fail 'no /sys/class/bluetooth/hciN controller found'
fi

if pgrep -x bluetoothd >/dev/null; then
  fail 'BlueZ bluetoothd owns the controller; stop it before running Floss'
else
  ok 'BlueZ bluetoothd is not running'
fi

if systemctl is-enabled btmanagerd.service >/dev/null 2>&1; then
  ok 'btmanagerd is enabled for controller hotplug management'
else
  warn 'btmanagerd is not enabled; adapters will not be managed after boot'
fi

if [[ -x "$state/output/release/btadapterd" ]]; then
  ok "btadapterd built at $state/output/release/btadapterd"
else
  warn 'btadapterd has not been built yet'
fi

if getent group bluetooth-audio >/dev/null; then
  ok 'bluetooth-audio group exists'
else
  warn 'bluetooth-audio group is absent; UIPC sockets will retain daemon group ownership'
fi

if (( failures > 0 )); then
  printf '\n%d blocking preflight issue(s).\n' "$failures" >&2
  exit 1
fi

printf '\nFloss runtime preflight passed.\n'

#!/usr/bin/env bash
set -euo pipefail

root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"
state="${DORSCHE_FLOSS_STATE:-$root/.cache/floss}"
hci="${1:-0}"
binary="${DORSCHE_BTADAPTERD:-$state/output/release/btadapterd}"
quirk_resolver="${DORSCHE_QUIRK_RESOLVER:-$root/target/release/dorsche_controller_quirks}"

[[ "$hci" =~ ^[0-9]+$ ]] || {
  printf 'HCI index must be numeric.\n' >&2
  exit 2
}
[[ -x "$binary" ]] || {
  printf 'btadapterd not found: %s\n' "$binary" >&2
  exit 2
}
[[ -d "/sys/class/bluetooth/hci$hci" ]] || {
  printf 'Bluetooth controller hci%s does not exist.\n' "$hci" >&2
  exit 2
}
if pgrep -x bluetoothd >/dev/null; then
  printf 'bluetoothd is running and owns hci%s. Stop bluetooth.service first.\n' "$hci" >&2
  exit 3
fi

if [[ -x "$quirk_resolver" ]]; then
  quirk_args=(--hci "$hci")
  if [[ "${DORSCHE_AUTO_CONTROLLER_QUIRKS:-0}" == 1 ]]; then
    quirk_args+=(--apply)
    if [[ $EUID -eq 0 ]]; then
      "$quirk_resolver" "${quirk_args[@]}"
    else
      sudo "$quirk_resolver" "${quirk_args[@]}"
    fi
  else
    "$quirk_resolver" "${quirk_args[@]}"
  fi
else
  printf 'Controller quirk resolver not built; continuing without detection.\n' >&2
fi

mkdir_audio=(install -d -m 0770 /var/run/bluetooth/audio)
if [[ $EUID -eq 0 ]]; then
  "${mkdir_audio[@]}"
else
  sudo "${mkdir_audio[@]}"
fi

# This upstream revision selects its Linux HCI implementation at build time;
# Android-style INIT_* positional flags are not part of btadapterd's CLI.
# Optional daemon switches such as `--debug --log-output=stderr` may be supplied
# as a whitespace-separated development override.
read -r -a extra_args <<< "${DORSCHE_BTADAPTERD_ARGS:-}"
exec "$binary" --hci="$hci" "${extra_args[@]}"

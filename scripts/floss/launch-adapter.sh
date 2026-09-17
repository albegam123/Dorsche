#!/usr/bin/env bash
set -euo pipefail

# btmanagerd deliberately separates the stable D-Bus adapter identity from the
# kernel's recyclable hciN number. Its SystemdInvoker encodes both as
# btadapterd@<virtual>_<real>.service. Keep that topology intact at the final
# exec boundary; collapsing the pair would bind Floss to a transient hciN.
instance="${1:-}"
binary="${DORSCHE_BTADAPTERD:-/usr/libexec/bluetooth/btadapterd}"
loader="${DORSCHE_DYNAMIC_LOADER:-}"
library_path="${DORSCHE_LIBRARY_PATH:-}"

if [[ ! "$instance" =~ ^([0-9]+)_([0-9]+)$ ]]; then
  printf 'Expected systemd instance <virtual_hci>_<real_hci>, got: %q\n' "$instance" >&2
  exit 2
fi

virtual_hci="${BASH_REMATCH[1]}"
real_hci="${BASH_REMATCH[2]}"

[[ -x "$binary" ]] || {
  printf 'btadapterd is not executable: %s\n' "$binary" >&2
  exit 2
}

# Never inspect VID:PID here. btmanagerd owns adapter identity and btusb owns
# hardware transport policy; this launcher only preserves their two indices.
if [[ -n "$loader" || -n "$library_path" ]]; then
  [[ -n "$loader" && -n "$library_path" && -x "$loader" && -d "$library_path" ]] || {
    printf 'DORSCHE_DYNAMIC_LOADER and DORSCHE_LIBRARY_PATH must form a valid pair.\n' >&2
    exit 2
  }
  exec "$loader" --library-path "$library_path" "$binary" \
    --index="$virtual_hci" --hci="$real_hci"
fi

exec "$binary" --index="$virtual_hci" --hci="$real_hci"

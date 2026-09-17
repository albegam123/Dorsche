#!/usr/bin/env bash
set -uo pipefail

address=""
adapter=0
runtime="${DORSCHE_RUNTIME_DIR:-}"
wait_seconds=25
event_seconds=10

usage() {
  cat <<'EOF'
Usage: sudo scripts/floss/hid-smoke.sh --address XX:XX:XX:XX:XX:XX [options]

Validate a bonded BLE HID/HOGP peripheral through the unmodified Floss D-Bus
API, Floss HID Host, UHID, and the Linux input subsystem.

Options:
  --adapter N          Floss virtual D-Bus adapter index (default: 0)
  --runtime DIR        Relocatable runtime containing bin/ and lib/
  --wait-seconds N     Timeout for each asynchronous stage (default: 25)
  --event-seconds N    Interactive mouse-event capture time (default: 10)
  --no-events          Do not read the generated /dev/input/eventN node

The device must already be bonded. For a new Logitech Easy-Switch slot, hold
the slot button until it flashes rapidly, scan, and bond its current address.
EOF
}

while (($#)); do
  case "$1" in
    --address) address="${2:-}"; shift 2 ;;
    --adapter) adapter="${2:-}"; shift 2 ;;
    --runtime) runtime="${2:-}"; shift 2 ;;
    --wait-seconds) wait_seconds="${2:-}"; shift 2 ;;
    --event-seconds) event_seconds="${2:-}"; shift 2 ;;
    --no-events) event_seconds=0; shift ;;
    -h|--help) usage; exit 0 ;;
    *) printf 'Unknown argument: %s\n' "$1" >&2; usage >&2; exit 2 ;;
  esac
done

[[ "$address" =~ ^([[:xdigit:]]{2}:){5}[[:xdigit:]]{2}$ ]] || {
  printf 'A valid --address is required.\n' >&2
  exit 2
}
for value in "$adapter" "$wait_seconds" "$event_seconds"; do
  [[ "$value" =~ ^[0-9]+$ ]] || {
    printf 'Numeric option expected, got: %s\n' "$value" >&2
    exit 2
  }
done
((wait_seconds > 0)) || { printf -- '--wait-seconds must be positive.\n' >&2; exit 2; }

if [[ -n "$runtime" ]]; then
  loader="$runtime/lib/ld-linux-x86-64.so.2"
  libraries="$runtime/lib"
  client="$runtime/bin/btclient"
  [[ -x "$loader" && -x "$client" && -d "$libraries" ]] || {
    printf 'Invalid relocatable runtime: %s\n' "$runtime" >&2
    exit 2
  }
  run_client() { "$loader" --library-path "$libraries" "$client" "$@"; }
else
  client="${DORSCHE_BTCLIENT:-$(command -v btclient || true)}"
  [[ -x "$client" ]] || {
    printf 'btclient not found; pass --runtime DIR or set DORSCHE_BTCLIENT.\n' >&2
    exit 2
  }
  run_client() { "$client" "$@"; }
fi

command -v busctl >/dev/null || { printf 'busctl is required.\n' >&2; exit 2; }
adapter_path="/org/chromium/bluetooth/hci$adapter/adapter"
interface="org.chromium.bluetooth.Bluetooth"

info="$(run_client --command "device info $address" 2>&1)"
printf '%s\n' "$info"
grep -Fq 'Bond State: Bonded' <<< "$info" || {
  printf '[FAIL] Device is not bonded. Put it in pairing mode and run bond add first.\n' >&2
  exit 1
}

# A slow HOGP peripheral may drop the first post-SMP service-discovery link.
# FetchRemoteUuids is the official Floss retry path: it reuses the stack GATT
# cache/state and avoids duplicating GATT discovery in this test harness.
busctl --system call org.chromium.bluetooth "$adapter_path" "$interface" \
  FetchRemoteUuids 'a{sv}' 2 \
  address s "$address" name s 'Dorsche HID smoke' >/dev/null

hogp=0
for ((attempt = 0; attempt < wait_seconds; attempt++)); do
  info="$(run_client --command "device info $address" 2>&1)"
  if grep -Fq ': Hogp' <<< "$info"; then
    hogp=1
    break
  fi
  sleep 1
done
((hogp)) || {
  printf '[FAIL] Floss did not publish HOGP UUID 0x1812 within %d seconds.\n' \
    "$wait_seconds" >&2
  exit 1
}
printf '[ OK ] Floss discovered HOGP UUID 0x1812\n'

# Connect only after UUID publication. ConnectAllEnabledProfiles selects the
# in-tree Floss HID Host from device capabilities; the harness never opens ATT
# itself and never substitutes a userspace HID implementation.
run_client --command "device connect $address"

address_lower="${address,,}"
event=""
for ((attempt = 0; attempt < wait_seconds; attempt++)); do
  event="$(awk -v address="$address_lower" '
    BEGIN { RS="" }
    tolower($0) ~ "uniq=" address && $0 ~ /Name=.*Mouse/ {
      if (match($0, /event[0-9]+/)) {
        print "/dev/input/" substr($0, RSTART, RLENGTH)
        exit
      }
    }
  ' /proc/bus/input/devices)"
  [[ -n "$event" && -c "$event" ]] && break
  sleep 1
done
[[ -n "$event" && -c "$event" ]] || {
  printf '[FAIL] Floss HID Host did not create a mouse input node.\n' >&2
  exit 1
}

printf '[ OK ] HOGP -> UHID -> Linux input: %s\n' "$event"
grep -A8 -B2 -i "Uniq=$address" /proc/bus/input/devices || true

if ((event_seconds == 0)); then
  exit 0
fi
[[ $EUID -eq 0 ]] || {
  printf '[WARN] Run as root to capture input events from %s.\n' "$event"
  exit 0
}

printf '\nMove the mouse, click both buttons, and scroll for %d seconds now.\n' \
  "$event_seconds"
# input_event is 24 bytes on the supported x86_64 host. Counting complete
# records proves that events traversed HOGP, UHID, and evdev without flooding
# the terminal with high-rate REL_X/REL_Y reports.
bytes="$(timeout "$event_seconds" dd if="$event" bs=24 status=none 2>/dev/null | wc -c)"
records=$((bytes / 24))
((records > 0)) || {
  printf '[FAIL] No evdev records received during the interactive window.\n' >&2
  exit 1
}
printf '[PASS] Received %d complete evdev records from %s.\n' "$records" "$event"

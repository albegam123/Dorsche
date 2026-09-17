#!/usr/bin/env bash
set -uo pipefail

root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"
address=""
hci=0
dbus_adapter=0
cycles=1
seconds=5
delay_seconds=10
volume=50
max_msbc_loss_percent=5
runtime="${DORSCHE_RUNTIME_DIR:-}"
output=""
unit="${DORSCHE_BTADAPTERD_UNIT:-dorsche-btadapterd.service}"
run_delayed=1

usage() {
  cat <<'EOF'
Usage: sudo scripts/floss/classic-audio-diag.sh --address XX:XX:XX:XX:XX:XX [options]

Options:
  --hci N              Physical Linux HCI index used for USB/controller evidence
  --dbus-adapter N     Floss D-Bus object index (default: 0)
  --cycles N           A2DP/CVSD/mSBC cycles (default: 1)
  --seconds N          Stream duration per case (default: 5)
  --delay-seconds N    Delayed-loopback history (default: 10)
  --volume N           A2DP volume in 0..127 (default: 50)
  --max-msbc-loss-percent N  Fail above this mSBC loss percentage (default: 5)
  --runtime DIR        Relocatable runtime containing bin/ and lib/
  --output DIR         Evidence directory (default: out/classic-audio-TIMESTAMP)
  --unit NAME          btadapterd journal unit
  --no-delayed         Skip the delayed full-duplex case

Environment overrides:
  DORSCHE_A2DP_SMOKE, DORSCHE_HFP_SMOKE
EOF
}

while (($#)); do
  case "$1" in
    --address) address="${2:-}"; shift 2 ;;
    --hci) hci="${2:-}"; shift 2 ;;
    --dbus-adapter) dbus_adapter="${2:-}"; shift 2 ;;
    --cycles) cycles="${2:-}"; shift 2 ;;
    --seconds) seconds="${2:-}"; shift 2 ;;
    --delay-seconds) delay_seconds="${2:-}"; shift 2 ;;
    --volume) volume="${2:-}"; shift 2 ;;
    --max-msbc-loss-percent) max_msbc_loss_percent="${2:-}"; shift 2 ;;
    --runtime) runtime="${2:-}"; shift 2 ;;
    --output) output="${2:-}"; shift 2 ;;
    --unit) unit="${2:-}"; shift 2 ;;
    --no-delayed) run_delayed=0; shift ;;
    -h|--help) usage; exit 0 ;;
    *) printf 'Unknown argument: %s\n' "$1" >&2; usage >&2; exit 2 ;;
  esac
done
[[ "$max_msbc_loss_percent" =~ ^[0-9]+([.][0-9]+)?$ ]] || {
  printf 'Invalid --max-msbc-loss-percent: %s\n' "$max_msbc_loss_percent" >&2
  exit 2
}

[[ $EUID -eq 0 ]] || {
  printf 'Run this diagnostic as root so UIPC sockets and journals are accessible.\n' >&2
  exit 2
}
[[ "$address" =~ ^([[:xdigit:]]{2}:){5}[[:xdigit:]]{2}$ ]] || {
  printf 'A valid --address is required.\n' >&2
  exit 2
}
for value in "$hci" "$dbus_adapter" "$cycles" "$seconds" "$delay_seconds" "$volume"; do
  [[ "$value" =~ ^[0-9]+$ ]] || { printf 'Numeric option expected, got: %s\n' "$value" >&2; exit 2; }
done
((cycles > 0 && seconds > 0 && delay_seconds <= 60 && volume <= 127)) || {
  printf 'Require cycles/seconds > 0, delay <= 60, and volume <= 127.\n' >&2
  exit 2
}

timestamp="$(date -u +%Y%m%dT%H%M%SZ)"
output="${output:-$root/out/classic-audio-$timestamp}"
mkdir -p "$output"

if [[ -n "$runtime" ]]; then
  loader="$runtime/lib/ld-linux-x86-64.so.2"
  libraries="$runtime/lib"
  a2dp="${DORSCHE_A2DP_SMOKE:-$runtime/bin/floss_audio_smoke}"
  if [[ -n "${DORSCHE_HFP_SMOKE:-}" ]]; then
    hfp="$DORSCHE_HFP_SMOKE"
  elif [[ -x "$runtime/bin/floss_hfp_smoke.capture" ]]; then
    hfp="$runtime/bin/floss_hfp_smoke.capture"
  else
    hfp="$runtime/bin/floss_hfp_smoke"
  fi
  [[ -x "$loader" && -d "$libraries" ]] || {
    printf 'Invalid relocatable runtime: %s\n' "$runtime" >&2
    exit 2
  }
else
  loader=""
  libraries=""
  if [[ -x /usr/libexec/bluetooth/floss_audio_smoke ]]; then
    default_a2dp=/usr/libexec/bluetooth/floss_audio_smoke
    default_hfp=/usr/libexec/bluetooth/floss_hfp_smoke
  else
    default_a2dp="$root/target/release/floss_audio_smoke"
    default_hfp="$root/target/release/floss_hfp_smoke"
  fi
  a2dp="${DORSCHE_A2DP_SMOKE:-$default_a2dp}"
  hfp="${DORSCHE_HFP_SMOKE:-$default_hfp}"
fi
[[ -x "$a2dp" && -x "$hfp" ]] || {
  printf 'Missing smoke binary: a2dp=%s hfp=%s\n' "$a2dp" "$hfp" >&2
  exit 2
}

failures=0
passes=0
collect_runtime_evidence() {
  journalctl -u "$unit" --since "@$since_epoch" --no-pager \
    > "$output/btadapterd-journal.log" 2>&1 || true
  grep 'Stopped SCO codec:' "$output/btadapterd-journal.log" \
    > "$output/sco-packet-loss.txt" 2>/dev/null || true
  awk '
    /NO_DATA_RECEIVED/ { no_data++ }
    /PARTIALLY_LOST/ { partial++ }
    /POSSIBLY_INCOMPLETE/ { incomplete++ }
    END {
      printf "NO_DATA_RECEIVED=%d\nPARTIALLY_LOST=%d\nPOSSIBLY_INCOMPLETE=%d\n",
             no_data + 0, partial + 0, incomplete + 0
    }
  ' "$output/btadapterd-journal.log" > "$output/sco-status-counts.txt"
}

run_case() {
  local label="$1"
  shift
  printf '\n[%s] START %s\n' "$(date -u +%FT%TZ)" "$label" | tee -a "$output/summary.log"
  if "$@" 2>&1 | tee "$output/$label.log"; then
    printf '[PASS] %s\n' "$label" | tee -a "$output/summary.log"
    passes=$((passes + 1))
  else
    local status=$?
    printf '[FAIL] %s status=%d\n' "$label" "$status" | tee -a "$output/summary.log"
    failures=$((failures + 1))
  fi
}

run_audio_case() {
  local label="$1"
  local binary="$2"
  shift 2
  if [[ -n "$loader" ]]; then
    run_case "$label" "$loader" --library-path "$libraries" "$binary" "$@"
  else
    run_case "$label" "$binary" "$@"
  fi
}

since_epoch="$(date +%s)"
{
  printf 'started_utc=%s\n' "$timestamp"
  printf 'dorsche_commit=%s\n' "$(git -C "$root" rev-parse HEAD 2>/dev/null || printf unknown)"
  printf 'kernel=%s\n' "$(uname -srvm)"
  printf 'address=%s\nphysical_hci=%s\ndbus_adapter=%s\n' "$address" "$hci" "$dbus_adapter"
  printf 'cycles=%s\nseconds=%s\ndelay_seconds=%s\n' "$cycles" "$seconds" "$delay_seconds"
  printf 'max_msbc_loss_percent=%s\n' "$max_msbc_loss_percent"
  printf 'a2dp_binary=%s\nhfp_binary=%s\n' "$a2dp" "$hfp"
  readlink -f "/sys/class/bluetooth/hci$hci/device" 2>/dev/null | sed 's/^/hci_sysfs=/'
} > "$output/metadata.txt"

device_path="$(readlink -f "/sys/class/bluetooth/hci$hci/device" 2>/dev/null || true)"
if [[ -n "$device_path" ]] && command -v udevadm >/dev/null; then
  udevadm info --query=property --path="$device_path" > "$output/controller.txt" 2>&1 || true
fi
if [[ -r /var/lib/bluetooth/sysprops.conf ]]; then
  cp /var/lib/bluetooth/sysprops.conf "$output/sysprops.conf"
fi
lsusb -t > "$output/lsusb-tree.txt" 2>&1 || true
lsusb > "$output/lsusb.txt" 2>&1 || true

media_path="/org/chromium/bluetooth/hci$dbus_adapter/media"
adapter_path="/org/chromium/bluetooth/hci$dbus_adapter/adapter"
connected=0
: > "$output/connect.log"
for attempt in {1..3}; do
  printf 'attempt=%d time=%s\n' "$attempt" "$(date -u +%FT%TZ)" \
    >> "$output/connect.log"
  if ! busctl --system call org.chromium.bluetooth "$media_path" \
      org.chromium.bluetooth.BluetoothMedia Connect s "$address" \
      >> "$output/connect.log" 2>&1; then
    sleep 1
    continue
  fi
  for _ in {1..10}; do
    busctl --system call org.chromium.bluetooth "$adapter_path" \
      org.chromium.bluetooth.Bluetooth GetConnectedDevices \
      > "$output/connected-devices.txt" 2>&1 || true
    if grep -Fqi "$address" "$output/connected-devices.txt"; then
      connected=1
      break
    fi
    sleep 1
  done
  if ((connected)); then
    break
  fi
done
if ((connected == 0)); then
  printf '[FAIL] headset did not connect after three page attempts\n' \
    | tee -a "$output/summary.log"
  failures=$((failures + 1))
  collect_runtime_evidence
  printf '\npasses=%d failures=%d evidence=%s\n' "$passes" "$failures" "$output" \
    | tee -a "$output/summary.log"
  exit 1
fi
# ACL presence precedes completion of the queued A2DP/HFP profile handshakes.
sleep 2

for ((cycle = 1; cycle <= cycles; cycle++)); do
  prefix="$(printf 'cycle-%03d' "$cycle")"
  run_audio_case "$prefix-a2dp-sbc" "$a2dp" \
    --address "$address" --seconds "$seconds" --volume "$volume" --frequency 440
  run_audio_case "$prefix-hfp-cvsd" "$hfp" \
    --address "$address" --seconds "$seconds" --codec cvsd --frequency 440 \
    --capture "$output/$prefix-cvsd.raw"
  run_audio_case "$prefix-hfp-msbc" "$hfp" \
    --address "$address" --seconds "$seconds" --codec msbc --frequency 440 \
    --capture "$output/$prefix-msbc.raw"
  if ((run_delayed)); then
    run_audio_case "$prefix-hfp-msbc-delay" "$hfp" \
      --address "$address" --seconds "$seconds" --codec msbc --loopback \
      --loopback-delay-seconds "$delay_seconds" --loopback-gain 0.35 \
      --capture "$output/$prefix-msbc-delay.raw"
  fi
done

collect_runtime_evidence
loss_violations="$(sed -n \
  's/.*Stopped SCO codec:MSBC.*packet_loss_ratio:\([0-9.]*\).*/\1/p' \
  "$output/btadapterd-journal.log" \
  | awk -v maximum="$max_msbc_loss_percent" '$1 * 100 > maximum { count++ } END { print count + 0 }')"
if ((loss_violations > 0)); then
  printf '[FAIL] %d mSBC run(s) exceeded %s%% packet loss\n' \
    "$loss_violations" "$max_msbc_loss_percent" | tee -a "$output/summary.log"
  failures=$((failures + loss_violations))
else
  printf '[PASS] all mSBC runs at or below %s%% packet loss\n' \
    "$max_msbc_loss_percent" | tee -a "$output/summary.log"
fi

printf '\npasses=%d failures=%d evidence=%s\n' "$passes" "$failures" "$output" \
  | tee -a "$output/summary.log"
((failures == 0))

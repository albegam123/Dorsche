#!/usr/bin/env bash
set -euo pipefail

root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"
state="${DORSCHE_FLOSS_STATE:-$root/.cache/floss}"
package="$root/third_party/floss/system/build/dpkg/floss/package"
audio_smoke="$root/target/release/floss_audio_smoke"
hfp_smoke="$root/target/release/floss_hfp_smoke"

if [[ $EUID -ne 0 ]]; then
  printf 'Runtime installation changes system D-Bus/systemd state; run explicitly with sudo.\n' >&2
  exit 2
fi

for binary in btadapterd btmanagerd btclient; do
  [[ -x "$state/output/release/$binary" ]] || {
    printf 'Missing build artifact: %s/output/release/%s\n' "$state" "$binary" >&2
    exit 2
  }
done
for binary in "$audio_smoke" "$hfp_smoke"; do
  [[ -x "$binary" ]] || {
    printf 'Missing Dorsche smoke binary: %s\n' "$binary" >&2
    printf 'Build them with: cargo build --release --bin floss_audio_smoke --bin floss_hfp_smoke\n' >&2
    exit 2
  }
done

getent group bluetooth >/dev/null || groupadd --system bluetooth
getent group bluetooth-audio >/dev/null || groupadd --system bluetooth-audio

install -D -m 0755 "$state/output/release/btadapterd" \
  /usr/libexec/bluetooth/btadapterd
install -D -m 0755 "$state/output/release/btmanagerd" \
  /usr/libexec/bluetooth/btmanagerd
install -D -m 0755 "$state/output/release/btclient" \
  /usr/local/bin/btclient
install -D -m 0755 "$audio_smoke" \
  /usr/libexec/bluetooth/floss_audio_smoke
install -D -m 0755 "$hfp_smoke" \
  /usr/libexec/bluetooth/floss_hfp_smoke
install -D -m 0755 "$root/scripts/floss/classic-audio-diag.sh" \
  /usr/local/sbin/dorsche-classic-audio-diag
install -D -m 0755 "$root/scripts/floss/launch-adapter.sh" \
  /usr/libexec/bluetooth/dorsche-btadapterd-launch
install -D -m 0644 \
  "$package/etc/dbus-1/system.d/org.chromium.bluetooth.conf" \
  /etc/dbus-1/system.d/org.chromium.bluetooth.conf
install -D -m 0644 "$package/lib/systemd/system/btmanagerd.service" \
  /usr/lib/systemd/system/btmanagerd.service
install -D -m 0644 "$package/lib/systemd/system/btadapterd@.service" \
  /usr/lib/systemd/system/btadapterd@.service
install -D -m 0644 "$root/config/floss/systemd/10-dorsche-audio.conf" \
  /etc/systemd/system/btadapterd@.service.d/10-dorsche-audio.conf
install -d -m 0750 -g bluetooth /var/lib/bluetooth /var/log/bluetooth
if [[ ! -e /var/lib/bluetooth/sysprops.conf ]]; then
  install -m 0640 -g bluetooth "$root/config/floss/sysprops.conf" \
    /var/lib/bluetooth/sysprops.conf
fi
install -d -m 0770 -g bluetooth-audio /var/run/bluetooth/audio

systemctl daemon-reload
systemctl reload dbus.service

printf 'Installed Floss runtime plus Dorsche audio diagnostics. Services were not enabled or started.\n'

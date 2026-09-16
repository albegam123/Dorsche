#!/usr/bin/env bash
set -euo pipefail

root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"
state="${DORSCHE_FLOSS_STATE:-$root/.cache/floss}"
package="$root/third_party/floss/system/build/dpkg/floss/package"

if [[ $EUID -ne 0 ]]; then
  printf 'Runtime installation changes system D-Bus/systemd state; run explicitly with sudo.\n' >&2
  exit 2
fi

for binary in btadapterd btmanagerd btclient; do
  [[ -x "$state/output/debug/$binary" ]] || {
    printf 'Missing build artifact: %s/output/debug/%s\n' "$state" "$binary" >&2
    exit 2
  }
done

getent group bluetooth >/dev/null || groupadd --system bluetooth
getent group bluetooth-audio >/dev/null || groupadd --system bluetooth-audio

install -D -m 0755 "$state/output/debug/btadapterd" \
  /usr/libexec/bluetooth/btadapterd
install -D -m 0755 "$state/output/debug/btmanagerd" \
  /usr/libexec/bluetooth/btmanagerd
install -D -m 0755 "$state/output/debug/btclient" \
  /usr/local/bin/btclient
install -D -m 0644 \
  "$package/etc/dbus-1/system.d/org.chromium.bluetooth.conf" \
  /etc/dbus-1/system.d/org.chromium.bluetooth.conf
install -D -m 0644 "$package/lib/systemd/system/btmanagerd.service" \
  /usr/lib/systemd/system/btmanagerd.service
install -D -m 0644 "$package/lib/systemd/system/btadapterd@.service" \
  /usr/lib/systemd/system/btadapterd@.service
install -d -m 0770 -g bluetooth-audio /var/run/bluetooth/audio

systemctl daemon-reload
systemctl reload dbus.service

printf 'Installed pristine Floss runtime files. Services were not enabled or started.\n'

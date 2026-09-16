# Dorsche Floss port

## What is actually ported

`third_party/floss` is a pristine snapshot of AOSP
`platform/packages/modules/Bluetooth` at commit
`ab906156f47eaead385190502647fe508229dfbf`. It contains the real Fluoride/GD
implementation: HCI, ACL, L2CAP, ATT/GATT, SMP, SDP, RFCOMM, A2DP, AVRCP, HFP,
LE Audio, native C++ `libbluetooth`, Rust `bt_topshim`, the Rust Floss stack,
`btadapterd`, `btmanagerd`, and `btclient`.

This is not a reimplementation and the vendored tree carries no Dorsche patch.
Its source identity is recorded in `vendor/floss.lock` and checked by
`scripts/floss/verify-source.sh`.

## Minimal-change boundary

Dorsche does not open an HCI socket. `btadapterd` exclusively owns `hciN` and
exports the upstream system-D-Bus API:

- service `org.chromium.bluetooth`;
- adapter `/org/chromium/bluetooth/hciN/adapter`;
- media `/org/chromium/bluetooth/hciN/media`;
- GATT `/org/chromium/bluetooth/hciN/gatt`.

The media API already has `StartAudioRequest`, `StartScoCall`, presentation
position, codec negotiation, and LE Audio host/peer stream controls. Classic
audio payloads use Floss's existing UIPC sockets:

- A2DP source PCM: `/var/run/bluetooth/audio/.a2dp_data`;
- bidirectional HFP PCM: `/var/run/bluetooth/audio/.sco_data`.

Dorsche must connect at these D-Bus/UIPC boundaries. It must not fork HCI,
L2CAP, profile state machines, pairing, or device storage. DMA-BUF is retained
inside the local capture/AI graph; the existing Floss UIPC ABI is a byte stream,
so crossing into A2DP/HFP requires the negotiated PCM representation. Changing
that ABI to SCM_RIGHTS/DMA-BUF would be an optional upstreamable optimization,
not a prerequisite for the first working Linux port.

## Build on Debian/Ubuntu

The upstream build supports Debian Bullseye or newer and Ubuntu 20.10 or newer,
on x86_64. Start with:

```bash
scripts/floss/verify-source.sh
scripts/floss/build.sh bootstrap
```

The bootstrap command stages the exact platform2/common-mk and Rust/protobuf
dependencies used by AOSP and prints missing packages. It never invokes sudo.
Install the reported dependencies and the two upstream-only packages,
`libchrome` and `modp_b64`, following
`third_party/floss/system/build/dpkg/README.txt`, then run:

```bash
scripts/floss/build.sh build
scripts/floss/build.sh test
sudo scripts/floss/install-runtime.sh
```

Artifacts are isolated under `.cache/floss/output`; source files remain clean.
The build produces `btadapterd`, `btmanagerd`, and `btclient`. The explicit
install step copies those binaries plus AOSP's own D-Bus policy and systemd
units; it does not enable or start services.

## Run on a standard Linux host

Use a dedicated USB Bluetooth controller during bring-up. The same controller
cannot be owned by BlueZ and Floss simultaneously.

```bash
scripts/floss/preflight.sh
sudo systemctl stop bluetooth.service
sudo scripts/floss/run-adapter.sh 0
```

In another terminal, verify the real daemon rather than just socket creation:

```bash
busctl --system status org.chromium.bluetooth
busctl --system introspect \
  org.chromium.bluetooth /org/chromium/bluetooth/hci0/adapter
```

Then use the upstream `.cache/floss/output/debug/btclient` to enable discovery,
pair a device, and exercise GATT/A2DP/HFP. Production installation should use
the unmodified D-Bus policy and systemd units under
`third_party/floss/system/build/dpkg/floss/package`.

## Current host limitation

The development container used to prepare this import has no `hciN`, no
Docker/Podman, and no passwordless sudo. Dorsche's Rust/C++ tests run here, but
an honest hardware pass cannot be claimed until `preflight.sh` succeeds on the
target standard-Linux machine.

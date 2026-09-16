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

The host tool versions used for the verified Ubuntu 24.04 build were
`cxxbridge-cmd 1.0.94`, `pdl-compiler 0.1.1`, `grpcio-compiler 0.13.0`,
AOSP `aconfig` from build revision `8f9ca807`, and `sysprop_cpp` from
`platform-tools-34.0.0`. `libchrome` reported `BASE_VER=1094370`; `modp_b64`
came from Chromium `110.0.5481.77`. These are build-host dependencies, not
forked Bluetooth sources.

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

Then use the upstream `.cache/floss/output/release/btclient` to enable discovery,
pair a device, and exercise GATT/A2DP/HFP. Production installation should use
the unmodified D-Bus policy and systemd units under
`third_party/floss/system/build/dpkg/floss/package`.

## Host compatibility overlay

`scripts/floss/build.sh` makes an out-of-tree copy under
`.cache/floss/staging/bt` and applies the narrowly scoped patch recorded in
`scripts/floss/patches/`. The overlay pins the Rust `cxx` crate to the same
1.0.94 ABI as AOSP's `cxxbridge` generator and enables two dependency features
normally supplied by ChromiumOS's vendored Rust graph. The imported
`third_party/floss` tree is never patched and remains verifiable byte-for-byte.

The wrapper also locates Ubuntu's versioned `libclang` for bindgen and suppresses
only Clang 18's newly split `vla-cxx-extension` diagnostic. All other upstream
`-Werror` checks remain active.

At install time, Dorsche creates the runtime state/log directories and, when no
administrator config exists, installs `config/floss/sysprops.conf`. It disables
only the Android-specific `LE_GET_VENDOR_CAPABILITIES` probe; standard USB HCI
controllers are not required to implement that vendor opcode. Existing host
configuration is never overwritten.

The installer also adds a systemd drop-in making `bluetooth-audio` a
supplementary group of `btadapterd`. This lets the daemon assign its A2DP/SCO
UIPC sockets to the intended group without broadening its upstream capability
bounding set. Rust audio clients use Tokio-native `zbus` for control methods and
SCM_RIGHTS FD transfer; no C-style D-Bus glue is needed.

## Linux audio-server direction

The production graph target is PipeWire with Rust-native PipeWire/libspa
bindings and negotiated SPA buffers. `zbus` owns the Floss control plane;
PipeWire owns graph scheduling, devices, clock domains, and buffer negotiation;
Floss UIPC remains the Classic Bluetooth transport boundary. TinyALSA may be
used only as a low-level ALSA diagnostic during hardware bring-up. It is not the
application graph, and GStreamer is not part of the architecture.

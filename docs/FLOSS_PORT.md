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

## Minimal host overlay

`scripts/floss/build.sh` makes an out-of-tree copy under
`.cache/floss/staging/bt` and applies the narrowly scoped patches recorded in
`scripts/floss/patches/`. The first patch pins the Rust `cxx` crate to the same
1.0.94 ABI as AOSP's `cxxbridge` generator and enables two dependency features
normally supplied by ChromiumOS's vendored Rust graph. The second patch exposes
Linux 6.18's upstream `HCI_DRV_PKT` transport through the existing host HCI user
socket. On HFP SCO connect/disconnect it can send btusb's switch-altsetting
driver command (opcode `0x0401`) instead of relying on the ChromeOS-private
management opcode. The imported `third_party/floss` tree is never patched and
remains verifiable byte-for-byte.

The HCI driver command path defaults off. Enable it only on Linux 6.18 or newer
after confirming the controller uses upstream `HCI_DRV_PKT`:

```ini
bluetooth.hfp.linux_hci_driver_altsetting.enabled=true
```

That gate preserves the behavior of older kernels. CVSD selects USB
altsetting 2. Transparent mSBC defaults to altsetting 1, while
`bluetooth.hfp.linux_hci_driver_msbc_altsetting` selects a validated value from
1 through 6 for controller-specific USB bandwidth layouts. The corresponding
`bluetooth.hfp.linux_hci_driver_msbc_packet_size` must match the HCI SCO packet
size exposed by that layout. Disconnect restores altsetting 0. The command must
be sent by Floss because its
`HCI_CHANNEL_USER` socket exclusively owns the controller; a sidecar process
or external kernel module would violate that ownership model.

Linux 6.18 marks Realtek btusb devices with `BTUSB_USE_ALT3_FOR_WBS`. The tested
RTL8761BU `2b89:8761` therefore uses mSBC altsetting 3 and 72-byte packets. The
generic default remains altsetting 1 so this host overlay does not silently
change other controllers.

Production startup runs `dorsche-controller-quirks` as a narrowly privileged
systemd `ExecStartPre`. The resolver walks from `/sys/class/bluetooth/hciN/device`
to the owning USB device, selects a reviewed VID:PID entry, and atomically
updates the altsetting/packet-size pair before Floss reads sysprops. Unknown and
non-USB transports are left untouched and do not block startup. The resolver
never enables the Linux HCI driver-command gate by itself.

The wrapper also locates Ubuntu's versioned `libclang` for bindgen and suppresses
only Clang 18's newly split `vla-cxx-extension` diagnostic. All other upstream
`-Werror` checks remain active.

Linux 6.18.15 has a separate btusb teardown race: SCO transmit URBs can outlive
the endpoint alternate setting they were built against. The independently
reviewable fix is kept in `kernel/patches/`; see `kernel/README.md`. It changes
neither Floss nor the pristine import and is not required for controllers whose
kernel driver already serializes isochronous TX teardown.

At install time, Dorsche creates the runtime state/log directories and, when no
administrator config exists, installs `config/floss/sysprops.conf`. It disables
only the Android-specific `LE_GET_VENDOR_CAPABILITIES` probe; standard USB HCI
controllers are not required to implement that vendor opcode. Existing host
configuration is never overwritten.

The installer also builds/installs the resolver and Classic audio smoke tools,
then adds a systemd drop-in making `bluetooth-audio` a
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

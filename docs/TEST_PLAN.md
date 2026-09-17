# Dorsche / Floss test plan

## Hardware

Minimum bring-up hardware:

- x86_64 Linux host with a dedicated USB Bluetooth controller exposed as
  `/sys/class/bluetooth/hciN` (normally `hci0`);
- one BLE peripheral supporting GATT read/write/notify;
- one Classic Bluetooth headset supporting A2DP sink, AVRCP, and HFP/HSP;
- microphone and speaker endpoints visible to PipeWire for Dorsche audio tests.

Preferred coverage hardware:

- Bluetooth 5.2+ controller with LE Extended Advertising and LE Audio ISO;
- LE Audio earbuds supporting BAP/CAP, unicast CIS, and preferably broadcast BIS;
- a second Linux machine/controller for deterministic peer and RF-isolation tests;
- USB protocol analyzer or Ellisys/Frontline sniffer for HCI/air-trace correlation;
- optional USB audio interface with hardware loopback for latency and barge-in
  measurements.

Do not use the host's only keyboard/mouse Bluetooth controller during bring-up.
BlueZ and Floss cannot own the same `hciN` simultaneously.

## Priority order

### P0: build and controller ownership

1. Verify the pristine source hash with `scripts/floss/verify-source.sh`.
2. Build release `btadapterd`, `btmanagerd`, and `btclient`.
3. Confirm the USB device, `btusb` binding, and `/sys/class/bluetooth/hci0`.
4. Stop `bluetooth.service`; prove no `bluetoothd` process owns the controller.
5. Start `btadapterd --hci=0`; require successful HCI Reset and controller-info
   read with no command timeout.
6. Require `org.chromium.bluetooth` and the adapter object on system D-Bus.

P0 blocks everything else. Failures here are controller/driver/firmware,
exclusive-ownership, permissions, or daemon-startup issues—not audio issues.

### P1: core protocol behavior

1. Enable/disable the adapter repeatedly and scan both BR/EDR and LE.
2. Pair, cancel pairing, unpair, restart Floss, then verify key persistence and
   reconnect behavior.
3. Exercise GATT discovery, MTU exchange, read/write, notifications, and a
   disconnect during outstanding I/O.
4. Run 100 connect/disconnect cycles and a 30-minute continuous scan while
   checking RSS and HCI command/event health.

### P2: Classic audio boundary

1. Negotiate A2DP SBC first; verify start/suspend/resume and AVRCP controls.
2. Feed deterministic PCM through `/var/run/bluetooth/audio/.a2dp_data` and
   check channel order, sample rate, underruns, presentation position, and drift.
3. Start HFP/SCO using `/var/run/bluetooth/audio/.sco_data`; verify both directions,
   initially forcing CVSD with `floss_hfp_smoke`. Require USB interface
   altsetting `0 -> 2 -> 0`, nonzero downlink/uplink byte counts, microphone
   RMS/peak, clean `StopScoCall` on every error path, and no btusb URB submission
   failure during teardown. Then enable mSBC where supported and verify the
   controller-specific `(altsetting, HCI packet size)` pair (for example
   `(1, 48)` on the tested CSR or `(3, 72)` on the tested RTL8761BU), call-state
   transitions, packet-status/PLC counters, and recovery after RF loss. Treat
   the pair atomically: a packet-size mismatch must fail loudly rather than be
   decoded as audio.
4. Only after the existing UIPC ABI is stable, attach Dorsche's PipeWire graph.

### P3: AI audio and barge-in

1. Validate microphone capture, VAD, inference, and sink Actors separately with
   synthetic frames before using a radio link.
2. Measure capture-to-VAD, VAD-to-interrupt, and end-to-end playback latency at
   p50/p95/p99; record channel queue depth and dropped-frame counters.
3. During playback, inject speech plus the synchronized echo-reference track;
   require prompt playback cancellation without false triggers from self-audio.
4. Stress full duplex for one hour with CPU pressure, USB reset, headset roam,
   and PipeWire graph restart.

### P4: LE Audio software unicast

1. Require a Bluetooth 5.2+ controller whose kernel HCI interface exposes ISO
   support and a real BAP/CAP unicast headset; the Classic HiTune T3 cannot
   satisfy this phase.
2. Discover the Floss group id, run `floss_le_audio_smoke` separately in output,
   input and duplex modes, and retain HCI ISO traces.
3. Verify negotiated sample rate, width, channel count and data interval against
   PAC/ASE configuration. Reject a pass based only on socket traffic.
4. Capture at least ten minutes of peer PCM and record RMS, peak, ring drops,
   late frames, CIS packet loss and presentation-delay drift.
5. Run `dorsche_pipewire --probe`, then expose source and sink nodes and verify
   them with `pw-cli`, `pw-record` and `pw-play`.

### P5: broadcast, offload and robustness

Test BIS/Auracast, controller/DSP offload, multi-device policy, suspend/resume,
controller hotplug, daemon crash recovery, malformed peer traffic, fuzz targets,
and long-duration soak only after P0-P4 are repeatable. Broadcast and offload
require positive provider capabilities; never enable them by changing only the
Floss Linux HAL verifier booleans.

## Evidence to retain

For every hardware run, record kernel/controller firmware versions, USB VID:PID,
Floss revision, Dorsche commit, D-Bus method results, daemon journal, btsnoop/HCI
trace, negotiated codecs, and latency/underrun/drop metrics. A test is not
"passed" merely because a process started; each phase requires observable HCI,
D-Bus, profile, and audio-plane evidence.

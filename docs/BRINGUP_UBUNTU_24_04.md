# Ubuntu 24.04 Floss bring-up

Date: 2026-09-16 UTC

## Result

P0 build, controller ownership, daemon startup, system D-Bus export, and active
BR/EDR + LE discovery passed on a standard x86_64 Ubuntu 24.04 host. Classic
pairing/profile connection and a deterministic A2DP/SBC PCM stream have also
passed on real hardware.

This is a real hardware smoke pass, not a socket-only simulation. HFP/SCO PCM,
LE Audio, repeated reconnect/suspend/resume, and soak remain open and must not
be inferred from this result.

## Environment

- kernel: `6.8.0-117-generic`;
- controller: USB `0a12:0001`, Cambridge Silicon Radio HCI mode;
- kernel object: `/sys/class/bluetooth/hci0`;
- AOSP Bluetooth revision:
  `ab906156f47eaead385190502647fe508229dfbf`;
- source tree SHA-256:
  `dc9e11599a929a23f23177bc6e78e1dc446f71d041265235a84d62d81a169aa9`.

BlueZ `bluetoothd` was stopped so Floss exclusively owned `hci0`.

## Evidence

The complete C++ GD/Fluoride and Rust Floss build produced release ELF binaries:

- `btadapterd` SHA-256
  `aa09e6759a7ea5483f00e076ace880b007c6bf4a17bf9cefc4fafcf7b177f96f`;
- `btmanagerd` SHA-256
  `80c794db325ed4b5078bf4e83cd30178481e295e2fe9909ed2a47636f0194961`;
- `btclient` SHA-256
  `fb10b9708b9723bb77ba70e431a1f0a1fcab97957d2db8676140fa30b9d9ca44`.

Journal evidence included:

- `HCI device ready`;
- `HAL opened successfully`;
- initialization of A2DP SBC source/sink, AVRCP Target, HFP AG, GATT, L2CAP,
  RFCOMM, HID, and the LE scanning manager.

`org.chromium.bluetooth` successfully exported:

```text
/org/chromium/bluetooth/hci0/adapter
/org/chromium/bluetooth/hci0/admin
/org/chromium/bluetooth/hci0/battery_manager
/org/chromium/bluetooth/hci0/battery_provider_manager
/org/chromium/bluetooth/hci0/gatt
/org/chromium/bluetooth/hci0/logging
/org/chromium/bluetooth/hci0/media
/org/chromium/bluetooth/hci0/qa
/org/chromium/bluetooth/hci0/telephony
```

D-Bus calls returned controller address `00:1A:7D:DA:71:13`,
`StartDiscovery=true`, and `IsDiscovering=true`. The journal recorded multiple
remote inquiry results. `CancelDiscovery=true` returned the controller to
`IsDiscovering=false` while `btadapterd@0.service` remained active.

## Classic headset and A2DP result

Floss bonded and connected a UGREEN HiTune T3 (`F0:BE:25:79:62:A4`). SDP and
profile callbacks confirmed A2DP Sink, AVRCP Controller, and HFP SLC. Dorsche's
Tokio-native `zbus` client then exercised the upstream media API without a C or
Python D-Bus shim:

- `SetAudioConfig` selected SBC, 48 kHz, signed 16-bit stereo;
- `StartAudioRequest` transferred a Unix listener FD with SCM_RIGHTS and Floss
  acknowledged it with status byte `1`;
- the Rust client fed 595,200 bytes of paced PCM over `.a2dp_data` for a
  three-second 440 Hz tone plus a 100 ms silent drain tail;
- the Floss SBC encoder reported 3840 PCM bytes per 20 ms tick and a final
  278-kbit/s configuration;
- `StopAudioRequest` acknowledged status byte `0`, and
  `GetA2dpAudioStarted` returned false after suspend;
- no PCM underflow was logged during the measured feed interval.

The unmodified upstream service unit restricts the daemon to network
capabilities, so its attempt to assign the UIPC socket to `bluetooth-audio`
needs that supplementary group. Dorsche installs a minimal systemd drop-in for
the group membership; it does not grant `CAP_CHOWN` or patch Floss.

## HFP/SCO status

HFP service-level connection passed, and `StartScoCall` progressed through
codec negotiation to Floss `BTA_AG_SCO_OPEN_ST`; the listener reported CVSD and
`.sco_data` was created. This is not yet an HFP audio pass. Ubuntu's upstream
6.8 kernel does not implement the ChromeOS management extensions used by Floss:

```text
MGMT_OP_GET_SCO_CODEC_CAPABILITIES = 0x0100
MGMT_OP_NOTIFY_SCO_CONNECTION_CHANGE = 0x0101
```

The USB audio interface consequently stayed at alternate setting zero, no SCO
clock packets reached Floss, and the UIPC transmit queue stopped draining. The
call was explicitly stopped and acknowledged. The next HFP task is a reviewed
upstream-kernel compatibility adapter (or the corresponding ChromeOS kernel
support); changing PipeWire, ALSA, or the application PCM code cannot repair
this missing btusb isochronous transport notification.

## Controller compatibility setting

The older CSR controller does not reply to Android's vendor-specific
`LE_GET_VENDOR_CAPABILITIES` opcode. The unmodified GD controller code already
provides an upstream sysprop for this case, so the Linux runtime installs:

```ini
[Sysprops]
bluetooth.core.le.vendor_capabilities.enabled=false
```

Without it, the HCI watchdog correctly requests a controller reset after the
vendor command timeout. This is configuration, not a fork of controller logic,
and can be re-enabled for a controller whose firmware implements the Android
vendor extension.

## Linux 6.18.15 HFP/SCO follow-up

A follow-up run used the same CSR USB controller and headset on a Debian 12
host running upstream Linux 6.18.15. Because the controller address and the
headset's stored identity were unchanged, its existing Floss bond record was
migrated to the second host; no key material is stored in this repository.
The headset then reconnected with A2DP, AVRCP, and HFP SLC active.

The patched host transport uses Linux 6.18's upstream `HCI_DRV_PKT` on Floss's
existing `HCI_CHANNEL_USER` socket. The opt-in sysprop is also registered in
Floss's explicit Linux sysprop allowlist; merely adding an unknown key to
`sysprops.conf` is intentionally ineffective.

Observed hardware results:

- A2DP/SBC at 48-kHz/S16LE/stereo sent 595,200 bytes of paced PCM and stopped
  with listener status zero;
- HFP forced CVSD at 8-kHz/S16LE/mono and completed two full-duplex runs;
- the five-second run sent 80,000 bytes and captured 79,824 bytes from the
  headset microphone (`RMS=2111.9`, `peak=21111`);
- the repeat run sent 32,000 bytes and captured 31,824 bytes
  (`RMS=2446.0`, `peak=19728`);
- the btusb audio interface transitioned `bAlternateSetting 0 -> 2 -> 0`;
- the repeat trace observed altsetting 2 approximately 413 ms after test start
  and restoration to zero approximately 32 ms after the two-second stream;
- a 12-second live microphone loopback sent and captured exactly 191,808 bytes
  through a bounded Rust channel (`RMS=999.7`, `peak=19715`) while preserving
  the same `0 -> 2 -> 0` USB transition;
- `StopScoCall` acknowledged cleanly, `btadapterd` remained active, and no
  kernel oops was observed.

Transparent mSBC was subsequently validated using the controller's 48-byte USB
SCO packet layout. A ten-second 16-kHz/S16LE/mono live microphone loopback sent
and captured 319,440 bytes (`RMS=35.1`, `peak=1403`). A following five-second
CVSD regression sent and captured 79,776 bytes (`RMS=3029.6`, `peak=28079`).
Both tests stopped cleanly.

Those runs also exposed and then validated a Linux btusb teardown fix. Upstream
6.18.15 can switch the USB isochronous interface to altsetting zero while a SCO
TX URB is still being submitted on the shared TX anchor, yielding one
`submission failed (90)` message. The out-of-tree patch documented in
`kernel/README.md` gives SCO TX its own anchor and serializes it with altsetting
changes. With the final module, consecutive mSBC and CVSD runs completed with no
new kernel message or oops. An eight-second A2DP/SBC regression then sent
1,555,200 bytes of 48-kHz/S16LE/stereo PCM, stopped with listener status zero,
and likewise added no kernel message. The user's original kernel source tree
was never modified. RF-loss recovery and long-duration soak remain separate
tests.

### Realtek RTL8761BU WBS result

A later run replaced the CSR controller with Realtek RTL8761BU `2b89:8761`
(firmware `0x09a98a6b`). A generic altsetting-1/24-byte configuration carried
data but marked all 1,064 decoded mSBC frames lost, yielding all-zero uplink
PCM. Linux 6.18 sets `BTUSB_USE_ALT3_FOR_WBS` on Realtek devices. Matching that
policy with altsetting 3 changed the controller's HCI SCO packet layout to 72
bytes. At that point in the investigation, before the ChromiumOS userspace-SCO
kernel interface was ported, the temporary verified configuration was:

```ini
bluetooth.hfp.linux_hci_driver_altsetting.enabled=true
bluetooth.hfp.linux_hci_driver_msbc_altsetting=3
bluetooth.hfp.linux_hci_driver_msbc_packet_size=72
```

This configuration is historical evidence, not current deployment guidance.
It has been removed from Dorsche. The production path uses the codec-aware
kernel MGMT interface, after which `btusb` selects altsetting 3 and reports the
72-byte WBS packet length without any userspace VID:PID or USB-layout policy.

An eight-second 440-Hz reference/downlink run then sent 256,000 bytes and
captured 255,120 bytes of nonzero uplink PCM. Floss decoded 1,063 frames with a
1.5052% packet-loss ratio and no `NO_DATA_RECEIVED`, `PARTIALLY_LOST`, or
`POSSIBLY_INCOMPLETE` events in that run. A 12-second microphone loopback sent
and captured 383,040 bytes (`RMS=170.8`, `peak=2549`) and stopped cleanly. The
following CVSD regression captured 79,680 bytes (`RMS=1831.7`, `peak=29701`),
and the A2DP/SBC regression sent 979,200 PCM bytes with listener status zero.

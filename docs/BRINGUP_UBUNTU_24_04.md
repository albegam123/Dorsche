# Ubuntu 24.04 Floss bring-up

Date: 2026-09-16 UTC

## Result

P0 build, controller ownership, daemon startup, system D-Bus export, and active
BR/EDR + LE discovery passed on a standard x86_64 Ubuntu 24.04 host.

This is a real hardware smoke pass, not a socket-only simulation. Pairing,
profile connection, A2DP PCM, HFP/SCO PCM, LE Audio, suspend/resume, and soak
remain open and must not be inferred from this result.

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

# Linux kernel compatibility patches

Dorsche keeps kernel changes out of both the vendored Floss snapshot and the
user's kernel checkout. The patches in `kernel/patches/` are narrow Linux host
compatibility fixes that can be reviewed, applied, built, and upstreamed
independently.

## Linux 6.18 btusb SCO teardown race

`0001-bluetooth-btusb-serialize-sco-tx-altsetting.patch` fixes a race between
SCO transmit submission and a USB Bluetooth isochronous alternate-setting
change. Without it, the final SCO URB can retain the old endpoint packet size
while `usb_set_interface(..., 0)` disables that endpoint, causing this harmless
but real teardown failure:

```text
Bluetooth: hci0: urb ... submission failed (90)
```

The patch gives SCO TX a dedicated USB anchor and a per-controller mutex. Only
SCO isochronous TX moves to that anchor; HCI command, ACL, and ISO data retain
the upstream `tx_anchor` path. The lock covers endpoint lookup, URB construction,
submission, cancellation, and altsetting switch. This is kernel-driver
serialization, not a global lock in Dorsche's actor graph.

The patch is based on upstream Linux 6.18.15. Apply it in a disposable worktree,
not in a dirty source checkout:

```bash
git -C /path/to/linux worktree add /tmp/dorsche-linux-6.18 v6.18.15
git -C /tmp/dorsche-linux-6.18 apply --check \
  /path/to/Dorsche/kernel/patches/0001-bluetooth-btusb-serialize-sco-tx-altsetting.patch
git -C /tmp/dorsche-linux-6.18 apply \
  /path/to/Dorsche/kernel/patches/0001-bluetooth-btusb-serialize-sco-tx-altsetting.patch
make -C /tmp/dorsche-linux-6.18 O=/tmp/dorsche-kbuild olddefconfig
make -C /tmp/dorsche-linux-6.18 O=/tmp/dorsche-kbuild \
  M=drivers/bluetooth modules -j"$(nproc)"
```

For a temporary hardware test, stop Floss before replacing `btusb`, ensure all
helper modules are available, load the newly built module, wait until
`/sys/class/bluetooth/hci0` and the management index both exist, and only then
restart Floss. The test module need not be installed under `/lib/modules`, so a
reboot restores the distribution module.

Acceptance requires both mSBC and CVSD full-duplex tests to return to USB
altsetting zero with equal uplink/downlink byte counts, nonzero microphone
energy, no `submission failed (90)`, and no kernel oops. Also repeat A2DP after
the module change because command/ACL transport must remain unaffected.

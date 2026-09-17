# Dorsche

The repository now vendors the real AOSP Gabeldorsche/Fluoride/Floss Bluetooth
stack under `third_party/floss`; see [docs/FLOSS_PORT.md](docs/FLOSS_PORT.md).
The prioritized hardware matrix is in [docs/TEST_PLAN.md](docs/TEST_PLAN.md),
and the first Ubuntu/USB-controller result is recorded in
[docs/BRINGUP_UBUNTU_24_04.md](docs/BRINGUP_UBUNTU_24_04.md).
That upstream code—not the small audio experiment below—owns HCI, pairing,
L2CAP/GATT, Bluetooth profiles, and the Linux D-Bus services. The Dorsche audio
graph attaches at Floss's existing media D-Bus and UIPC boundaries so the
Bluetooth core remains unmodified.

Dorsche is a compact Linux wireless/audio perception skeleton inspired by the
layering of Floss/Gabeldorsche and the fd-oriented graph model of PipeWire. The
hardware plane is C++20; the ownership, scheduling, and graph plane is Rust on
Tokio. It intentionally has no GStreamer dependency and no application-level
`Mutex`/`RwLock`.

The production wireless boundary is documented in
[`docs/WIRELESS_AUDIO_ARCHITECTURE.md`](docs/WIRELESS_AUDIO_ARCHITECTURE.md).
It includes a versioned LE Audio session ABI, ASE/LC3/ISO QoS and broadcast
types, explicit software/offload capability selection, a metadata-preserving
memfd SPSC data plane, Floss LE Audio unicast control, and an optional native
PipeWire adapter.

```text
                      bounded Tokio mpsc edges

  C++ capture  ->  Microphone actor  ----------------------->  VAD / AI actor
       ^                                                        ^       |
       | cxx UniquePtr + owned fd                               | echo  | barge-in
       v                                                        |       v
  Controller proxy  <--------------------------------------  Playback sink
                         borrowed &[f32] playback submit
```

The controller proxy owns the only `HardwareController`. Its two inboxes model
Floss's topshim boundary and serialize access without shared mutable state. The
speaker and capture paths are separate channels; `tokio::select! { biased; ... }`
gives playback control priority. VAD and playback likewise have dedicated echo
and capacity-one interruption edges, so a stop request cannot be trapped behind
ordinary audio traffic.

## Zero-copy ownership protocol

For every captured 10 ms, 16 kHz mono `f32` frame:

1. C++ allocates `/dev/dma_heap/system` when available, falling back to `memfd`
   for development hosts, and fills its shared mapping.
2. C++ executes a release fence and signals a pollable `eventfd` (a stand-in for
   a driver's `sync_file` DMA fence).
3. `take_buffer_fd()` unmaps C++ CPU access and atomically moves fd ownership by
   exchanging the stored fd with `-1`. `take_fence_fd()` does the same for the
   completion fence.
4. Rust adopts both descriptors as `OwnedFd`-backed `File`s, asynchronously
   waits for the fence, executes an acquire fence, and creates a read-only map.
5. `Arc<SharedPcmFrame>` moves through the graph. It dereferences to `[f32]` and
   keeps both the mapping and buffer lease alive. `Arc::clone` only increments a
   refcount; it never copies PCM pages.

A literal stable-Rust `Arc<[f32]>` cannot legally adopt external mmap/DMA-BUF
pages: `Arc` requires an allocator-owned block containing its private refcount
header. Constructing one from the mmap pointer would be undefined behavior, and
constructing one normally would copy. Dorsche therefore uses the honest
zero-copy equivalent `Arc<SharedPcmFrame>` on capture edges. Synthesized TTS
frames use ordinary `Arc<[f32]>`, and the same allocation is borrowed by C++
playback and returned to VAD as the echo reference.

## Files

```text
Cargo.toml                 Rust package and Tokio/cxx dependencies
build.rs                   CXX bridge and C++20 compilation
include/dorsche_core.h     move-only hardware objects
src/dorsche_core.cc        DMA-heap/memfd producer and playback shim
src/bridge.rs              audited cxx ABI and Send safety contract
src/main.rs                graph construction and shutdown
src/topshim/frame.rs       fd/fence adoption and mapped frame lease
src/topshim/actors.rs      four message-passing actors and priority tracks
src/bin/floss_audio_smoke.rs  native zbus + Floss UIPC A2DP hardware smoke test
src/bin/floss_hfp_smoke.rs    native zbus + full-duplex CVSD/SCO hardware smoke test
```

## Run

```bash
cargo run
```

Press Ctrl-C to stop. For an automated smoke run:

```bash
DORSCHE_DEMO_MS=1300 cargo run --quiet
```

The synthetic microphone alternates into a speech burst at 800 ms while the
assistant is playing. Expected output includes a `barge-in` event followed by
immediate playback cancellation. A production backend replaces the synthetic
fill/playback bodies with HCI/ALSA/USB ioctls and replaces `eventfd` with the
kernel driver's `sync_file`; none of the Rust graph or ownership boundaries need
to change. The demo brackets DMA-heap CPU access with `DMA_BUF_IOCTL_SYNC` when
the relevant Linux UAPI headers are available. HCI ownership belongs exclusively
to the vendored Floss `btadapterd`, never to this audio shim.

For a real Floss A2DP sink, use the native Rust control/data-plane smoke test:

```bash
cargo run --bin floss_audio_smoke -- \
  --address F0:BE:25:79:62:A4 --seconds 3 --volume 40
```

The tool uses Tokio-native `zbus`, including SCM_RIGHTS transfer of the listener
FD required by `StartAudioRequest`, and feeds paced 48-kHz/S16LE/stereo PCM into
Floss's existing UIPC socket. The caller must be a member of `bluetooth-audio`;
root is neither required nor recommended after logging into the updated group.

After A2DP passes, exercise HFP/SCO independently with:

```bash
cargo run --bin floss_hfp_smoke -- \
  --address F0:BE:25:79:62:A4 --seconds 5
```

The HFP test initially forces CVSD so the host PCM contract is deterministic
(8-kHz/S16LE/mono). It concurrently sends a quiet reference tone and consumes
microphone PCM, reports byte counts/RMS/peak, and always requests `StopScoCall`
after an accepted start—even when the data-plane test times out or fails.

For an audible end-to-end microphone check, echo uplink PCM back to the headset
at a feedback-safe default gain:

```bash
cargo run --bin floss_hfp_smoke -- \
  --address F0:BE:25:79:62:A4 --seconds 10 --loopback
```

`--loopback-gain` accepts `(0.0, 1.0]`; start with the default `0.35`. The live
path uses a bounded Tokio channel and transfers frame ownership between the SCO
uplink/downlink tasks without an audio-path mutex.

Once CVSD is stable, select wide-band 16-kHz mSBC explicitly:

```bash
cargo run --bin floss_hfp_smoke -- \
  --address F0:BE:25:79:62:A4 --seconds 10 --codec msbc --loopback
```

On the Linux 6.18 btusb driver-command path, CVSD uses USB altsetting 2.
Transparent mSBC defaults to altsetting 1, but its altsetting and HCI SCO packet
size are a controller-specific pair; every path must return to altsetting 0 on
stop. For example, the tested CSR `0a12:0001` uses altsetting 1 with 48-byte
packets:

```ini
[Sysprops]
bluetooth.hfp.linux_hci_driver_altsetting.enabled=true
bluetooth.hfp.linux_hci_driver_msbc_altsetting=1
bluetooth.hfp.linux_hci_driver_msbc_packet_size=48
```

The tested Realtek RTL8761BU `2b89:8761` follows Linux btusb's Realtek WBS
selection and requires altsetting 3 with 72-byte packets. Altsetting 1 produced
24-byte packets whose HCI status marked every decoded frame lost; changing only
the altsetting caused Floss to reject the resulting 72-byte packets as a size
mismatch. Use both values together:

```ini
bluetooth.hfp.linux_hci_driver_msbc_altsetting=3
bluetooth.hfp.linux_hci_driver_msbc_packet_size=72
```

Do not apply either controller quirk globally. The overlay accepts only USB
altsettings 1 through 6 and the mSBC framer's supported 24/48/60/72-byte sizes;
invalid values retain safe defaults. On Linux 6.18, btusb may log one
`EMSGSIZE (90)` while changing
back to altsetting 0 because the driver does not cancel its SCO TX anchor before
`usb_set_interface`; this occurs after the full-duplex stream has completed and
is distinct from a packet-size mismatch or kernel oops.

### Floss dependency policy

Dorsche keeps the complete Floss Bluetooth protocol/profile implementation,
but its host overlay removes the unused `grpcio` dependency from `bt_topshim`.
No Floss Rust source references that crate; removing it changes neither the
topshim ABI nor HCI, Classic, LE, A2DP, AVRCP, HFP, HID, GATT, PAN, or LE Audio
support. D-Bus remains the control plane. Upstream Pandora/PTS sources stay in
the pristine import because their optional test harnesses may use gRPC, but
they are not linked into `btadapterd` or the Dorsche runtime.

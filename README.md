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
scripts/floss/classic-audio-diag.sh   repeatable Classic audio diagnostics/soak
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

For an unmistakable full-duplex demonstration, delay the microphone return by
10 seconds:

```bash
cargo run --bin floss_hfp_smoke -- \
  --address F0:BE:25:79:62:A4 --seconds 15 --codec msbc \
  --loopback --loopback-delay-seconds 10 --loopback-gain 0.60
```

The first 10 seconds of downlink are silence while uplink capture remains
active. Playback then emits audio from exactly 10 seconds earlier while capture
continues. The delayed tail is drained at the PCM clock rate, so this example
runs for approximately 25 seconds rather than truncating the last 10 seconds.

Once CVSD is stable, select wide-band 16-kHz mSBC explicitly:

```bash
cargo run --bin floss_hfp_smoke -- \
  --address F0:BE:25:79:62:A4 --seconds 10 --codec msbc --loopback
```

The Bluetooth stack must not choose a USB alternate setting. HFP negotiates
CVSD or transparent mSBC, then a codec-aware Linux driver notification lets
`btusb_work()` select bandwidth from the active codec, USB descriptors,
controller SCO MTU, and kernel-owned quirks. In particular, no Dorsche table
maps USB VID:PID values to transport parameters.

Mainline Linux 6.18 lacks the two ChromiumOS management operations used by
pristine Floss for that notification. Apply Dorsche's port of the official
ChromiumOS interface together with the SCO teardown fix:

```bash
git -C /path/to/linux apply \
  /path/to/Dorsche/kernel/patches/0001-bluetooth-btusb-serialize-sco-tx-altsetting.patch
git -C /path/to/linux apply \
  /path/to/Dorsche/kernel/patches/0002-bluetooth-add-floss-userspace-sco.patch
```

No Floss sysprop, sidecar, or Rust resolver selects a USB layout. A controller
that the kernel does not mark WBS-capable stays on CVSD rather than being
force-enabled. On an unpatched Linux 6.18 kernel, btusb may log one
`EMSGSIZE (90)` while changing back to altsetting 0 because the driver does not
cancel its SCO TX anchor before
`usb_set_interface`; this occurs after the full-duplex stream has completed and
is distinct from a packet-size mismatch or kernel oops.

Run the complete Classic audio diagnostic once, or turn it into a soak with a
larger cycle count:

```bash
sudo scripts/floss/classic-audio-diag.sh \
  --address F0:BE:25:79:62:A4 --hci 4 --cycles 1 --seconds 5

sudo scripts/floss/classic-audio-diag.sh \
  --address F0:BE:25:79:62:A4 --hci 4 --cycles 100 --seconds 10 --no-delayed
```

Every case is independently marked PASS/FAIL. Raw PCM, udev controller metadata,
USB topology, sysprops, connected devices, packet-loss summaries and the daemon
journal are retained under `out/classic-audio-*`. A D-Bus `Connect` return is
not treated as success: the script waits for the headset to appear in
`GetConnectedDevices` and aborts the audio matrix if paging times out.

### Floss dependency policy

Dorsche keeps the complete Floss Bluetooth protocol/profile implementation,
but its host overlay removes the unused `grpcio` dependency from `bt_topshim`.
No Floss Rust source references that crate; removing it changes neither the
topshim ABI nor HCI, Classic, LE, A2DP, AVRCP, HFP, HID, GATT, PAN, or LE Audio
support. D-Bus remains the control plane. Upstream Pandora/PTS sources stay in
the pristine import because their optional test harnesses may use gRPC, but
they are not linked into `btadapterd` or the Dorsche runtime.

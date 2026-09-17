# Dorsche wireless audio architecture

## Ownership boundary

Floss remains the only owner of HCI, GATT control services, BAP/CAP/ASE state,
CIG/CIS and BIG/BIS setup. Dorsche does not implement a competing Bluetooth
state machine. Its stable Rust boundary models sessions, provider capabilities,
media timing and payload ownership independently of D-Bus, PipeWire and the
current Floss implementation.

```text
application / AI                         ordinary Linux applications
        |                                            |
        | typed session API                          | SPA PCM
        v                                            v
 Dorsche Wireless Audio HAL <--------------> optional PipeWire adapter
        |
        | zbus control                     memfd SPSC media rings
        v
 pristine Floss/GD  <------ .lea_data PCM ------> software host adapter
        |
        | HCI ISO (CIS/BIS)
        v
 controller / DSP
```

LE Audio control services use ATT/GATT. Realtime audio uses LE Isochronous
Channels; it is never represented as an ordinary GATT notification stream.

## ABI rules

`src/wireless/types.rs` is ABI version 1 and represents:

- software, controller/DSP-offload and broadcast session types;
- PCM and LC3 configuration;
- ASE, SDU interval, framing, PHY, retransmission, transport latency and
  presentation-delay ranges;
- transparent/controller ISO data paths and controller delay;
- BIS/subgroup/broadcast configuration;
- presentation position and remote-device delay;
- PCM and LC3-SDU frame descriptors, including sequence, stream handle, ISO
  status, capture/presentation timestamps and deadline.

Offload is never an implicit optimization of a software session. It is a
different `SessionType`; if the capability registry cannot provide it, the
caller must explicitly request a software fallback.

## Data plane

The local realtime transport is a sealed-shape `memfd` SPSC ring with an
`eventfd` wakeup. Producer and consumer cursors live on separate cache lines.
The producer writes directly into a reserved shared slot, then publishes it
with a release store. The consumer acquires the cursor and holds an immutable
slot lease; dropping the lease releases that slot back to the producer.

The eventfd is only a wakeup. It is not used as the memory-consistency fence.
There is no application `Mutex`/`RwLock`, per-frame allocation or hidden queue.

The current upstream Floss Linux audio boundary is a Unix `SOCK_STREAM` at
`/run/bluetooth/audio/.lea_data`. It carries negotiated PCM, not LC3 SDUs. This
means one kernel-to-userspace copy and one framing adapter remain at that
boundary. Preserving LC3/ISO metadata end-to-end requires a future upstreamable
Floss host ABI, not a private modification of the pristine source tree.

The optional PipeWire adapter negotiates native SPA PCM buffers. Its process
callback performs one bounded copy between a SPA buffer and the Dorsche ring.
The core DMA/AI graph remains zero-copy; the project does not claim that the
current UIPC and SPA adapters make the entire radio-to-application path
zero-copy.

## Floss LE Audio path

`src/wireless/floss.rs` uses Tokio-native zbus for control only:

1. select the active Floss group;
2. publish media or microphone context metadata;
3. start host and/or peer direction;
4. wait for Floss's `Started` confirmation;
5. read the negotiated PCM parameters;
6. connect `.lea_data`;
7. on every partial failure, stop all already-started directions.

`floss_le_audio_smoke` tests unicast output, input or full duplex without
PipeWire. `dorsche_pipewire` attaches either direction to a native PipeWire
node. Neither tool opens an HCI socket.

## Runtime support versus retained upstream source

The vendored Floss snapshot contains the upstream LE Audio unicast and
broadcast core. Its Linux host verifier currently reports hardware offload and
broadcast as unsupported, and its Linux Rust service exposes the unicast client
but not the broadcaster interface. Dorsche models those sessions and provider
capabilities now, without falsely enabling an incomplete runtime path.

Completing hardware offload requires controller/DSP vendor data-path support.
Completing broadcast on Linux requires a narrow upstream-style overlay for the
Floss broadcaster topshim, callback/service surface and Linux HAL verifier,
followed by BIS-capable hardware testing. Merely changing the verifier boolean
would be incorrect.

## Build and probes

The PipeWire adapter is optional:

```bash
sudo apt install libpipewire-0.3-dev pipewire-bin
cargo test --all-targets --features pipewire
cargo run --features pipewire --bin dorsche_pipewire -- --probe
```

With a connected LE Audio group:

```bash
cargo run --bin floss_le_audio_smoke -- \
  --group 1 --mode duplex --seconds 30 --capture /tmp/le-uplink.raw

cargo run --features pipewire --bin dorsche_pipewire -- \
  --group 1 --direction source --node-name dorsche.le.microphone

cargo run --features pipewire --bin dorsche_pipewire -- \
  --group 1 --direction sink --node-name dorsche.le.speaker
```

The group id comes from Floss LE Audio callbacks/client state; it is not the
adapter number or Bluetooth address.

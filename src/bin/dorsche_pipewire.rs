use anyhow::{Context, Result, bail, ensure};
use dorsche::wireless::{
    Consumer, FlossLeAudio, FlossLeDirections, FrameDescriptor, PayloadKind, PcmConfiguration,
    PipeWireAdapter, Producer, RingPair,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, ReadHalf, WriteHalf},
    net::UnixStream,
};

enum Direction {
    Source,
    Sink,
}

struct Args {
    adapter: u32,
    group_id: i32,
    direction: Direction,
    node_name: String,
    probe: bool,
}

impl Args {
    fn parse() -> Result<Self> {
        let mut adapter = 0;
        let mut group_id = -1;
        let mut direction = None;
        let mut node_name = None;
        let mut probe = false;
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            let missing = || anyhow::anyhow!("{arg} requires a value");
            match arg.as_str() {
                "--adapter" => adapter = args.next().ok_or_else(missing)?.parse()?,
                "--group" => group_id = args.next().ok_or_else(missing)?.parse()?,
                "--direction" => {
                    direction = Some(match args.next().ok_or_else(missing)?.as_str() {
                        "source" => Direction::Source,
                        "sink" => Direction::Sink,
                        value => bail!("unknown direction {value}; use source or sink"),
                    })
                }
                "--node-name" => node_name = Some(args.next().ok_or_else(missing)?),
                "--probe" => probe = true,
                "-h" | "--help" => {
                    println!(
                        "Usage: dorsche_pipewire --group ID --direction source|sink \\\n                         [--adapter 0] [--node-name dorsche.le_audio]\n\
                         dorsche_pipewire --probe"
                    );
                    std::process::exit(0);
                }
                _ => bail!("unknown argument: {arg}"),
            }
        }
        ensure!(
            probe || group_id >= 0,
            "--group is required and must be non-negative"
        );
        Ok(Self {
            adapter,
            group_id,
            direction: if probe {
                Direction::Source
            } else {
                direction.context("--direction is required")?
            },
            node_name: node_name.unwrap_or_else(|| "dorsche.le_audio".into()),
            probe,
        })
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let args = Args::parse()?;
    if args.probe {
        PipeWireAdapter::probe_server(PcmConfiguration {
            sample_rate_hz: 48_000,
            bits_per_sample: 16,
            channels: 2,
            data_interval_us: 10_000,
        })?;
        eprintln!("PipeWire SPA probe passed: 48000 Hz/S16LE/stereo");
        return Ok(());
    }
    let floss = FlossLeAudio::system(args.adapter).await?;
    let directions = match args.direction {
        Direction::Source => FlossLeDirections::INPUT,
        Direction::Sink => FlossLeDirections::OUTPUT,
    };
    let mut data_path = floss
        .start_software_unicast(args.group_id, directions)
        .await
        .context("start Floss LE Audio for PipeWire")?;
    let stream = data_path.take_stream()?;
    let (reader, writer) = tokio::io::split(stream);

    let pump_result = match args.direction {
        Direction::Source => {
            drop(writer);
            let pcm = data_path
                .peer_pcm
                .context("Floss returned no peer PCM config")?;
            let (producer, consumer) = RingPair::create(16, pcm.bytes_per_interval()? as u32)?;
            spawn_pipewire_source(args.node_name, pcm, consumer);
            eprintln!("Dorsche PipeWire LE Audio source active; press Ctrl-C to stop");
            tokio::select! {
                result = pump_peer_to_ring(reader, producer, pcm) => result,
                result = tokio::signal::ctrl_c() => result.context("Ctrl-C handler"),
            }
        }
        Direction::Sink => {
            drop(reader);
            let pcm = data_path
                .host_pcm
                .context("Floss returned no host PCM config")?;
            let (producer, consumer) = RingPair::create(16, pcm.bytes_per_interval()? as u32)?;
            spawn_pipewire_sink(args.node_name, pcm, producer);
            eprintln!("Dorsche PipeWire LE Audio sink active; press Ctrl-C to stop");
            tokio::select! {
                result = pump_ring_to_host(consumer, writer) => result,
                result = tokio::signal::ctrl_c() => result.context("Ctrl-C handler"),
            }
        }
    };

    let stop_result = data_path.stop().await;
    pump_result?;
    stop_result?;
    Ok(())
}

fn spawn_pipewire_source(node_name: String, pcm: PcmConfiguration, consumer: Consumer) {
    std::thread::Builder::new()
        .name("dorsche-pw-source".into())
        .spawn(move || {
            if let Err(error) = PipeWireAdapter::run_source(&node_name, pcm, consumer) {
                eprintln!("PipeWire source stopped: {error:#}");
            }
        })
        .expect("spawn PipeWire source thread");
}

fn spawn_pipewire_sink(node_name: String, pcm: PcmConfiguration, producer: Producer) {
    std::thread::Builder::new()
        .name("dorsche-pw-sink".into())
        .spawn(move || {
            if let Err(error) = PipeWireAdapter::run_sink(&node_name, pcm, producer) {
                eprintln!("PipeWire sink stopped: {error:#}");
            }
        })
        .expect("spawn PipeWire sink thread");
}

async fn pump_peer_to_ring(
    mut reader: ReadHalf<UnixStream>,
    mut producer: Producer,
    pcm: PcmConfiguration,
) -> Result<()> {
    let bytes = pcm.bytes_per_interval()?;
    let mut sequence = 0_u64;
    loop {
        let mut slot = producer
            .try_reserve()
            .context("PipeWire source ring overrun")?;
        reader
            .read_exact(&mut slot.payload_mut()[..bytes])
            .await
            .context("read Floss LE Audio peer PCM")?;
        let mut descriptor = FrameDescriptor::new(PayloadKind::Pcm, sequence);
        descriptor.sample_rate_hz = pcm.sample_rate_hz;
        descriptor.channels = u16::from(pcm.channels);
        descriptor.bits_per_sample = u16::from(pcm.bits_per_sample);
        descriptor.capture_time_ns = monotonic_time_ns();
        slot.commit(descriptor, bytes)?;
        sequence = sequence.wrapping_add(1);
    }
}

async fn pump_ring_to_host(
    mut consumer: Consumer,
    mut writer: WriteHalf<UnixStream>,
) -> Result<()> {
    loop {
        if let Some(frame) = consumer.try_acquire()? {
            ensure!(
                frame.descriptor().payload_kind == PayloadKind::Pcm as u8,
                "PipeWire sink received a non-PCM frame"
            );
            writer
                .write_all(frame.payload())
                .await
                .context("write Floss LE Audio host PCM")?;
        } else {
            consumer.wait_readable().await?;
        }
    }
}

fn monotonic_time_ns() -> u64 {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime writes one initialized timespec.
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) } != 0 {
        return 0;
    }
    time.tv_sec as u64 * 1_000_000_000 + time.tv_nsec as u64
}

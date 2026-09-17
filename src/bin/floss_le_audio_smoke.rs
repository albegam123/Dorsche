use std::{f64::consts::TAU, fs::File, io::Write, path::PathBuf, time::Duration};

use anyhow::{Context, Result, bail, ensure};
use dorsche::wireless::{
    FlossLeAudio, FlossLeDirections, FrameDescriptor, PayloadKind, PcmConfiguration, RingPair,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, ReadHalf, WriteHalf},
    net::UnixStream,
    time::{Instant, MissedTickBehavior, interval_at},
};

#[derive(Clone, Copy, Debug)]
enum Mode {
    Output,
    Input,
    Duplex,
}

struct Args {
    adapter: u32,
    group_id: i32,
    seconds: u64,
    frequency_hz: f64,
    mode: Mode,
    capture: Option<PathBuf>,
}

impl Args {
    fn parse() -> Result<Self> {
        let mut parsed = Self {
            adapter: 0,
            group_id: -1,
            seconds: 10,
            frequency_hz: 440.0,
            mode: Mode::Duplex,
            capture: None,
        };
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            let missing = || anyhow::anyhow!("{arg} requires a value");
            match arg.as_str() {
                "--adapter" => parsed.adapter = args.next().ok_or_else(missing)?.parse()?,
                "--group" => parsed.group_id = args.next().ok_or_else(missing)?.parse()?,
                "--seconds" => parsed.seconds = args.next().ok_or_else(missing)?.parse()?,
                "--frequency" => parsed.frequency_hz = args.next().ok_or_else(missing)?.parse()?,
                "--mode" => {
                    parsed.mode = match args.next().ok_or_else(missing)?.as_str() {
                        "output" => Mode::Output,
                        "input" => Mode::Input,
                        "duplex" => Mode::Duplex,
                        mode => bail!("unknown mode {mode}; use output, input, or duplex"),
                    }
                }
                "--capture" => {
                    parsed.capture = Some(PathBuf::from(args.next().ok_or_else(missing)?))
                }
                "-h" | "--help" => {
                    println!(
                        "Usage: floss_le_audio_smoke --group ID [--adapter 0] \\\n+                         [--mode output|input|duplex] [--seconds 10] \\\n+                         [--frequency 440] [--capture peer-input.raw]"
                    );
                    std::process::exit(0);
                }
                _ => bail!("unknown argument: {arg}"),
            }
        }
        ensure!(
            parsed.group_id >= 0,
            "--group is required and must be non-negative"
        );
        ensure!(parsed.seconds > 0, "--seconds must be positive");
        ensure!(
            parsed.frequency_hz.is_finite() && parsed.frequency_hz > 0.0,
            "--frequency must be positive"
        );
        Ok(parsed)
    }

    fn directions(&self) -> FlossLeDirections {
        match self.mode {
            Mode::Output => FlossLeDirections::OUTPUT,
            Mode::Input => FlossLeDirections::INPUT,
            Mode::Duplex => FlossLeDirections::FULL_DUPLEX,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse()?;
    let floss = FlossLeAudio::system(args.adapter).await?;
    let (group_status, stream_status) = floss.group_status(args.group_id).await?;
    eprintln!(
        "LE Audio group {}: group_status={}, stream_status={}",
        args.group_id, group_status, stream_status
    );

    let mut data_path = floss
        .start_software_unicast(args.group_id, args.directions())
        .await
        .context("start Floss LE Audio software unicast")?;
    eprintln!(
        "negotiated: host={:?}, peer={:?}, mode={:?}",
        data_path.host_pcm, data_path.peer_pcm, args.mode
    );
    let stream = data_path.take_stream()?;
    let (reader, writer) = tokio::io::split(stream);

    let io_result = match args.mode {
        Mode::Output => {
            drop(reader);
            write_tone(writer, data_path.host_pcm.unwrap(), &args).await
        }
        Mode::Input => {
            drop(writer);
            capture_peer(reader, data_path.peer_pcm.unwrap(), &args).await
        }
        Mode::Duplex => {
            let output = write_tone(writer, data_path.host_pcm.unwrap(), &args);
            let input = capture_peer(reader, data_path.peer_pcm.unwrap(), &args);
            let (played, captured) = tokio::try_join!(output, input)?;
            Ok(played + captured)
        }
    };

    let stop_result = data_path.stop().await;
    let bytes = io_result?;
    stop_result?;
    eprintln!("LE Audio stopped cleanly; processed {bytes} bytes");
    Ok(())
}

async fn write_tone(
    mut writer: WriteHalf<UnixStream>,
    pcm: PcmConfiguration,
    args: &Args,
) -> Result<u64> {
    let bytes_per_interval = pcm.bytes_per_interval()?;
    let frames_per_interval =
        bytes_per_interval / usize::from(pcm.channels) / usize::from(pcm.bits_per_sample / 8);
    let interval = Duration::from_micros(u64::from(pcm.data_interval_us));
    let intervals = args.seconds * 1_000_000 / u64::from(pcm.data_interval_us);
    let mut ticker = interval_at(Instant::now(), interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut bytes = vec![0_u8; bytes_per_interval];
    let mut sample_index = 0_u64;

    for _ in 0..intervals {
        ticker.tick().await;
        for frame in 0..frames_per_interval {
            let phase = TAU * args.frequency_hz * sample_index as f64 / pcm.sample_rate_hz as f64;
            write_sample(
                &mut bytes,
                frame,
                pcm.channels,
                pcm.bits_per_sample,
                phase.sin() * 0.05,
            );
            sample_index += 1;
        }
        writer
            .write_all(&bytes)
            .await
            .context("write LE Audio host PCM")?;
    }
    writer.shutdown().await?;
    Ok(intervals * bytes_per_interval as u64)
}

fn write_sample(buffer: &mut [u8], frame: usize, channels: u8, bits: u8, sample: f64) {
    let width = usize::from(bits / 8);
    let integer = (sample * ((1_i64 << (bits - 1)) - 1) as f64) as i64;
    let encoded = integer.to_le_bytes();
    for channel in 0..usize::from(channels) {
        let offset = (frame * usize::from(channels) + channel) * width;
        buffer[offset..offset + width].copy_from_slice(&encoded[..width]);
    }
}

async fn capture_peer(
    mut reader: ReadHalf<UnixStream>,
    pcm: PcmConfiguration,
    args: &Args,
) -> Result<u64> {
    let bytes_per_interval = pcm.bytes_per_interval()?;
    let intervals = args.seconds * 1_000_000 / u64::from(pcm.data_interval_us);
    let (mut producer, mut consumer) = RingPair::create(8, bytes_per_interval as u32)?;
    let mut capture = args
        .capture
        .as_ref()
        .map(File::create)
        .transpose()
        .context("create LE Audio capture file")?;
    let mut sum_squares = 0_f64;
    let mut peak = 0_f64;
    let mut samples = 0_u64;

    for sequence in 0..intervals {
        let mut slot = producer
            .try_reserve()
            .context("LE Audio capture ring overrun")?;
        reader
            .read_exact(&mut slot.payload_mut()[..bytes_per_interval])
            .await
            .context("read LE Audio peer PCM")?;
        let mut descriptor = FrameDescriptor::new(PayloadKind::Pcm, sequence);
        descriptor.sample_rate_hz = pcm.sample_rate_hz;
        descriptor.channels = u16::from(pcm.channels);
        descriptor.bits_per_sample = u16::from(pcm.bits_per_sample);
        descriptor.capture_time_ns = monotonic_time_ns();
        slot.commit(descriptor, bytes_per_interval)?;

        // The consumer observes the exact memfd slot after an Acquire load; no
        // staging Vec or per-frame allocation occurs between UIPC and here.
        let lease = consumer
            .try_acquire()?
            .context("published LE Audio frame was not visible")?;
        if let Some(file) = capture.as_mut() {
            file.write_all(lease.payload())?;
        }
        for sample in decode_samples(lease.payload(), pcm.bits_per_sample) {
            let sample = sample?;
            sum_squares += sample * sample;
            peak = peak.max(sample.abs());
            samples += 1;
        }
    }

    let rms = if samples == 0 {
        0.0
    } else {
        (sum_squares / samples as f64).sqrt()
    };
    eprintln!(
        "peer capture: bytes={}, samples={}, rms={:.6}, peak={:.6}, drops={}",
        intervals * bytes_per_interval as u64,
        samples,
        rms,
        peak,
        producer.dropped_frames()
    );
    Ok(intervals * bytes_per_interval as u64)
}

fn decode_samples(bytes: &[u8], bits: u8) -> impl Iterator<Item = Result<f64>> + '_ {
    let width = usize::from(bits / 8);
    bytes.chunks_exact(width).map(move |sample| {
        let integer = match bits {
            16 => i16::from_le_bytes([sample[0], sample[1]]) as i64,
            24 => {
                let extended = if sample[2] & 0x80 == 0 { 0 } else { 0xff };
                i32::from_le_bytes([sample[0], sample[1], sample[2], extended]) as i64
            }
            32 => i32::from_le_bytes([sample[0], sample[1], sample[2], sample[3]]) as i64,
            _ => bail!("unsupported PCM width {bits}"),
        };
        Ok(integer as f64 / (1_i64 << (bits - 1)) as f64)
    })
}

fn monotonic_time_ns() -> u64 {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) } != 0 {
        return 0;
    }
    time.tv_sec as u64 * 1_000_000_000 + time.tv_nsec as u64
}

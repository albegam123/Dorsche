use std::f32::consts::TAU;
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tokio::time::{Instant, MissedTickBehavior, interval_at, timeout};
use zbus::zvariant::Fd;

const FLOSS_SERVICE: &str = "org.chromium.bluetooth";
const MEDIA_PATH: &str = "/org/chromium/bluetooth/hci0/media";
const SCO_DATA_PATH: &str = "/run/bluetooth/audio/.sco_data";

const FRAME_TIME: Duration = Duration::from_millis(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Codec {
    Cvsd,
    Msbc,
}

impl Codec {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "cvsd" => Ok(Self::Cvsd),
            "msbc" => Ok(Self::Msbc),
            _ => bail!("--codec must be cvsd or msbc"),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Cvsd => "CVSD",
            Self::Msbc => "mSBC",
        }
    }

    fn sample_rate(self) -> usize {
        match self {
            Self::Cvsd => 8_000,
            Self::Msbc => 16_000,
        }
    }

    fn expected_bit(self) -> u8 {
        match self {
            Self::Cvsd => 0b001,
            Self::Msbc => 0b010,
        }
    }

    fn disabled_codecs(self) -> i32 {
        match self {
            Self::Cvsd => 0b110,
            Self::Msbc => 0b101,
        }
    }
}

#[zbus::proxy(
    interface = "org.chromium.bluetooth.BluetoothMedia",
    default_service = "org.chromium.bluetooth",
    default_path = "/org/chromium/bluetooth/hci0/media"
)]
trait BluetoothMedia {
    #[zbus(name = "SetHfpActiveDevice")]
    async fn set_hfp_active_device(&self, address: &str) -> zbus::Result<()>;

    #[zbus(name = "StartScoCall")]
    async fn start_sco_call(
        &self,
        address: &str,
        sco_offload: bool,
        disabled_codecs: i32,
        listener: Fd<'_>,
    ) -> zbus::Result<bool>;

    #[zbus(name = "GetHfpAudioFinalCodecs")]
    async fn get_hfp_audio_final_codecs(&self, address: &str) -> zbus::Result<u8>;

    #[zbus(name = "StopScoCall")]
    async fn stop_sco_call(&self, address: &str, listener: Fd<'_>) -> zbus::Result<()>;
}

#[derive(Debug)]
struct Args {
    address: String,
    seconds: u64,
    frequency_hz: f32,
    loopback: bool,
    loopback_gain: f32,
    capture_path: Option<PathBuf>,
    codec: Codec,
}

impl Args {
    fn parse() -> Result<Self> {
        let mut args = std::env::args().skip(1);
        let mut parsed = Self {
            address: String::new(),
            seconds: 5,
            frequency_hz: 440.0,
            loopback: false,
            loopback_gain: 0.35,
            capture_path: None,
            codec: Codec::Cvsd,
        };

        while let Some(arg) = args.next() {
            let value = || anyhow::anyhow!("{arg} requires a value");
            match arg.as_str() {
                "--address" => parsed.address = args.next().ok_or_else(value)?,
                "--seconds" => parsed.seconds = args.next().ok_or_else(value)?.parse()?,
                "--frequency" => parsed.frequency_hz = args.next().ok_or_else(value)?.parse()?,
                "--loopback" => parsed.loopback = true,
                "--loopback-gain" => {
                    parsed.loopback_gain = args.next().ok_or_else(value)?.parse()?
                }
                "--capture" => {
                    parsed.capture_path = Some(PathBuf::from(args.next().ok_or_else(value)?))
                }
                "--codec" => parsed.codec = Codec::parse(&args.next().ok_or_else(value)?)?,
                "-h" | "--help" => {
                    println!(
                        "Usage: floss_hfp_smoke --address XX:XX:XX:XX:XX:XX \
                         [--seconds 5] [--codec cvsd|msbc] [--frequency 440] \
                         [--loopback [--loopback-gain 0.35]] [--capture FILE]"
                    );
                    std::process::exit(0);
                }
                _ => bail!("unknown argument: {arg}"),
            }
        }

        if parsed.address.is_empty() {
            bail!("--address is required");
        }
        if parsed.seconds == 0 || !parsed.frequency_hz.is_finite() || parsed.frequency_hz <= 0.0 {
            bail!("duration and frequency must be positive");
        }
        if !parsed.loopback_gain.is_finite()
            || parsed.loopback_gain <= 0.0
            || parsed.loopback_gain > 1.0
        {
            bail!("--loopback-gain must be in (0.0, 1.0]");
        }
        Ok(parsed)
    }
}

fn listener_pair() -> Result<(UnixStream, StdUnixStream)> {
    let (client, daemon) = StdUnixStream::pair().context("create listener socketpair")?;
    client.set_nonblocking(true)?;
    Ok((UnixStream::from_std(client)?, daemon))
}

async fn wait_for_listener(listener: &mut UnixStream, operation: &str) -> Result<u8> {
    let mut status = [0_u8; 1];
    timeout(Duration::from_secs(10), listener.read_exact(&mut status))
        .await
        .with_context(|| format!("timed out waiting for Floss {operation}"))??;
    Ok(status[0])
}

async fn connect_sco_uipc() -> Result<UnixStream> {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match UnixStream::connect(SCO_DATA_PATH).await {
            Ok(stream) => return Ok(stream),
            Err(error) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(20)).await;
                let _ = error;
            }
            Err(error) => return Err(error).with_context(|| format!("connect to {SCO_DATA_PATH}")),
        }
    }
}

#[derive(Debug, Default)]
struct CaptureStats {
    bytes: u64,
    samples: u64,
    sum_squares: f64,
    peak: i16,
}

impl CaptureStats {
    fn account(&mut self, pcm: &[u8]) {
        self.bytes += pcm.len() as u64;
        for sample in pcm.chunks_exact(2) {
            let value = i16::from_le_bytes([sample[0], sample[1]]);
            self.samples += 1;
            self.sum_squares += f64::from(value) * f64::from(value);
            self.peak = self.peak.max(value.saturating_abs());
        }
    }

    fn rms(&self) -> f64 {
        if self.samples == 0 {
            0.0
        } else {
            (self.sum_squares / self.samples as f64).sqrt()
        }
    }
}

async fn exercise_full_duplex(
    stream: UnixStream,
    args: &Args,
) -> Result<(u64, CaptureStats, Vec<u8>)> {
    let (mut reader, mut writer) = stream.into_split();
    let duration = Duration::from_secs(args.seconds);
    let frequency_hz = args.frequency_hz;
    let sample_rate = args.codec.sample_rate();

    let playback = async move {
        let samples_per_frame = sample_rate / 100;
        let mut pcm = vec![0_u8; samples_per_frame * size_of::<i16>()];
        let amplitude = i16::MAX as f32 * 10_f32.powf(-36.0 / 20.0);
        let ticks = duration.as_millis() as usize / FRAME_TIME.as_millis() as usize;
        let mut sample_index = 0_u64;
        let mut bytes = 0_u64;
        let mut ticker = interval_at(Instant::now(), FRAME_TIME);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

        for _ in 0..ticks {
            ticker.tick().await;
            for (index, sample) in pcm.chunks_exact_mut(2).enumerate() {
                let phase =
                    TAU * frequency_hz * (sample_index + index as u64) as f32 / sample_rate as f32;
                sample.copy_from_slice(&((amplitude * phase.sin()) as i16).to_le_bytes());
            }
            sample_index += samples_per_frame as u64;
            writer
                .write_all(&pcm)
                .await
                .context("write SCO playback PCM")?;
            bytes += pcm.len() as u64;
        }
        writer.shutdown().await.context("close SCO playback half")?;
        Ok::<_, anyhow::Error>(bytes)
    };

    let capture = async move {
        let deadline = Instant::now() + duration;
        let mut stats = CaptureStats::default();
        let mut captured_pcm =
            Vec::with_capacity(sample_rate * size_of::<i16>() * args.seconds as usize);
        let mut buffer = [0_u8; 640];
        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => break,
                result = reader.read(&mut buffer) => {
                    let read = result.context("read SCO microphone PCM")?;
                    if read == 0 {
                        break;
                    }
                    stats.account(&buffer[..read]);
                    captured_pcm.extend_from_slice(&buffer[..read]);
                }
            }
        }
        Ok::<_, anyhow::Error>((stats, captured_pcm))
    };

    // The two halves must run concurrently: servicing downlink only would
    // eventually back-pressure uplink and invalidate this full-duplex test.
    let (played, (captured, captured_pcm)) = tokio::try_join!(playback, capture)?;
    Ok((played, captured, captured_pcm))
}

async fn exercise_loopback(
    stream: UnixStream,
    args: &Args,
) -> Result<(u64, CaptureStats, Vec<u8>)> {
    let (mut reader, mut writer) = stream.into_split();
    let deadline = Instant::now() + Duration::from_secs(args.seconds);
    let gain = args.loopback_gain;
    // Eight small SCO reads bound latency and memory. If downlink ever stalls,
    // backpressure reaches capture instead of growing an unbounded audio queue.
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(8);

    let capture = async move {
        let mut stats = CaptureStats::default();
        let mut captured_pcm =
            Vec::with_capacity(args.codec.sample_rate() * size_of::<i16>() * args.seconds as usize);
        let mut buffer = [0_u8; 640];
        loop {
            let read = tokio::select! {
                _ = tokio::time::sleep_until(deadline) => break,
                result = reader.read(&mut buffer) => {
                    result.context("read SCO microphone PCM")?
                }
            };
            if read == 0 {
                break;
            }
            stats.account(&buffer[..read]);
            captured_pcm.extend_from_slice(&buffer[..read]);
            // Ownership, rather than a shared mutable buffer, crosses from the
            // uplink task to the downlink task. No lock is on the audio path.
            let frame = buffer[..read].to_vec();
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => break,
                result = tx.send(frame) => {
                    if result.is_err() {
                        break;
                    }
                }
            }
        }
        Ok::<_, anyhow::Error>((stats, captured_pcm))
    };

    let playback = async move {
        let mut bytes = 0_u64;
        while let Some(mut frame) = rx.recv().await {
            // Attenuate before echoing microphone PCM. Acoustic leakage plus a
            // unity-gain software loop can otherwise become a positive-feedback
            // oscillator even when the Bluetooth transport itself is healthy.
            for sample in frame.chunks_exact_mut(2) {
                let input = i16::from_le_bytes([sample[0], sample[1]]);
                let output = (f32::from(input) * gain)
                    .round()
                    .clamp(f32::from(i16::MIN), f32::from(i16::MAX))
                    as i16;
                sample.copy_from_slice(&output.to_le_bytes());
            }
            writer
                .write_all(&frame)
                .await
                .context("echo SCO microphone PCM to headset")?;
            bytes += frame.len() as u64;
        }
        writer.shutdown().await.context("close SCO loopback half")?;
        Ok::<_, anyhow::Error>(bytes)
    };

    let ((captured, captured_pcm), played) = tokio::try_join!(capture, playback)?;
    Ok((played, captured, captured_pcm))
}

async fn stop_sco(media: &BluetoothMediaProxy<'_>, address: &str) -> Result<u8> {
    let (mut stop_rx, stop_tx) = listener_pair()?;
    media
        .stop_sco_call(address, Fd::from(stop_tx.as_fd()))
        .await?;
    wait_for_listener(&mut stop_rx, "SCO stop").await
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse()?;
    let connection = zbus::Connection::system()
        .await
        .context("connect to system D-Bus")?;
    let media = BluetoothMediaProxy::builder(&connection)
        .destination(FLOSS_SERVICE)?
        .path(MEDIA_PATH)?
        .build()
        .await
        .context("create Floss BluetoothMedia proxy")?;

    media.set_hfp_active_device(&args.address).await?;
    let (mut start_rx, start_tx) = listener_pair()?;
    if !media
        .start_sco_call(
            &args.address,
            false,
            args.codec.disabled_codecs(),
            Fd::from(start_tx.as_fd()),
        )
        .await?
    {
        bail!("Floss rejected StartScoCall (is the HFP SLC connected?)");
    }

    // From this point onward every exit path must issue StopScoCall. Leaving
    // SCO connected also leaves the USB interface in its isochronous setting.
    let test_result = async {
        let codec = wait_for_listener(&mut start_rx, "SCO start").await?;
        let reported = media.get_hfp_audio_final_codecs(&args.address).await?;
        let expected = args.codec.expected_bit();
        if codec != expected || reported != expected {
            bail!(
                "expected {} codec bit {expected}, listener={codec}, reported={reported}",
                args.codec.name()
            );
        }
        println!(
            "SCO started: {} {} Hz/S16LE/mono, mode={}",
            args.codec.name(),
            args.codec.sample_rate(),
            if args.loopback { "mic-loopback" } else { "reference-tone" }
        );

        let data = connect_sco_uipc().await?;
        let (played, captured, captured_pcm) = timeout(
            Duration::from_secs(args.seconds + 5),
            async {
                if args.loopback {
                    exercise_loopback(data, &args).await
                } else {
                    exercise_full_duplex(data, &args).await
                }
            },
        )
        .await
        .context("full-duplex SCO test timed out")??;
        println!(
            "SCO full duplex: played={played} B, captured={} B, mic_samples={}, mic_rms={:.1}, mic_peak={}",
            captured.bytes,
            captured.samples,
            captured.rms(),
            captured.peak
        );
        if captured.bytes == 0 {
            bail!("SCO uplink produced no microphone PCM");
        }
        if let Some(path) = &args.capture_path {
            std::fs::write(path, &captured_pcm)
                .with_context(|| format!("write captured PCM to {}", path.display()))?;
            println!(
                "captured raw PCM: {} ({} Hz/S16LE/mono, {} B)",
                path.display(),
                args.codec.sample_rate(),
                captured_pcm.len()
            );
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;

    let stop_result = stop_sco(&media, &args.address).await;
    match (test_result, stop_result) {
        (Ok(()), Ok(status)) if status == 0 => {
            println!("SCO stopped cleanly");
            Ok(())
        }
        (Ok(()), Ok(status)) => bail!("unexpected SCO stop listener status={status}"),
        (Ok(()), Err(stop_error)) => Err(stop_error).context("SCO test passed but cleanup failed"),
        (Err(test_error), Ok(_)) => Err(test_error),
        (Err(test_error), Err(stop_error)) => {
            Err(test_error).context(format!("SCO cleanup also failed: {stop_error:#}"))
        }
    }
}

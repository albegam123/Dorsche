use std::f32::consts::TAU;
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream as StdUnixStream;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::time::{Instant, MissedTickBehavior, interval_at, timeout};
use zbus::zvariant::Fd;

const FLOSS_SERVICE: &str = "org.chromium.bluetooth";
const MEDIA_PATH: &str = "/org/chromium/bluetooth/hci0/media";
const SCO_DATA_PATH: &str = "/run/bluetooth/audio/.sco_data";

// Force narrow-band CVSD for the first hardware bring-up. It has an unambiguous
// 8-kHz/S16LE host contract and exercises btusb isochronous altsetting 2.
const DISABLE_MSBC_AND_LC3: i32 = 0b110;
const CVSD_CODEC_BIT: u8 = 0b001;
const SAMPLE_RATE: usize = 8_000;
const FRAME_TIME: Duration = Duration::from_millis(10);
const SAMPLES_PER_FRAME: usize = SAMPLE_RATE / 100;

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
}

impl Args {
    fn parse() -> Result<Self> {
        let mut args = std::env::args().skip(1);
        let mut parsed = Self {
            address: String::new(),
            seconds: 5,
            frequency_hz: 440.0,
        };

        while let Some(arg) = args.next() {
            let value = || anyhow::anyhow!("{arg} requires a value");
            match arg.as_str() {
                "--address" => parsed.address = args.next().ok_or_else(value)?,
                "--seconds" => parsed.seconds = args.next().ok_or_else(value)?.parse()?,
                "--frequency" => parsed.frequency_hz = args.next().ok_or_else(value)?.parse()?,
                "-h" | "--help" => {
                    println!(
                        "Usage: floss_hfp_smoke --address XX:XX:XX:XX:XX:XX \
                         [--seconds 5] [--frequency 440]"
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

async fn exercise_full_duplex(stream: UnixStream, args: &Args) -> Result<(u64, CaptureStats)> {
    let (mut reader, mut writer) = stream.into_split();
    let duration = Duration::from_secs(args.seconds);
    let frequency_hz = args.frequency_hz;

    let playback = async move {
        let mut pcm = vec![0_u8; SAMPLES_PER_FRAME * size_of::<i16>()];
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
                    TAU * frequency_hz * (sample_index + index as u64) as f32 / SAMPLE_RATE as f32;
                sample.copy_from_slice(&((amplitude * phase.sin()) as i16).to_le_bytes());
            }
            sample_index += SAMPLES_PER_FRAME as u64;
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
                }
            }
        }
        Ok::<_, anyhow::Error>(stats)
    };

    // The two halves must run concurrently: servicing downlink only would
    // eventually back-pressure uplink and invalidate this full-duplex test.
    tokio::try_join!(playback, capture)
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
            DISABLE_MSBC_AND_LC3,
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
        if codec != CVSD_CODEC_BIT || reported != CVSD_CODEC_BIT {
            bail!("expected CVSD codec bit 1, listener={codec}, reported={reported}");
        }
        println!("SCO started: CVSD 8000 Hz/S16LE/mono");

        let data = connect_sco_uipc().await?;
        let (played, captured) = timeout(
            Duration::from_secs(args.seconds + 5),
            exercise_full_duplex(data, &args),
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

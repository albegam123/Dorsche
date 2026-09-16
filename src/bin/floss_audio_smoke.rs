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
const A2DP_DATA_PATH: &str = "/run/bluetooth/audio/.a2dp_data";

const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: usize = 2;
const FRAME_TIME: Duration = Duration::from_millis(20);
const FRAMES_PER_TICK: usize = 960;

#[zbus::proxy(
    interface = "org.chromium.bluetooth.BluetoothMedia",
    default_service = "org.chromium.bluetooth",
    default_path = "/org/chromium/bluetooth/hci0/media"
)]
trait BluetoothMedia {
    #[zbus(name = "SetActiveDevice")]
    async fn set_active_device(&self, address: &str) -> zbus::Result<()>;

    #[zbus(name = "SetAudioConfig")]
    async fn set_audio_config(
        &self,
        address: &str,
        codec_type: u32,
        sample_rate: i32,
        bits_per_sample: i32,
        channel_mode: i32,
    ) -> zbus::Result<bool>;

    #[zbus(name = "SetVolume")]
    async fn set_volume(&self, volume: u8) -> zbus::Result<()>;

    #[zbus(name = "StartAudioRequest")]
    async fn start_audio_request(&self, listener: Fd<'_>) -> zbus::Result<bool>;

    #[zbus(name = "StopAudioRequest")]
    async fn stop_audio_request(&self, listener: Fd<'_>) -> zbus::Result<()>;

    #[zbus(name = "GetA2dpAudioStarted")]
    async fn get_a2dp_audio_started(&self, address: &str) -> zbus::Result<bool>;
}

#[derive(Debug)]
struct Args {
    address: String,
    seconds: u64,
    volume: u8,
    frequency_hz: f32,
}

impl Args {
    fn parse() -> Result<Self> {
        let mut args = std::env::args().skip(1);
        let mut parsed = Self {
            address: String::new(),
            seconds: 3,
            volume: 40,
            frequency_hz: 440.0,
        };

        while let Some(arg) = args.next() {
            let value = || anyhow::anyhow!("{arg} requires a value");
            match arg.as_str() {
                "--address" => parsed.address = args.next().ok_or_else(value)?,
                "--seconds" => parsed.seconds = args.next().ok_or_else(value)?.parse()?,
                "--volume" => parsed.volume = args.next().ok_or_else(value)?.parse()?,
                "--frequency" => parsed.frequency_hz = args.next().ok_or_else(value)?.parse()?,
                "-h" | "--help" => {
                    println!(
                        "Usage: floss_audio_smoke --address XX:XX:XX:XX:XX:XX \
                         [--seconds 3] [--volume 40] [--frequency 440]"
                    );
                    std::process::exit(0);
                }
                _ => bail!("unknown argument: {arg}"),
            }
        }

        if parsed.address.is_empty() {
            bail!("--address is required");
        }
        if parsed.volume > 127 {
            bail!("--volume must be in 0..=127");
        }
        if parsed.seconds == 0 || !parsed.frequency_hz.is_finite() || parsed.frequency_hz <= 0.0 {
            bail!("duration and frequency must be positive");
        }
        Ok(parsed)
    }
}

async fn listener_pair() -> Result<(UnixStream, StdUnixStream)> {
    let (client, daemon) = StdUnixStream::pair().context("create listener socketpair")?;
    client.set_nonblocking(true)?;
    Ok((UnixStream::from_std(client)?, daemon))
}

async fn wait_for_listener(listener: &mut UnixStream, operation: &str) -> Result<u8> {
    let mut status = [0_u8; 1];
    timeout(Duration::from_secs(8), listener.read_exact(&mut status))
        .await
        .with_context(|| format!("timed out waiting for Floss {operation}"))??;
    Ok(status[0])
}

async fn wait_for_audio_state(
    media: &BluetoothMediaProxy<'_>,
    address: &str,
    expected: bool,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if media.get_a2dp_audio_started(address).await? == expected {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("timed out waiting for A2DP started={expected}");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn stop_existing_stream(media: &BluetoothMediaProxy<'_>, address: &str) -> Result<()> {
    if !media.get_a2dp_audio_started(address).await? {
        return Ok(());
    }

    let (mut stop_rx, stop_tx) = listener_pair().await?;
    media.stop_audio_request(Fd::from(stop_tx.as_fd())).await?;
    let status = wait_for_listener(&mut stop_rx, "pre-test A2DP stop").await?;
    if status != 0 {
        bail!("unexpected pre-test A2DP stop status={status}");
    }
    wait_for_audio_state(media, address, false).await
}

async fn connect_uipc() -> Result<UnixStream> {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match UnixStream::connect(A2DP_DATA_PATH).await {
            Ok(stream) => return Ok(stream),
            Err(error) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(20)).await;
                let _ = error;
            }
            Err(error) => {
                return Err(error).with_context(|| format!("connect to {A2DP_DATA_PATH}"));
            }
        }
    }
}

async fn write_test_tone(stream: &mut UnixStream, args: &Args) -> Result<u64> {
    // Floss' SBC feeding contract is interleaved S16LE PCM. At 48 kHz stereo,
    // one 20 ms scheduler quantum is exactly 960 * 2 * 2 = 3840 bytes.
    let mut pcm = vec![0_u8; FRAMES_PER_TICK * CHANNELS * size_of::<i16>()];
    let mut sample_index = 0_u64;
    let tick_count = args.seconds * 1_000 / FRAME_TIME.as_millis() as u64;
    let amplitude = i16::MAX as f32 * 10_f32.powf(-30.0 / 20.0);
    let mut ticker = interval_at(Instant::now(), FRAME_TIME);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

    for _ in 0..tick_count {
        ticker.tick().await;
        for frame in 0..FRAMES_PER_TICK {
            let phase = TAU * args.frequency_hz * sample_index as f32 / SAMPLE_RATE as f32;
            let sample = (amplitude * phase.sin()) as i16;
            let bytes = sample.to_le_bytes();
            for channel in 0..CHANNELS {
                let offset = (frame * CHANNELS + channel) * size_of::<i16>();
                pcm[offset..offset + 2].copy_from_slice(&bytes);
            }
            sample_index += 1;
        }
        stream.write_all(&pcm).await.context("feed A2DP PCM")?;
    }

    // A short silent tail lets the encoder drain without clipping the last SBC packet.
    pcm.fill(0);
    for _ in 0..5 {
        ticker.tick().await;
        stream
            .write_all(&pcm)
            .await
            .context("feed A2DP drain silence")?;
    }
    stream.shutdown().await.context("close A2DP data stream")?;
    Ok((tick_count + 5) * pcm.len() as u64)
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

    media.set_active_device(&args.address).await?;
    // Codec changes suspend an already-started stream asynchronously. Starting again while
    // LOCAL_SUSPEND_PENDING is set races the Floss state machine and is correctly rejected.
    stop_existing_stream(&media, &args.address).await?;
    // Floss bitfields: SBC=0, 48 kHz=0x02, S16=0x01, stereo=0x02.
    if !media
        .set_audio_config(&args.address, 0, 0x02, 0x01, 0x02)
        .await?
    {
        bail!("Floss rejected SBC 48-kHz/S16/stereo configuration");
    }
    wait_for_audio_state(&media, &args.address, false).await?;
    media.set_volume(args.volume).await?;

    let (mut start_rx, start_tx) = listener_pair().await?;
    let accepted = media
        .start_audio_request(Fd::from(start_tx.as_fd()))
        .await?;
    if !accepted {
        bail!("Floss rejected StartAudioRequest");
    }
    let status = wait_for_listener(&mut start_rx, "A2DP start").await?;
    if status != 1 || !media.get_a2dp_audio_started(&args.address).await? {
        bail!("A2DP start failed: listener status={status}");
    }

    println!(
        "A2DP started: {} Hz/S16LE/stereo SBC, volume {}/127",
        SAMPLE_RATE, args.volume
    );
    let mut data = connect_uipc().await?;
    let bytes = write_test_tone(&mut data, &args).await?;
    println!("sent {bytes} PCM bytes at {} Hz", args.frequency_hz);

    let (mut stop_rx, stop_tx) = listener_pair().await?;
    media.stop_audio_request(Fd::from(stop_tx.as_fd())).await?;
    let status = wait_for_listener(&mut stop_rx, "A2DP stop").await?;
    println!("A2DP stopped: listener status={status}");
    Ok(())
}

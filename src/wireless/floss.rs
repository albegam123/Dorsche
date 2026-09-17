use std::{collections::HashMap, path::Path, time::Duration};

use anyhow::{Context, Result, bail, ensure};
use tokio::{
    net::UnixStream,
    time::{Instant, sleep},
};
use zbus::zvariant::OwnedValue;

use super::types::PcmConfiguration;

pub const FLOSS_SERVICE: &str = "org.chromium.bluetooth";
pub const LE_AUDIO_DATA_PATH: &str = "/run/bluetooth/audio/.lea_data";

type PropertyMap = HashMap<String, OwnedValue>;

#[zbus::proxy(interface = "org.chromium.bluetooth.BluetoothMedia")]
trait BluetoothMedia {
    #[zbus(name = "GroupSetActive")]
    async fn group_set_active(&self, group_id: i32) -> zbus::Result<()>;

    #[zbus(name = "HostStartAudioRequest")]
    async fn host_start_audio_request(&self) -> zbus::Result<bool>;

    #[zbus(name = "HostStopAudioRequest")]
    async fn host_stop_audio_request(&self) -> zbus::Result<()>;

    #[zbus(name = "PeerStartAudioRequest")]
    async fn peer_start_audio_request(&self) -> zbus::Result<bool>;

    #[zbus(name = "PeerStopAudioRequest")]
    async fn peer_stop_audio_request(&self) -> zbus::Result<()>;

    #[zbus(name = "GetHostPcmConfig")]
    async fn get_host_pcm_config(&self) -> zbus::Result<PropertyMap>;

    #[zbus(name = "GetPeerPcmConfig")]
    async fn get_peer_pcm_config(&self) -> zbus::Result<PropertyMap>;

    #[zbus(name = "GetHostStreamStarted")]
    async fn get_host_stream_started(&self) -> zbus::Result<i32>;

    #[zbus(name = "GetPeerStreamStarted")]
    async fn get_peer_stream_started(&self) -> zbus::Result<i32>;

    #[zbus(name = "GetGroupStatus")]
    async fn get_group_status(&self, group_id: i32) -> zbus::Result<i32>;

    #[zbus(name = "GetGroupStreamStatus")]
    async fn get_group_stream_status(&self, group_id: i32) -> zbus::Result<i32>;

    #[zbus(name = "SourceMetadataChanged")]
    async fn source_metadata_changed(
        &self,
        usage: i32,
        content_type: i32,
        gain: f64,
    ) -> zbus::Result<bool>;

    #[zbus(name = "SinkMetadataChanged")]
    async fn sink_metadata_changed(&self, source: i32, gain: f64) -> zbus::Result<bool>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FlossLeDirections {
    pub host_to_peer: bool,
    pub peer_to_host: bool,
}

impl FlossLeDirections {
    pub const OUTPUT: Self = Self {
        host_to_peer: true,
        peer_to_host: false,
    };
    pub const INPUT: Self = Self {
        host_to_peer: false,
        peer_to_host: true,
    };
    pub const FULL_DUPLEX: Self = Self {
        host_to_peer: true,
        peer_to_host: true,
    };

    fn validate(self) -> Result<()> {
        ensure!(
            self.host_to_peer || self.peer_to_host,
            "at least one LE Audio direction must be selected"
        );
        Ok(())
    }
}

/// Tokio/zbus control-plane client for Floss's upstream Linux LE Audio API.
/// It does not duplicate BAP/CAP/ASE state; Floss remains the sole owner.
#[derive(Clone)]
pub struct FlossLeAudio {
    connection: zbus::Connection,
    media_path: String,
    data_path: String,
}

impl FlossLeAudio {
    pub async fn system(adapter: u32) -> Result<Self> {
        let connection = zbus::Connection::system()
            .await
            .context("connect to system D-Bus")?;
        Ok(Self::with_connection(connection, adapter))
    }

    pub fn with_connection(connection: zbus::Connection, adapter: u32) -> Self {
        Self {
            connection,
            media_path: format!("/org/chromium/bluetooth/hci{adapter}/media"),
            data_path: LE_AUDIO_DATA_PATH.to_owned(),
        }
    }

    pub fn with_data_path(mut self, data_path: impl Into<String>) -> Self {
        self.data_path = data_path.into();
        self
    }

    async fn proxy(&self) -> Result<BluetoothMediaProxy<'_>> {
        BluetoothMediaProxy::builder(&self.connection)
            .destination(FLOSS_SERVICE)?
            .path(self.media_path.as_str())?
            .build()
            .await
            .context("create Floss BluetoothMedia proxy")
    }

    pub async fn group_status(&self, group_id: i32) -> Result<(i32, i32)> {
        let proxy = self.proxy().await?;
        Ok((
            proxy.get_group_status(group_id).await?,
            proxy.get_group_stream_status(group_id).await?,
        ))
    }

    /// Request Floss software-host streaming and connect its UIPC socket.
    /// Control is completed before returning the byte stream; on partial
    /// failure, all already-started directions are explicitly rolled back.
    pub async fn start_software_unicast(
        &self,
        group_id: i32,
        directions: FlossLeDirections,
    ) -> Result<FlossLeDataPath> {
        directions.validate()?;
        ensure!(group_id >= 0, "LE Audio group id must be non-negative");
        let proxy = self.proxy().await?;
        proxy.group_set_active(group_id).await?;

        // Metadata selects the BAP context before starting the ASEs. Values
        // match upstream BtLeAudioUsage/ContentType/Source enums.
        if directions.host_to_peer {
            ensure!(
                proxy.source_metadata_changed(1, 2, 1.0).await?,
                "Floss rejected LE Audio media metadata"
            );
        }
        if directions.peer_to_host {
            ensure!(
                proxy.sink_metadata_changed(1, 1.0).await?,
                "Floss rejected LE Audio microphone metadata"
            );
        }

        let mut host_started = false;
        let mut peer_started = false;
        let start_result: Result<()> = async {
            if directions.host_to_peer {
                ensure!(
                    proxy.host_start_audio_request().await?,
                    "Floss rejected HostStartAudioRequest"
                );
                host_started = true;
            }
            if directions.peer_to_host {
                ensure!(
                    proxy.peer_start_audio_request().await?,
                    "Floss rejected PeerStartAudioRequest"
                );
                peer_started = true;
            }
            wait_started(&proxy, directions, Duration::from_secs(8)).await
        }
        .await;

        if let Err(error) = start_result {
            stop_directions(&proxy, host_started, peer_started).await;
            return Err(error);
        }

        let host_pcm = if directions.host_to_peer {
            Some(parse_pcm_config(proxy.get_host_pcm_config().await?)?)
        } else {
            None
        };
        let peer_pcm = if directions.peer_to_host {
            Some(parse_pcm_config(proxy.get_peer_pcm_config().await?)?)
        } else {
            None
        };

        let stream = match connect_with_deadline(&self.data_path, Duration::from_secs(5)).await {
            Ok(stream) => stream,
            Err(error) => {
                stop_directions(&proxy, host_started, peer_started).await;
                return Err(error);
            }
        };

        Ok(FlossLeDataPath {
            owner: self.clone(),
            stream: Some(stream),
            directions,
            host_pcm,
            peer_pcm,
            stopped: false,
        })
    }
}

pub struct FlossLeDataPath {
    owner: FlossLeAudio,
    stream: Option<UnixStream>,
    directions: FlossLeDirections,
    pub host_pcm: Option<PcmConfiguration>,
    pub peer_pcm: Option<PcmConfiguration>,
    stopped: bool,
}

impl FlossLeDataPath {
    pub fn take_stream(&mut self) -> Result<UnixStream> {
        self.stream
            .take()
            .context("LE Audio data stream already taken")
    }

    /// Stop both directions even if one D-Bus call fails. Callers should use
    /// this on every exit path; Drop logs a warning because async cleanup cannot
    /// be made reliable from a synchronous destructor.
    pub async fn stop(mut self) -> Result<()> {
        self.stream.take();
        let proxy = self.owner.proxy().await?;
        let mut first_error = None;
        if self.directions.peer_to_host
            && let Err(error) = proxy.peer_stop_audio_request().await
        {
            first_error = Some(error);
        }
        if self.directions.host_to_peer
            && let Err(error) = proxy.host_stop_audio_request().await
            && first_error.is_none()
        {
            first_error = Some(error);
        }
        self.stopped = true;
        if let Some(error) = first_error {
            return Err(error).context("stop Floss LE Audio data path");
        }
        Ok(())
    }
}

impl Drop for FlossLeDataPath {
    fn drop(&mut self) {
        if !self.stopped {
            eprintln!("warning: FlossLeDataPath dropped without async stop(); Floss owns recovery");
        }
    }
}

async fn wait_started(
    proxy: &BluetoothMediaProxy<'_>,
    directions: FlossLeDirections,
    timeout: Duration,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let host = !directions.host_to_peer || proxy.get_host_stream_started().await? == 1;
        let peer = !directions.peer_to_host || proxy.get_peer_stream_started().await? == 1;
        if host && peer {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("timed out waiting for Floss LE Audio stream confirmation");
        }
        sleep(Duration::from_millis(20)).await;
    }
}

async fn stop_directions(proxy: &BluetoothMediaProxy<'_>, host: bool, peer: bool) {
    if peer {
        let _ = proxy.peer_stop_audio_request().await;
    }
    if host {
        let _ = proxy.host_stop_audio_request().await;
    }
}

async fn connect_with_deadline(path: impl AsRef<Path>, timeout: Duration) -> Result<UnixStream> {
    let path = path.as_ref();
    let deadline = Instant::now() + timeout;
    loop {
        match UnixStream::connect(path).await {
            Ok(stream) => return Ok(stream),
            Err(error) if Instant::now() < deadline => {
                let _ = error;
                sleep(Duration::from_millis(20)).await;
            }
            Err(error) => {
                return Err(error).with_context(|| format!("connect to {}", path.display()));
            }
        }
    }
}

fn parse_pcm_config(properties: PropertyMap) -> Result<PcmConfiguration> {
    let config = PcmConfiguration {
        data_interval_us: property(&properties, "data_interval_us")?,
        sample_rate_hz: property(&properties, "sample_rate")?,
        bits_per_sample: property(&properties, "bits_per_sample")?,
        channels: property(&properties, "channels_count")?,
    };
    config.validate()?;
    config.bytes_per_interval()?;
    Ok(config)
}

fn property<T>(properties: &PropertyMap, name: &str) -> Result<T>
where
    for<'a> T: TryFrom<&'a OwnedValue, Error = zbus::zvariant::Error>,
{
    let value = properties
        .get(name)
        .with_context(|| format!("Floss PCM config missing {name}"))?;
    T::try_from(value).with_context(|| format!("invalid Floss PCM property {name}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_upstream_floss_property_map() {
        let properties = HashMap::from([
            ("data_interval_us".into(), OwnedValue::from(10_000_u32)),
            ("sample_rate".into(), OwnedValue::from(48_000_u32)),
            ("bits_per_sample".into(), OwnedValue::from(16_u8)),
            ("channels_count".into(), OwnedValue::from(2_u8)),
        ]);
        let config = parse_pcm_config(properties).unwrap();
        assert_eq!(config.bytes_per_interval().unwrap(), 1_920);
    }
}

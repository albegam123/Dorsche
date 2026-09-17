use std::fmt;

use anyhow::{Result, bail, ensure};

/// Version of the Dorsche Wireless Audio HAL contract, independent of the
/// implementation version of Floss or of a northbound audio server.
pub const WIRELESS_AUDIO_ABI_VERSION: u16 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum SessionType {
    LeUnicastSoftwareEncoding = 1,
    LeUnicastSoftwareDecoding = 2,
    LeUnicastHardwareEncoding = 3,
    LeUnicastHardwareDecoding = 4,
    LeBroadcastSoftwareEncoding = 5,
    LeBroadcastHardwareEncoding = 6,
}

impl SessionType {
    pub const fn is_offloaded(self) -> bool {
        matches!(
            self,
            Self::LeUnicastHardwareEncoding
                | Self::LeUnicastHardwareDecoding
                | Self::LeBroadcastHardwareEncoding
        )
    }

    pub const fn carries_host_payload(self) -> bool {
        !self.is_offloaded()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum StreamDirection {
    Sink = 1,
    Source = 2,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum AudioContext {
    Unspecified = 0,
    Conversational = 1,
    Media = 2,
    Game = 3,
    Live = 4,
    SoundEffects = 5,
    Notifications = 6,
    EmergencyAlarm = 7,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Framing {
    Unframed = 0,
    Framed = 1,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Phy {
    Le1M = 1,
    Le2M = 2,
    LeCoded = 3,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Packing {
    Sequential = 0,
    Interleaved = 1,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PcmConfiguration {
    pub sample_rate_hz: u32,
    pub bits_per_sample: u8,
    pub channels: u8,
    pub data_interval_us: u32,
}

impl PcmConfiguration {
    pub fn validate(self) -> Result<()> {
        ensure!(
            matches!(
                self.sample_rate_hz,
                8_000 | 16_000 | 24_000 | 32_000 | 44_100 | 48_000
            ),
            "unsupported PCM sample rate {}",
            self.sample_rate_hz
        );
        ensure!(
            matches!(self.bits_per_sample, 16 | 24 | 32),
            "PCM width must be 16, 24, or 32 bits"
        );
        ensure!(
            (1..=8).contains(&self.channels),
            "PCM channel count must be 1..=8"
        );
        ensure!(
            self.data_interval_us > 0,
            "PCM data interval must be non-zero"
        );
        Ok(())
    }

    pub fn bytes_per_interval(self) -> Result<usize> {
        self.validate()?;
        let frames = u64::from(self.sample_rate_hz)
            .checked_mul(u64::from(self.data_interval_us))
            .ok_or_else(|| anyhow::anyhow!("PCM interval overflow"))?;
        ensure!(
            frames % 1_000_000 == 0,
            "PCM interval is not an integral number of frames"
        );
        let bytes =
            frames / 1_000_000 * u64::from(self.channels) * u64::from(self.bits_per_sample / 8);
        usize::try_from(bytes).map_err(Into::into)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Lc3Configuration {
    pub sampling_frequency_hz: u32,
    pub frame_duration_us: u16,
    pub octets_per_codec_frame: u16,
    pub codec_frames_per_sdu: u8,
    pub audio_channel_allocation: u32,
}

impl Lc3Configuration {
    pub fn validate(self) -> Result<()> {
        ensure!(
            matches!(
                self.sampling_frequency_hz,
                8_000 | 16_000 | 24_000 | 32_000 | 44_100 | 48_000
            ),
            "unsupported LC3 sampling frequency"
        );
        ensure!(
            matches!(self.frame_duration_us, 7_500 | 10_000),
            "LC3 frame duration must be 7.5 or 10 ms"
        );
        ensure!(self.octets_per_codec_frame > 0, "LC3 frame cannot be empty");
        ensure!(
            self.codec_frames_per_sdu > 0,
            "SDU must contain at least one codec frame"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IsoQos {
    pub sdu_interval_us: u32,
    pub framing: Framing,
    pub phys: Vec<Phy>,
    pub max_sdu: u16,
    pub retransmission_number: u8,
    pub max_transport_latency_ms: u16,
    pub presentation_delay_min_us: u32,
    pub presentation_delay_max_us: u32,
}

impl IsoQos {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.sdu_interval_us > 0, "SDU interval must be non-zero");
        ensure!(!self.phys.is_empty(), "at least one LE PHY is required");
        ensure!(self.max_sdu > 0, "Max SDU must be non-zero");
        ensure!(
            self.presentation_delay_min_us <= self.presentation_delay_max_us,
            "invalid presentation delay range"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DataPathConfiguration {
    pub data_path_id: u8,
    pub transparent: bool,
    pub controller_delay_us: u32,
    pub codec_configuration: Vec<u8>,
    pub vendor_configuration: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AseConfiguration {
    pub ase_id: u8,
    pub direction: StreamDirection,
    pub cis_handle: Option<u16>,
    pub lc3: Lc3Configuration,
    pub qos: IsoQos,
    pub data_path: DataPathConfiguration,
}

impl AseConfiguration {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.ase_id != 0, "ASE id zero is reserved");
        self.lc3.validate()?;
        self.qos.validate()?;
        let sdu_octets =
            u32::from(self.lc3.octets_per_codec_frame) * u32::from(self.lc3.codec_frames_per_sdu);
        ensure!(
            sdu_octets <= u32::from(self.qos.max_sdu),
            "LC3 payload exceeds Max SDU"
        );
        if self.data_path.transparent {
            ensure!(
                self.data_path.codec_configuration.is_empty(),
                "transparent ISO path cannot carry controller codec configuration"
            );
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LeAudioConfiguration {
    pub group_id: i32,
    pub context: AudioContext,
    pub packing: Packing,
    pub pcm: PcmConfiguration,
    pub ases: Vec<AseConfiguration>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BisConfiguration {
    pub bis_index: u8,
    pub stream_handle: Option<u16>,
    pub lc3: Lc3Configuration,
    pub data_path: DataPathConfiguration,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BroadcastSubgroup {
    pub codec: Lc3Configuration,
    pub metadata: Vec<u8>,
    pub bis: Vec<BisConfiguration>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BroadcastConfiguration {
    pub broadcast_id: u32,
    pub encrypted: bool,
    pub presentation_delay_us: u32,
    pub pcm: PcmConfiguration,
    pub subgroups: Vec<BroadcastSubgroup>,
}

impl BroadcastConfiguration {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.broadcast_id <= 0x00ff_ffff,
            "broadcast id is a 24-bit value"
        );
        ensure!(
            self.presentation_delay_us > 0,
            "broadcast presentation delay must be non-zero"
        );
        self.pcm.validate()?;
        ensure!(
            !self.subgroups.is_empty(),
            "broadcast requires at least one subgroup"
        );
        let mut seen_bis = [false; 32];
        for subgroup in &self.subgroups {
            subgroup.codec.validate()?;
            ensure!(
                !subgroup.bis.is_empty(),
                "broadcast subgroup requires at least one BIS"
            );
            for bis in &subgroup.bis {
                ensure!(
                    (1..=31).contains(&bis.bis_index),
                    "BIS index must be 1..=31"
                );
                ensure!(
                    !seen_bis[bis.bis_index as usize],
                    "duplicate BIS index {}",
                    bis.bis_index
                );
                seen_bis[bis.bis_index as usize] = true;
                bis.lc3.validate()?;
                ensure!(
                    bis.lc3 == subgroup.codec,
                    "BIS codec must match its subgroup codec"
                );
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AudioConfiguration {
    Pcm(PcmConfiguration),
    LeAudio(LeAudioConfiguration),
    LeBroadcast(BroadcastConfiguration),
}

impl AudioConfiguration {
    pub fn validate_for(&self, session_type: SessionType) -> Result<()> {
        match self {
            Self::Pcm(pcm) => {
                pcm.validate()?;
                if session_type.is_offloaded() {
                    bail!("hardware-offload session requires a negotiated LE Audio configuration");
                }
            }
            Self::LeAudio(le) => {
                ensure!(
                    !matches!(
                        session_type,
                        SessionType::LeBroadcastSoftwareEncoding
                            | SessionType::LeBroadcastHardwareEncoding
                    ),
                    "unicast LE Audio configuration cannot configure a broadcast session"
                );
                ensure!(le.group_id >= 0, "LE Audio group id must be non-negative");
                le.pcm.validate()?;
                ensure!(
                    !le.ases.is_empty(),
                    "LE Audio session requires at least one ASE"
                );
                for ase in &le.ases {
                    ase.validate()?;
                }
            }
            Self::LeBroadcast(broadcast) => {
                ensure!(
                    matches!(
                        session_type,
                        SessionType::LeBroadcastSoftwareEncoding
                            | SessionType::LeBroadcastHardwareEncoding
                    ),
                    "broadcast configuration requires a broadcast session"
                );
                broadcast.validate()?;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PresentationPosition {
    pub remote_delay_ns: u64,
    pub transmitted_octets: u64,
    pub monotonic_time_ns: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum PayloadKind {
    Pcm = 1,
    Lc3Sdu = 2,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum IsoStatus {
    Valid = 0,
    PossiblyInvalid = 1,
    Lost = 2,
    Late = 3,
}

/// Fixed-size descriptor stored immediately before each shared-memory payload.
/// Never replace this with a Rust enum layout: this structure is a process ABI.
#[derive(Clone, Copy, Eq, PartialEq)]
#[repr(C)]
pub struct FrameDescriptor {
    pub abi_version: u16,
    pub header_bytes: u16,
    pub payload_kind: u8,
    pub iso_status: u8,
    pub flags: u16,
    pub payload_bytes: u32,
    pub sequence: u64,
    pub stream_handle: u32,
    pub sample_rate_hz: u32,
    pub channels: u16,
    pub bits_per_sample: u16,
    pub capture_time_ns: u64,
    pub presentation_time_ns: u64,
    pub deadline_ns: u64,
}

const _: () = assert!(std::mem::size_of::<FrameDescriptor>() == 64);

impl FrameDescriptor {
    pub fn new(kind: PayloadKind, sequence: u64) -> Self {
        Self {
            abi_version: WIRELESS_AUDIO_ABI_VERSION,
            header_bytes: std::mem::size_of::<Self>() as u16,
            payload_kind: kind as u8,
            iso_status: IsoStatus::Valid as u8,
            flags: 0,
            payload_bytes: 0,
            sequence,
            stream_handle: 0,
            sample_rate_hz: 0,
            channels: 0,
            bits_per_sample: 0,
            capture_time_ns: 0,
            presentation_time_ns: 0,
            deadline_ns: 0,
        }
    }
}

impl fmt::Debug for FrameDescriptor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FrameDescriptor")
            .field("kind", &self.payload_kind)
            .field("sequence", &self.sequence)
            .field("payload_bytes", &self.payload_bytes)
            .field("stream_handle", &self.stream_handle)
            .field("presentation_time_ns", &self.presentation_time_ns)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pcm_interval_is_exact() {
        let pcm = PcmConfiguration {
            sample_rate_hz: 48_000,
            bits_per_sample: 16,
            channels: 2,
            data_interval_us: 10_000,
        };
        assert_eq!(pcm.bytes_per_interval().unwrap(), 1_920);
    }

    #[test]
    fn offload_rejects_pcm_only_contract() {
        let config = AudioConfiguration::Pcm(PcmConfiguration {
            sample_rate_hz: 48_000,
            bits_per_sample: 16,
            channels: 2,
            data_interval_us: 10_000,
        });
        assert!(
            config
                .validate_for(SessionType::LeUnicastHardwareEncoding)
                .is_err()
        );
    }

    #[test]
    fn broadcast_rejects_duplicate_bis_indices() {
        let lc3 = Lc3Configuration {
            sampling_frequency_hz: 48_000,
            frame_duration_us: 10_000,
            octets_per_codec_frame: 120,
            codec_frames_per_sdu: 1,
            audio_channel_allocation: 3,
        };
        let data_path = DataPathConfiguration {
            data_path_id: 0,
            transparent: true,
            controller_delay_us: 0,
            codec_configuration: vec![],
            vendor_configuration: vec![],
        };
        let broadcast = BroadcastConfiguration {
            broadcast_id: 1,
            encrypted: false,
            presentation_delay_us: 40_000,
            pcm: PcmConfiguration {
                sample_rate_hz: 48_000,
                bits_per_sample: 16,
                channels: 2,
                data_interval_us: 10_000,
            },
            subgroups: vec![BroadcastSubgroup {
                codec: lc3,
                metadata: vec![],
                bis: vec![
                    BisConfiguration {
                        bis_index: 1,
                        stream_handle: None,
                        lc3,
                        data_path: data_path.clone(),
                    },
                    BisConfiguration {
                        bis_index: 1,
                        stream_handle: None,
                        lc3,
                        data_path,
                    },
                ],
            }],
        };
        assert!(broadcast.validate().is_err());
    }
}

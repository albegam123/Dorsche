use anyhow::{Result, bail};

use super::types::{AudioContext, Lc3Configuration, SessionType, StreamDirection};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodecCapability {
    pub sampling_frequencies_hz: Vec<u32>,
    pub frame_durations_us: Vec<u16>,
    pub octets_per_frame_min: u16,
    pub octets_per_frame_max: u16,
    pub max_codec_frames_per_sdu: u8,
    pub channel_allocations: u32,
}

impl CodecCapability {
    pub fn supports(&self, codec: Lc3Configuration) -> bool {
        self.sampling_frequencies_hz
            .contains(&codec.sampling_frequency_hz)
            && self.frame_durations_us.contains(&codec.frame_duration_us)
            && (self.octets_per_frame_min..=self.octets_per_frame_max)
                .contains(&codec.octets_per_codec_frame)
            && codec.codec_frames_per_sdu <= self.max_codec_frames_per_sdu
            && codec.audio_channel_allocation & !self.channel_allocations == 0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderCapabilities {
    pub name: String,
    pub session_types: Vec<SessionType>,
    pub codecs: Vec<CodecCapability>,
    pub max_streams: u8,
    pub supports_asymmetric: bool,
    pub supports_multidirectional: bool,
    pub supports_transparent_iso: bool,
    pub supports_controller_codec: bool,
}

impl ProviderCapabilities {
    pub fn supports(&self, request: &SessionRequest) -> bool {
        self.session_types.contains(&request.session_type)
            && self
                .codecs
                .iter()
                .any(|capability| capability.supports(request.codec))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionRequest {
    pub session_type: SessionType,
    pub direction: StreamDirection,
    pub context: AudioContext,
    pub codec: Lc3Configuration,
    pub require_multidirectional: bool,
}

/// Hardware/DSP/controller capability discovery is a plugin boundary. It is
/// intentionally synchronous: discovery runs on the provider actor, never on
/// a real-time media callback.
pub trait CapabilityProvider: Send {
    fn capabilities(&self) -> &ProviderCapabilities;

    fn accepts(&self, request: &SessionRequest) -> bool {
        self.capabilities().supports(request)
            && (!request.require_multidirectional || self.capabilities().supports_multidirectional)
    }
}

pub struct CapabilityRegistry {
    providers: Vec<Box<dyn CapabilityProvider>>,
}

impl CapabilityRegistry {
    pub fn new() -> Self {
        Self {
            providers: Vec::new(),
        }
    }

    pub fn register(&mut self, provider: impl CapabilityProvider + 'static) {
        self.providers.push(Box::new(provider));
    }

    /// Select in registration order. Platform policy registers controller/DSP
    /// offload before software when power wins, or software first when AI needs
    /// direct PCM/LC3 visibility. No hidden fallback changes SessionType.
    pub fn select(&self, request: &SessionRequest) -> Result<&ProviderCapabilities> {
        self.providers
            .iter()
            .find(|provider| provider.accepts(request))
            .map(|provider| provider.capabilities())
            .ok_or_else(|| anyhow::anyhow!("no provider accepts {request:?}"))
    }

    pub fn require_explicit_fallback(
        &self,
        preferred: &SessionRequest,
        fallback: Option<&SessionRequest>,
    ) -> Result<&ProviderCapabilities> {
        match self.select(preferred) {
            Ok(provider) => Ok(provider),
            Err(_) => match fallback {
                Some(fallback) => self.select(fallback),
                None => bail!("preferred provider unavailable and no explicit fallback supplied"),
            },
        }
    }
}

impl Default for CapabilityRegistry {
    fn default() -> Self {
        Self::new()
    }
}

pub struct SoftwareLc3Provider {
    capabilities: ProviderCapabilities,
}

impl SoftwareLc3Provider {
    pub fn new() -> Self {
        Self {
            capabilities: ProviderCapabilities {
                name: "floss-host-lc3".into(),
                session_types: vec![
                    SessionType::LeUnicastSoftwareEncoding,
                    SessionType::LeUnicastSoftwareDecoding,
                ],
                codecs: vec![CodecCapability {
                    sampling_frequencies_hz: vec![8_000, 16_000, 24_000, 32_000, 44_100, 48_000],
                    frame_durations_us: vec![7_500, 10_000],
                    octets_per_frame_min: 20,
                    octets_per_frame_max: 400,
                    max_codec_frames_per_sdu: 4,
                    channel_allocations: u32::MAX,
                }],
                max_streams: 31,
                supports_asymmetric: true,
                supports_multidirectional: true,
                supports_transparent_iso: true,
                supports_controller_codec: false,
            },
        }
    }
}

impl Default for SoftwareLc3Provider {
    fn default() -> Self {
        Self::new()
    }
}

impl CapabilityProvider for SoftwareLc3Provider {
    fn capabilities(&self) -> &ProviderCapabilities {
        &self.capabilities
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lc3() -> Lc3Configuration {
        Lc3Configuration {
            sampling_frequency_hz: 48_000,
            frame_duration_us: 10_000,
            octets_per_codec_frame: 120,
            codec_frames_per_sdu: 1,
            audio_channel_allocation: 3,
        }
    }

    #[test]
    fn software_and_offload_are_never_implicitly_interchanged() {
        let mut registry = CapabilityRegistry::new();
        registry.register(SoftwareLc3Provider::new());
        let offload = SessionRequest {
            session_type: SessionType::LeUnicastHardwareEncoding,
            direction: StreamDirection::Sink,
            context: AudioContext::Media,
            codec: lc3(),
            require_multidirectional: false,
        };
        assert!(registry.select(&offload).is_err());

        let software = SessionRequest {
            session_type: SessionType::LeUnicastSoftwareEncoding,
            ..offload
        };
        assert_eq!(registry.select(&software).unwrap().name, "floss-host-lc3");
        assert_eq!(
            registry
                .require_explicit_fallback(&offload, Some(&software))
                .unwrap()
                .name,
            "floss-host-lc3"
        );
    }
}

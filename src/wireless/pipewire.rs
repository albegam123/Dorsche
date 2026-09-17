//! Optional northbound PipeWire adapter.
//!
//! PipeWire owns graph scheduling and SPA buffers; it never owns BAP/ASE/ISO
//! state. The callback does no allocation and only moves one negotiated PCM
//! quantum between a SPA buffer and Dorsche's shared SPSC ring.

use std::io::Cursor;

use anyhow::{Context as _, Result, ensure};
use pipewire as pw;
use pw::{properties::properties, spa};
use spa::pod::Pod;

use super::{
    data_plane::{Consumer, Producer},
    types::{FrameDescriptor, PayloadKind, PcmConfiguration},
};

pub struct PipeWireAdapter;

impl PipeWireAdapter {
    /// Verify the native SPA connection and PCM negotiation without requiring
    /// a Bluetooth controller. The loop exits after PipeWire accepts the node.
    pub fn probe_server(pcm: PcmConfiguration) -> Result<()> {
        pcm.validate()?;
        pw::init();
        let mainloop = pw::main_loop::MainLoopRc::new(None)?;
        let context = pw::context::ContextRc::new(&mainloop, None)?;
        let core = context.connect_rc(None)?;
        let stream = pw::stream::StreamBox::new(
            &core,
            "dorsche-pipewire-probe",
            properties! {
                *pw::keys::APP_NAME => "Dorsche",
                *pw::keys::NODE_NAME => "dorsche.pipewire.probe",
                *pw::keys::NODE_VIRTUAL => "true",
                *pw::keys::MEDIA_TYPE => "Audio",
                *pw::keys::MEDIA_CLASS => "Audio/Source",
            },
        )?;
        let loop_for_state = mainloop.clone();
        let _listener = stream
            .add_local_listener::<()>()
            .state_changed(move |_, _, _, state| {
                if matches!(
                    state,
                    pw::stream::StreamState::Paused | pw::stream::StreamState::Streaming
                ) {
                    loop_for_state.quit();
                }
            })
            .register()?;
        let values = format_pod(pcm)?;
        let mut params = [Pod::from_bytes(&values).context("decode PipeWire format pod")?];
        stream.connect(
            spa::utils::Direction::Output,
            None,
            pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )?;
        mainloop.run();
        Ok(())
    }

    /// Publish wireless microphone PCM as a normal Linux `Audio/Source`.
    /// This call owns and runs a PipeWire main loop on the current thread.
    pub fn run_source(node_name: &str, pcm: PcmConfiguration, consumer: Consumer) -> Result<()> {
        pcm.validate()?;
        pw::init();
        let mainloop = pw::main_loop::MainLoopRc::new(None)?;
        let context = pw::context::ContextRc::new(&mainloop, None)?;
        let core = context.connect_rc(None)?;
        let stride = bytes_per_frame(pcm)?;

        let stream = pw::stream::StreamBox::new(
            &core,
            node_name,
            properties! {
                *pw::keys::APP_NAME => "Dorsche",
                *pw::keys::NODE_NAME => node_name,
                *pw::keys::NODE_VIRTUAL => "true",
                *pw::keys::MEDIA_TYPE => "Audio",
                *pw::keys::MEDIA_CATEGORY => "Capture",
                *pw::keys::MEDIA_ROLE => "Communication",
                *pw::keys::MEDIA_CLASS => "Audio/Source",
            },
        )?;

        let _listener = stream
            .add_local_listener_with_user_data(consumer)
            .process(move |stream, consumer| {
                let Some(mut pw_buffer) = stream.dequeue_buffer() else {
                    return;
                };
                let Some(data) = pw_buffer.datas_mut().first_mut() else {
                    return;
                };

                let copied = match (data.data(), consumer.try_acquire()) {
                    (Some(destination), Ok(Some(frame))) => {
                        let descriptor = frame.descriptor();
                        if descriptor.payload_kind != PayloadKind::Pcm as u8
                            || descriptor.sample_rate_hz != pcm.sample_rate_hz
                            || descriptor.channels != u16::from(pcm.channels)
                            || descriptor.bits_per_sample != u16::from(pcm.bits_per_sample)
                        {
                            0
                        } else {
                            let bytes = destination.len().min(frame.payload().len());
                            destination[..bytes].copy_from_slice(&frame.payload()[..bytes]);
                            bytes
                        }
                    }
                    _ => 0,
                };
                let chunk = data.chunk_mut();
                *chunk.offset_mut() = 0;
                *chunk.stride_mut() = stride as i32;
                *chunk.size_mut() = copied as u32;
            })
            .register()?;

        let values = format_pod(pcm)?;
        let mut params = [Pod::from_bytes(&values).context("decode PipeWire format pod")?];
        stream.connect(
            spa::utils::Direction::Output,
            None,
            pw::stream::StreamFlags::MAP_BUFFERS | pw::stream::StreamFlags::RT_PROCESS,
            &mut params,
        )?;
        mainloop.run();
        Ok(())
    }

    /// Expose a normal Linux `Audio/Sink` whose PCM feeds a Dorsche session.
    /// The wireless protocol and codec configuration remain entirely outside
    /// this adapter, so PipeWire can be omitted on headless AI appliances.
    pub fn run_sink(node_name: &str, pcm: PcmConfiguration, producer: Producer) -> Result<()> {
        pcm.validate()?;
        pw::init();
        let mainloop = pw::main_loop::MainLoopRc::new(None)?;
        let context = pw::context::ContextRc::new(&mainloop, None)?;
        let core = context.connect_rc(None)?;

        struct State {
            producer: Producer,
            sequence: u64,
            pcm: PcmConfiguration,
        }
        let state = State {
            producer,
            sequence: 0,
            pcm,
        };
        let stream = pw::stream::StreamBox::new(
            &core,
            node_name,
            properties! {
                *pw::keys::APP_NAME => "Dorsche",
                *pw::keys::NODE_NAME => node_name,
                *pw::keys::NODE_VIRTUAL => "true",
                *pw::keys::MEDIA_TYPE => "Audio",
                *pw::keys::MEDIA_CATEGORY => "Playback",
                *pw::keys::MEDIA_ROLE => "Communication",
                *pw::keys::MEDIA_CLASS => "Audio/Sink",
            },
        )?;

        let _listener = stream
            .add_local_listener_with_user_data(state)
            .process(|stream, state| {
                let Some(mut pw_buffer) = stream.dequeue_buffer() else {
                    return;
                };
                let Some(data) = pw_buffer.datas_mut().first_mut() else {
                    return;
                };
                let offset = data.chunk().offset() as usize;
                let size = data.chunk().size() as usize;
                let Some(source) = data.data() else {
                    return;
                };
                let Some(end) = offset.checked_add(size).filter(|end| *end <= source.len()) else {
                    return;
                };
                let Some(mut reservation) = state.producer.try_reserve() else {
                    return;
                };
                if size > reservation.capacity() {
                    return;
                }
                reservation.payload_mut()[..size].copy_from_slice(&source[offset..end]);
                let mut descriptor = FrameDescriptor::new(PayloadKind::Pcm, state.sequence);
                descriptor.sample_rate_hz = state.pcm.sample_rate_hz;
                descriptor.channels = u16::from(state.pcm.channels);
                descriptor.bits_per_sample = u16::from(state.pcm.bits_per_sample);
                descriptor.capture_time_ns = monotonic_time_ns();
                if reservation.commit(descriptor, size).is_ok() {
                    state.sequence = state.sequence.wrapping_add(1);
                }
            })
            .register()?;

        let values = format_pod(pcm)?;
        let mut params = [Pod::from_bytes(&values).context("decode PipeWire format pod")?];
        stream.connect(
            spa::utils::Direction::Input,
            None,
            pw::stream::StreamFlags::MAP_BUFFERS | pw::stream::StreamFlags::RT_PROCESS,
            &mut params,
        )?;
        mainloop.run();
        Ok(())
    }
}

fn format_pod(pcm: PcmConfiguration) -> Result<Vec<u8>> {
    let mut audio = spa::param::audio::AudioInfoRaw::new();
    audio.set_format(match pcm.bits_per_sample {
        16 => spa::param::audio::AudioFormat::S16LE,
        24 => spa::param::audio::AudioFormat::S24LE,
        32 => spa::param::audio::AudioFormat::S32LE,
        _ => unreachable!("validated by PcmConfiguration"),
    });
    audio.set_rate(pcm.sample_rate_hz);
    audio.set_channels(u32::from(pcm.channels));

    let object = spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: audio.into(),
    };
    let serialized = spa::pod::serialize::PodSerializer::serialize(
        Cursor::new(Vec::new()),
        &spa::pod::Value::Object(object),
    )
    .context("serialize PipeWire PCM format")?;
    Ok(serialized.0.into_inner())
}

fn bytes_per_frame(pcm: PcmConfiguration) -> Result<usize> {
    let sample_bytes = usize::from(pcm.bits_per_sample / 8);
    let stride = sample_bytes
        .checked_mul(usize::from(pcm.channels))
        .context("PipeWire PCM stride overflow")?;
    ensure!(stride > 0, "PipeWire PCM stride is zero");
    Ok(stride)
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

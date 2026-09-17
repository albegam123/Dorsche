use std::{collections::VecDeque, sync::Arc, time::Duration};

use anyhow::{Context, Result, anyhow};
use tokio::{
    sync::{mpsc, oneshot, watch},
    time::{self, MissedTickBehavior},
};

use crate::bridge::ffi;

use super::frame::{AudioFrame, SharedPcmFrame};

const FRAME_SAMPLES: usize = 160;
const SAMPLE_RATE: u32 = 16_000;

struct CaptureRequest {
    reply: oneshot::Sender<Result<cxx::UniquePtr<ffi::HardwareFrame>, String>>,
}

struct HardwarePlaybackRequest {
    pcm: Arc<[f32]>,
    sequence: u64,
    reply: oneshot::Sender<ffi::PlaybackReceipt>,
}

#[derive(Clone)]
pub struct ProxyHandle {
    capture_tx: mpsc::Sender<CaptureRequest>,
    playback_tx: mpsc::Sender<HardwarePlaybackRequest>,
}

impl ProxyHandle {
    async fn capture(&self) -> Result<cxx::UniquePtr<ffi::HardwareFrame>> {
        let (reply, receive) = oneshot::channel();
        self.capture_tx
            .send(CaptureRequest { reply })
            .await
            .map_err(|_| anyhow!("controller proxy stopped"))?;
        receive
            .await
            .context("controller dropped capture reply")?
            .map_err(|message| anyhow!(message))
    }

    async fn play(&self, pcm: Arc<[f32]>, sequence: u64) -> Result<ffi::PlaybackReceipt> {
        let (reply, receive) = oneshot::channel();
        self.playback_tx
            .send(HardwarePlaybackRequest {
                pcm,
                sequence,
                reply,
            })
            .await
            .map_err(|_| anyhow!("controller proxy stopped"))?;
        receive.await.context("controller dropped playback reply")
    }
}

/// Build the C++ controller proxy actor and return its capability handle.
///
/// There are deliberately two inboxes. `biased` makes speaker submissions win
/// over capture requests, bounding full-duplex control latency without a lock
/// or shared priority queue.
pub fn run_controller_proxy(
    mut shutdown: watch::Receiver<bool>,
) -> (ProxyHandle, tokio::task::JoinHandle<Result<()>>) {
    let (capture_tx, mut capture_rx) = mpsc::channel::<CaptureRequest>(2);
    let (playback_tx, mut playback_rx) = mpsc::channel::<HardwarePlaybackRequest>(8);
    let handle = ProxyHandle {
        capture_tx,
        playback_tx,
    };

    let task = tokio::spawn(async move {
        let mut controller = ffi::new_hardware_controller();
        if controller.is_null() {
            return Err(anyhow!("failed to construct C++ hardware controller"));
        }

        loop {
            tokio::select! {
                biased;
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() { break; }
                }
                Some(request) = playback_rx.recv() => {
                    // cxx exposes a synchronous borrow: C++ cannot retain pcm.
                    let receipt = controller.pin_mut().play_pcm(
                        request.pcm.as_ref(), request.sequence,
                    );
                    let _ = request.reply.send(receipt);
                }
                Some(request) = capture_rx.recv() => {
                    let result = controller
                        .pin_mut()
                        .capture_next()
                        .map_err(|error| error.to_string());
                    let _ = request.reply.send(result);
                }
                else => break,
            }
        }
        Ok(())
    });
    (handle, task)
}

/// Actor 1: clocks hardware capture and transfers frame ownership downstream.
pub async fn run_microphone_capture(
    proxy: ProxyHandle,
    frame_tx: mpsc::Sender<AudioFrame>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let mut clock = time::interval(Duration::from_millis(10));
    clock.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() { break; }
            }
            _ = clock.tick() => {
                let raw = proxy.capture().await?;
                let frame = SharedPcmFrame::from_cpp(raw).await?;
                // This send moves one Arc capability, never the mapped samples.
                // A bounded edge turns downstream slowness into backpressure.
                if frame_tx.send(frame).await.is_err() { break; }
            }
        }
    }
    Ok(())
}

pub enum PlaybackCommand {
    Start {
        utterance_id: u64,
        frames: VecDeque<Arc<[f32]>>,
    },
}

#[doc(hidden)]
pub struct EchoReference {
    pcm: Arc<[f32]>,
    playback_sequence: u64,
    presentation_time_ns: u64,
}

#[doc(hidden)]
pub struct BargeIn {
    microphone_sequence: u64,
}

/// Actor 3: consumes microphone audio and a dedicated high-priority echo edge.
pub async fn run_vad_ai(
    mut microphone_rx: mpsc::Receiver<AudioFrame>,
    mut echo_rx: mpsc::Receiver<EchoReference>,
    barge_tx: mpsc::Sender<BargeIn>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let mut latest_echo: Option<EchoReference> = None;
    let mut speech_active = false;

    loop {
        tokio::select! {
            // Tokio's biased select plus a physically separate bounded channel
            // ensures echo timing metadata is observed before ordinary mic work.
            biased;
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() { break; }
            }
            Some(echo) = echo_rx.recv() => {
                latest_echo = Some(echo);
            }
            Some(frame) = microphone_rx.recv() => {
                let aligned_echo = latest_echo.as_ref().filter(|echo| {
                    frame.capture_time_ns().abs_diff(echo.presentation_time_ns)
                        <= 50_000_000
                });

                // Minimal AEC-shaped VAD: the important property is that both
                // inputs are immutable leases aligned by monotonic timestamps.
                let energy = frame.iter().enumerate().map(|(index, mic)| {
                    let reference = aligned_echo
                        .and_then(|echo| echo.pcm.get(index))
                        .copied()
                        .unwrap_or_default();
                    let residual = mic - 0.20 * reference;
                    residual * residual
                }).sum::<f32>() / frame.len() as f32;
                let speaking = energy.sqrt() > 0.10;

                if speaking && !speech_active && let Some(echo) = aligned_echo {
                    eprintln!(
                        "barge-in: mic={} while playback={} ({} Hz, {})",
                        frame.sequence(),
                        echo.playback_sequence,
                        frame.sample_rate(),
                        if frame.is_dma_buf() { "dma-buf" } else { "memfd" },
                    );
                    // try_send is intentional: one pending interrupt already
                    // represents the newest desired state (stop speaking).
                    let _ = barge_tx.try_send(BargeIn {
                        microphone_sequence: frame.sequence(),
                    });
                }
                speech_active = speaking;
            }
            else => break,
        }
    }
    Ok(())
}

/// Actor 4: submits speaker frames and mirrors each exact Arc into the echo
/// reference track after the hardware assigns a presentation timestamp.
pub async fn run_playback_sink(
    proxy: ProxyHandle,
    mut command_rx: mpsc::Receiver<PlaybackCommand>,
    mut barge_rx: mpsc::Receiver<BargeIn>,
    echo_tx: mpsc::Sender<EchoReference>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let mut current: Option<(u64, VecDeque<Arc<[f32]>>)> = None;
    let mut sequence = 0_u64;
    let mut clock = time::interval(Duration::from_millis(10));
    clock.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            // Interrupt is the first branch and has its own capacity-one inbox:
            // it cannot sit behind a long TTS command queue.
            biased;
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() { break; }
            }
            Some(interrupt) = barge_rx.recv() => {
                if let Some((utterance_id, _)) = current.take() {
                    eprintln!(
                        "playback {} stopped by mic frame {}",
                        utterance_id, interrupt.microphone_sequence,
                    );
                }
            }
            Some(command) = command_rx.recv() => {
                let PlaybackCommand::Start { utterance_id, frames } = command;
                current = Some((utterance_id, frames));
            }
            _ = clock.tick(), if current.is_some() => {
                let next = current.as_mut().and_then(|(_, frames)| frames.pop_front());
                if let Some(pcm) = next {
                    let receipt = proxy.play(Arc::clone(&pcm), sequence).await?;
                    let echo = EchoReference {
                        // Arc clone is an ownership message, not a PCM copy.
                        pcm,
                        playback_sequence: receipt.sequence,
                        presentation_time_ns: receipt.presentation_time_ns,
                    };
                    if echo_tx.send(echo).await.is_err() { break; }
                    sequence += 1;
                } else {
                    current = None;
                }
            }
            else => break,
        }
    }
    Ok(())
}

pub fn make_demo_utterance(milliseconds: usize) -> VecDeque<Arc<[f32]>> {
    let frame_count = milliseconds / 10;
    (0..frame_count)
        .map(|frame| {
            let samples: Arc<[f32]> = (0..FRAME_SAMPLES)
                .map(|index| {
                    let sample = frame * FRAME_SAMPLES + index;
                    0.18 * (std::f32::consts::TAU * 220.0 * sample as f32 / SAMPLE_RATE as f32)
                        .sin()
                })
                .collect::<Vec<_>>()
                .into();
            samples
        })
        .collect()
}

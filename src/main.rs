use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::{mpsc, watch};

use dorsche::topshim::{
    PlaybackCommand, make_demo_utterance, run_controller_proxy, run_microphone_capture,
    run_playback_sink, run_vad_ai,
};

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // Every edge has one producer/consumer role and a deliberately small bound.
    // Backpressure is therefore visible in topology instead of hidden in shared
    // mutable state. No process-global synchronization primitive is required.
    let (microphone_tx, microphone_rx) = mpsc::channel(8);
    let (echo_tx, echo_rx) = mpsc::channel(8);
    let (barge_tx, barge_rx) = mpsc::channel(1);
    let (playback_tx, playback_rx) = mpsc::channel(2);

    let (proxy, proxy_task) = run_controller_proxy(shutdown_rx.clone());
    let capture_task = tokio::spawn(run_microphone_capture(
        proxy.clone(),
        microphone_tx,
        shutdown_rx.clone(),
    ));
    let vad_task = tokio::spawn(run_vad_ai(
        microphone_rx,
        echo_rx,
        barge_tx,
        shutdown_rx.clone(),
    ));
    let sink_task = tokio::spawn(run_playback_sink(
        proxy,
        playback_rx,
        barge_rx,
        echo_tx,
        shutdown_rx.clone(),
    ));

    // A two-second assistant utterance overlaps the synthetic microphone speech
    // burst at 800 ms, exercising the full-duplex barge-in path immediately.
    playback_tx
        .send(PlaybackCommand::Start {
            utterance_id: 1,
            frames: make_demo_utterance(2_000),
        })
        .await
        .context("start demo utterance")?;

    eprintln!("Dorsche running; press Ctrl-C to stop");
    if let Ok(milliseconds) = std::env::var("DORSCHE_DEMO_MS") {
        let milliseconds = milliseconds.parse::<u64>().context("DORSCHE_DEMO_MS")?;
        tokio::time::sleep(Duration::from_millis(milliseconds)).await;
    } else {
        tokio::signal::ctrl_c().await.context("Ctrl-C handler")?;
    }

    let _ = shutdown_tx.send(true);
    drop(playback_tx);

    for task in [proxy_task, capture_task, vad_task, sink_task] {
        task.await.context("actor panicked")??;
    }
    Ok(())
}

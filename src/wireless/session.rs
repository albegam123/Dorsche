use anyhow::{Result, anyhow, bail, ensure};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

use super::types::{AudioConfiguration, PresentationPosition, SessionType};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionState {
    Idle,
    Configured,
    Streaming,
    Suspended,
    Closed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionEvent {
    StateChanged(SessionState),
    ConfigurationChanged(AudioConfiguration),
    PositionChanged(PresentationPosition),
}

enum Command {
    Configure {
        configuration: AudioConfiguration,
        reply: oneshot::Sender<Result<()>>,
    },
    Start(oneshot::Sender<Result<()>>),
    Suspend(oneshot::Sender<Result<()>>),
    Stop(oneshot::Sender<Result<()>>),
    UpdatePosition {
        position: PresentationPosition,
        reply: oneshot::Sender<Result<()>>,
    },
    GetPosition(oneshot::Sender<PresentationPosition>),
    Close(oneshot::Sender<()>),
}

/// A capability handle, not shared mutable session state. Every clone sends to
/// the same single-owner actor, preserving command ordering without a Mutex.
#[derive(Clone)]
pub struct SessionHandle {
    session_type: SessionType,
    commands: mpsc::Sender<Command>,
}

impl SessionHandle {
    pub fn session_type(&self) -> SessionType {
        self.session_type
    }

    pub async fn configure(&self, configuration: AudioConfiguration) -> Result<()> {
        let (reply, receive) = oneshot::channel();
        self.commands
            .send(Command::Configure {
                configuration,
                reply,
            })
            .await
            .map_err(|_| anyhow!("wireless audio session stopped"))?;
        receive
            .await
            .map_err(|_| anyhow!("session dropped configure response"))?
    }

    pub async fn start(&self) -> Result<()> {
        request(&self.commands, Command::Start).await
    }

    pub async fn suspend(&self) -> Result<()> {
        request(&self.commands, Command::Suspend).await
    }

    pub async fn stop(&self) -> Result<()> {
        request(&self.commands, Command::Stop).await
    }

    pub async fn update_position(&self, position: PresentationPosition) -> Result<()> {
        let (reply, receive) = oneshot::channel();
        self.commands
            .send(Command::UpdatePosition { position, reply })
            .await
            .map_err(|_| anyhow!("wireless audio session stopped"))?;
        receive
            .await
            .map_err(|_| anyhow!("session dropped position response"))?
    }

    pub async fn presentation_position(&self) -> Result<PresentationPosition> {
        let (reply, receive) = oneshot::channel();
        self.commands
            .send(Command::GetPosition(reply))
            .await
            .map_err(|_| anyhow!("wireless audio session stopped"))?;
        receive
            .await
            .map_err(|_| anyhow!("session dropped position response"))
    }

    pub async fn close(&self) -> Result<()> {
        let (reply, receive) = oneshot::channel();
        self.commands
            .send(Command::Close(reply))
            .await
            .map_err(|_| anyhow!("wireless audio session stopped"))?;
        receive
            .await
            .map_err(|_| anyhow!("session dropped close response"))
    }
}

async fn request(
    commands: &mpsc::Sender<Command>,
    constructor: fn(oneshot::Sender<Result<()>>) -> Command,
) -> Result<()> {
    let (reply, receive) = oneshot::channel();
    commands
        .send(constructor(reply))
        .await
        .map_err(|_| anyhow!("wireless audio session stopped"))?;
    receive
        .await
        .map_err(|_| anyhow!("session dropped command response"))?
}

/// Start a policy/session state actor. Hardware adapters consume the emitted
/// events and report timing back through `update_position`; the state machine
/// therefore remains testable without Binder, D-Bus, PipeWire, or a controller.
pub fn spawn_session(
    session_type: SessionType,
) -> (
    SessionHandle,
    mpsc::Receiver<SessionEvent>,
    JoinHandle<Result<()>>,
) {
    let (command_tx, mut command_rx) = mpsc::channel(16);
    let (event_tx, event_rx) = mpsc::channel(16);
    let handle = SessionHandle {
        session_type,
        commands: command_tx,
    };

    let task = tokio::spawn(async move {
        let mut state = SessionState::Idle;
        let mut configuration: Option<AudioConfiguration> = None;
        let mut position = PresentationPosition::default();

        while let Some(command) = command_rx.recv().await {
            match command {
                Command::Configure {
                    configuration: next,
                    reply,
                } => {
                    let result = (|| {
                        ensure!(
                            matches!(
                                state,
                                SessionState::Idle
                                    | SessionState::Configured
                                    | SessionState::Suspended
                            ),
                            "cannot configure session in {state:?}"
                        );
                        next.validate_for(session_type)?;
                        configuration = Some(next.clone());
                        state = SessionState::Configured;
                        Ok(())
                    })();
                    if result.is_ok() {
                        emit(&event_tx, SessionEvent::ConfigurationChanged(next)).await?;
                        emit(&event_tx, SessionEvent::StateChanged(state)).await?;
                    }
                    let _ = reply.send(result);
                }
                Command::Start(reply) => {
                    let result = if configuration.is_none() {
                        Err(anyhow!("session has no audio configuration"))
                    } else if !matches!(state, SessionState::Configured | SessionState::Suspended) {
                        Err(anyhow!("cannot start session in {state:?}"))
                    } else {
                        state = SessionState::Streaming;
                        emit(&event_tx, SessionEvent::StateChanged(state)).await?;
                        Ok(())
                    };
                    let _ = reply.send(result);
                }
                Command::Suspend(reply) => {
                    let result = if state != SessionState::Streaming {
                        Err(anyhow!("cannot suspend session in {state:?}"))
                    } else {
                        state = SessionState::Suspended;
                        emit(&event_tx, SessionEvent::StateChanged(state)).await?;
                        Ok(())
                    };
                    let _ = reply.send(result);
                }
                Command::Stop(reply) => {
                    let result = if matches!(state, SessionState::Idle | SessionState::Closed) {
                        Err(anyhow!("cannot stop session in {state:?}"))
                    } else {
                        state = SessionState::Idle;
                        configuration = None;
                        position = PresentationPosition::default();
                        emit(&event_tx, SessionEvent::StateChanged(state)).await?;
                        Ok(())
                    };
                    let _ = reply.send(result);
                }
                Command::UpdatePosition {
                    position: next,
                    reply,
                } => {
                    let result = (|| {
                        ensure!(
                            state == SessionState::Streaming,
                            "position update outside streaming state"
                        );
                        ensure!(
                            next.monotonic_time_ns >= position.monotonic_time_ns,
                            "presentation clock moved backwards"
                        );
                        if session_type.carries_host_payload() {
                            ensure!(
                                next.transmitted_octets >= position.transmitted_octets,
                                "transmitted octet counter moved backwards"
                            );
                        } else if next.transmitted_octets != 0 {
                            bail!("offload sessions must not report host transmitted octets");
                        }
                        position = next;
                        Ok(())
                    })();
                    if result.is_ok() {
                        emit(&event_tx, SessionEvent::PositionChanged(next)).await?;
                    }
                    let _ = reply.send(result);
                }
                Command::GetPosition(reply) => {
                    let _ = reply.send(position);
                }
                Command::Close(reply) => {
                    state = SessionState::Closed;
                    emit(&event_tx, SessionEvent::StateChanged(state)).await?;
                    let _ = reply.send(());
                    break;
                }
            }
        }
        Ok(())
    });

    (handle, event_rx, task)
}

async fn emit(events: &mpsc::Sender<SessionEvent>, event: SessionEvent) -> Result<()> {
    events
        .send(event)
        .await
        .map_err(|_| anyhow!("session event consumer stopped"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wireless::PcmConfiguration;

    fn pcm() -> AudioConfiguration {
        AudioConfiguration::Pcm(PcmConfiguration {
            sample_rate_hz: 48_000,
            bits_per_sample: 16,
            channels: 2,
            data_interval_us: 10_000,
        })
    }

    #[tokio::test]
    async fn enforces_configuration_before_streaming() {
        let (session, mut events, task) = spawn_session(SessionType::LeUnicastSoftwareEncoding);
        assert!(session.start().await.is_err());
        session.configure(pcm()).await.unwrap();
        assert!(matches!(
            events.recv().await,
            Some(SessionEvent::ConfigurationChanged(_))
        ));
        assert_eq!(
            events.recv().await,
            Some(SessionEvent::StateChanged(SessionState::Configured))
        );
        session.start().await.unwrap();
        assert_eq!(
            events.recv().await,
            Some(SessionEvent::StateChanged(SessionState::Streaming))
        );
        session.suspend().await.unwrap();
        assert_eq!(
            events.recv().await,
            Some(SessionEvent::StateChanged(SessionState::Suspended))
        );
        session.close().await.unwrap();
        assert_eq!(
            events.recv().await,
            Some(SessionEvent::StateChanged(SessionState::Closed))
        );
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn rejects_non_monotonic_presentation_position() {
        let (session, mut events, task) = spawn_session(SessionType::LeUnicastSoftwareEncoding);
        session.configure(pcm()).await.unwrap();
        events.recv().await;
        events.recv().await;
        session.start().await.unwrap();
        events.recv().await;
        session
            .update_position(PresentationPosition {
                remote_delay_ns: 20_000_000,
                transmitted_octets: 100,
                monotonic_time_ns: 10,
            })
            .await
            .unwrap();
        events.recv().await;
        assert!(
            session
                .update_position(PresentationPosition {
                    remote_delay_ns: 20_000_000,
                    transmitted_octets: 99,
                    monotonic_time_ns: 11,
                })
                .await
                .is_err()
        );
        session.close().await.unwrap();
        events.recv().await;
        task.await.unwrap().unwrap();
    }
}

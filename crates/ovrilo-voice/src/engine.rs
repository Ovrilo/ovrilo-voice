//! The voice engine: a control task that owns room/device lifecycle plus the DSP thread.
//!
//! Hosts send [`VoiceCommand`]s through [`VoiceEngineHandle`] and read [`VoiceEvent`]s. The engine
//! is authoritative for voice state; a state change is reported only after it has been applied.

use tokio::sync::mpsc;

use crate::ConnectionState;
use crate::audio::{AudioBackend, ProcessorFactory};
use crate::contract::{DeviceId, VoiceCommand, VoiceError, VoiceErrorCode, VoiceEvent};
use crate::dsp::{Dsp, DspCommand};
use crate::transport::{Connected, Transport, TransportEvent, TransportSession};

/// Sends commands to a running engine. Dropping every handle leaves the room and stops the engine.
#[derive(Clone)]
pub struct VoiceEngineHandle {
    commands: mpsc::UnboundedSender<VoiceCommand>,
}

impl VoiceEngineHandle {
    /// Queues a command. Returns false if the engine has stopped.
    pub fn send(&self, command: VoiceCommand) -> bool {
        self.commands.send(command).is_ok()
    }
}

/// Entry point for hosts.
pub struct VoiceEngine;

impl VoiceEngine {
    /// Starts the engine on the current Tokio runtime.
    pub fn spawn<B, T>(
        backend: B,
        transport: T,
        processor: ProcessorFactory,
    ) -> (VoiceEngineHandle, mpsc::UnboundedReceiver<VoiceEvent>)
    where
        B: AudioBackend,
        T: Transport,
    {
        let (commands, command_rx) = mpsc::unbounded_channel();
        let (event_tx, events) = mpsc::unbounded_channel();
        let control = Control {
            backend,
            transport,
            dsp: Dsp::spawn(processor),
            events: event_tx,
            state: ConnectionState::Disconnected,
            session: None,
            transport_events: None,
            muted: false,
            deafened: false,
            input: None,
            output: None,
        };
        tokio::spawn(control.run(command_rx));
        (VoiceEngineHandle { commands }, events)
    }
}

struct Control<B: AudioBackend, T: Transport> {
    backend: B,
    transport: T,
    dsp: Dsp,
    events: mpsc::UnboundedSender<VoiceEvent>,
    state: ConnectionState,
    session: Option<T::Session>,
    transport_events: Option<mpsc::UnboundedReceiver<TransportEvent>>,
    muted: bool,
    deafened: bool,
    input: Option<DeviceId>,
    output: Option<DeviceId>,
}

impl<B: AudioBackend, T: Transport> Control<B, T> {
    async fn run(mut self, mut commands: mpsc::UnboundedReceiver<VoiceCommand>) {
        loop {
            tokio::select! {
                command = commands.recv() => match command {
                    Some(command) => self.handle_command(command).await,
                    None => break,
                },
                Some(event) = next_event(&mut self.transport_events) => self.handle_transport_event(event).await,
            }
        }
        self.leave().await;
    }

    fn emit(&self, event: VoiceEvent) {
        let _ = self.events.send(event);
    }

    fn set_state(&mut self, state: ConnectionState) {
        match self.state.transition(state) {
            Ok(next) if next != self.state => {
                self.state = next;
                self.emit(VoiceEvent::ConnectionStateChanged(next));
            }
            Ok(_) => {}
            Err(error) => {
                self.emit(VoiceError::new(VoiceErrorCode::Internal, error.to_string()).into())
            }
        }
    }

    async fn handle_command(&mut self, command: VoiceCommand) {
        match command {
            VoiceCommand::Join { server_url, token } => self.join(&server_url, &token).await,
            VoiceCommand::Leave => self.leave().await,
            VoiceCommand::SetMicrophoneMuted(muted) => {
                if muted != self.muted {
                    self.muted = muted;
                    self.dsp.send(DspCommand::SetMuted(muted));
                    self.dsp.sync().await;
                    self.sync_session_mute().await;
                    self.emit(VoiceEvent::MicrophoneMutedChanged(muted));
                }
            }
            VoiceCommand::SetDeafened(deafened) => {
                if deafened != self.deafened {
                    self.deafened = deafened;
                    self.dsp.send(DspCommand::SetDeafened(deafened));
                    self.dsp.sync().await;
                    self.sync_session_mute().await;
                    self.emit(VoiceEvent::DeafenedChanged(deafened));
                }
            }
            VoiceCommand::SetInputDevice(device) => {
                self.input = device;
                if self.session.is_some() {
                    self.open_input();
                }
            }
            VoiceCommand::SetOutputDevice(device) => {
                self.output = device;
                if self.session.is_some() {
                    self.open_output();
                }
            }
            VoiceCommand::RefreshDevices => {
                match (self.backend.input_devices(), self.backend.output_devices()) {
                    (Ok(inputs), Ok(outputs)) => {
                        self.emit(VoiceEvent::DevicesChanged { inputs, outputs })
                    }
                    (Err(error), _) | (_, Err(error)) => self.emit(error.into()),
                }
            }
        }
    }

    async fn join(&mut self, server_url: &str, token: &str) {
        if self.state != ConnectionState::Disconnected {
            self.emit(
                VoiceError::new(
                    VoiceErrorCode::InvalidState,
                    "already in a room; leave first",
                )
                .into(),
            );
            return;
        }
        self.set_state(ConnectionState::Connecting);

        match self.transport.connect(server_url, token).await {
            Ok(Connected {
                session,
                events,
                microphone,
            }) => {
                self.session = Some(session);
                self.transport_events = Some(events);
                self.dsp
                    .send(DspCommand::SetMicrophoneSink(Some(microphone)));
                // Devices are opened after connecting so a failed join never holds the microphone.
                self.open_output();
                self.open_input();
                self.sync_session_mute().await;
                self.set_state(ConnectionState::Connected);
            }
            Err(error) => {
                self.emit(error.into());
                self.set_state(ConnectionState::Disconnected);
            }
        }
    }

    /// Opens the selected microphone. On failure the session continues listen-only.
    fn open_input(&mut self) {
        match self.backend.open_input(self.input.as_ref()) {
            Ok(capture) => {
                self.dsp.send(DspCommand::SetCapture(Some(capture)));
                self.emit(VoiceEvent::MicrophoneActiveChanged(true));
            }
            Err(error) => {
                self.dsp.send(DspCommand::SetCapture(None));
                self.emit(error.into());
                self.emit(VoiceEvent::MicrophoneActiveChanged(false));
            }
        }
    }

    /// Opens the selected speaker. On failure the session continues without playout.
    fn open_output(&mut self) {
        match self.backend.open_output(self.output.as_ref()) {
            Ok(playout) => self.dsp.send(DspCommand::SetPlayout(Some(playout))),
            Err(error) => {
                self.dsp.send(DspCommand::SetPlayout(None));
                self.emit(error.into());
            }
        }
    }

    async fn sync_session_mute(&mut self) {
        let effective = self.muted || self.deafened;
        if let Some(session) = self.session.as_mut() {
            session.set_microphone_muted(effective).await;
        }
    }

    async fn leave(&mut self) {
        self.dsp.send(DspCommand::Reset);
        self.transport_events = None;
        if let Some(session) = self.session.take() {
            session.close().await;
        }
        self.set_state(ConnectionState::Disconnected);
    }

    async fn handle_transport_event(&mut self, event: TransportEvent) {
        match event {
            TransportEvent::ParticipantJoined { id, name } => {
                self.emit(VoiceEvent::ParticipantJoined { id, name })
            }
            TransportEvent::ParticipantLeft { id } => {
                self.dsp.send(DspCommand::RemoveRemote(id.clone()));
                self.emit(VoiceEvent::ParticipantLeft { id });
            }
            TransportEvent::RemoteAudioStarted { id, frames } => {
                self.dsp.send(DspCommand::AddRemote(id, frames))
            }
            TransportEvent::RemoteAudioEnded { id } => self.dsp.send(DspCommand::RemoveRemote(id)),
            TransportEvent::Reconnecting => self.set_state(ConnectionState::Reconnecting),
            TransportEvent::Reconnected => self.set_state(ConnectionState::Connected),
            TransportEvent::Disconnected { reason } => {
                self.emit(VoiceError::transport(format!("disconnected: {reason}")).into());
                self.leave().await;
            }
        }
    }
}

async fn next_event(
    events: &mut Option<mpsc::UnboundedReceiver<TransportEvent>>,
) -> Option<TransportEvent> {
    match events {
        Some(events) => events.recv().await,
        None => std::future::pending().await,
    }
}

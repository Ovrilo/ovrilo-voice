//! Engine behavior with fake devices and a fake transport (no audio hardware or network needed).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ovrilo_voice::audio::FRAME_SAMPLES;
use ovrilo_voice::{
    AudioBackend, AudioDevice, CaptureStream, Connected, ConnectionState, DeviceId, Frame,
    NetworkStats, NoProcessing, ParticipantId, PlayoutStream, Transport, TransportEvent,
    TransportSession, VoiceCommand, VoiceEngine, VoiceEngineHandle, VoiceError, VoiceErrorCode,
    VoiceEvent,
};
use tokio::sync::mpsc;

// ---------- fakes ----------

#[derive(Clone, Default)]
struct Devices {
    capture: Arc<Mutex<VecDeque<Frame>>>,
    played: Arc<Mutex<Vec<Frame>>>,
    input_fails: bool,
}

struct FakeBackend(Devices);

impl AudioBackend for FakeBackend {
    fn input_devices(&self) -> Result<Vec<AudioDevice>, VoiceError> {
        Ok(vec![AudioDevice {
            id: DeviceId("mic".into()),
            name: "Fake mic".into(),
            is_default: true,
        }])
    }

    fn output_devices(&self) -> Result<Vec<AudioDevice>, VoiceError> {
        Ok(vec![AudioDevice {
            id: DeviceId("spk".into()),
            name: "Fake speaker".into(),
            is_default: true,
        }])
    }

    fn open_input(
        &mut self,
        _device: Option<&DeviceId>,
    ) -> Result<Box<dyn CaptureStream>, VoiceError> {
        if self.0.input_fails {
            return Err(VoiceError::device("microphone is in use"));
        }
        Ok(Box::new(FakeCapture(self.0.capture.clone())))
    }

    fn open_output(
        &mut self,
        _device: Option<&DeviceId>,
    ) -> Result<Box<dyn PlayoutStream>, VoiceError> {
        Ok(Box::new(FakePlayout {
            played: self.0.played.clone(),
            started: Instant::now(),
            written: 0,
        }))
    }
}

struct FakeCapture(Arc<Mutex<VecDeque<Frame>>>);

impl CaptureStream for FakeCapture {
    fn read_frame(&mut self, out: &mut Frame) -> bool {
        match self.0.lock().unwrap().pop_front() {
            Some(frame) => {
                *out = frame;
                true
            }
            None => false,
        }
    }
}

/// Simulates a device consuming 10 ms per frame in real time.
struct FakePlayout {
    played: Arc<Mutex<Vec<Frame>>>,
    started: Instant,
    written: u64,
}

impl PlayoutStream for FakePlayout {
    fn write_frame(&mut self, frame: &Frame) {
        self.played.lock().unwrap().push(*frame);
        self.written += 1;
    }

    fn buffered_ms(&self) -> u32 {
        let elapsed = self.started.elapsed().as_millis() as u64;
        (self.written * 10).saturating_sub(elapsed) as u32
    }
}

#[derive(Default)]
struct Room {
    events: Option<mpsc::UnboundedSender<TransportEvent>>,
    microphone: Option<mpsc::Receiver<Frame>>,
    mute_calls: Vec<bool>,
    closed: bool,
}

#[derive(Clone, Default)]
struct FakeTransport {
    room: Arc<Mutex<Room>>,
    fail: bool,
}

struct FakeSession(Arc<Mutex<Room>>);

impl Transport for FakeTransport {
    type Session = FakeSession;

    async fn connect(
        &self,
        _url: &str,
        _token: &str,
    ) -> Result<Connected<FakeSession>, VoiceError> {
        if self.fail {
            return Err(VoiceError::transport("SFU unreachable"));
        }
        let (event_tx, events) = mpsc::unbounded_channel();
        let (microphone, mic_rx) = mpsc::channel(100);
        let mut room = self.room.lock().unwrap();
        room.events = Some(event_tx);
        room.microphone = Some(mic_rx);
        room.closed = false;
        Ok(Connected {
            session: FakeSession(self.room.clone()),
            events,
            microphone,
        })
    }
}

impl TransportSession for FakeSession {
    async fn set_microphone_muted(&mut self, muted: bool) {
        self.0.lock().unwrap().mute_calls.push(muted);
    }

    async fn close(self) {
        self.0.lock().unwrap().closed = true;
    }

    async fn network_stats(&self) -> NetworkStats {
        let mut stats = NetworkStats::default();
        stats.round_trip_ms = Some(42);
        stats.inbound_concealed_ms = 30;
        stats
    }
}

// ---------- helpers ----------

struct Harness {
    handle: VoiceEngineHandle,
    events: mpsc::UnboundedReceiver<VoiceEvent>,
    devices: Devices,
    transport: FakeTransport,
}

fn start(devices: Devices, transport: FakeTransport) -> Harness {
    let (handle, events) = VoiceEngine::spawn(
        FakeBackend(devices.clone()),
        transport.clone(),
        Box::new(|| Box::new(NoProcessing)),
    );
    Harness {
        handle,
        events,
        devices,
        transport,
    }
}

impl Harness {
    fn send(&self, command: VoiceCommand) {
        assert!(self.handle.send(command));
    }

    /// Next event other than the periodic [`VoiceEvent::StatsUpdated`].
    async fn next(&mut self) -> VoiceEvent {
        loop {
            match self.next_any().await {
                VoiceEvent::StatsUpdated(_) => continue,
                event => return event,
            }
        }
    }

    async fn next_any(&mut self) -> VoiceEvent {
        tokio::time::timeout(Duration::from_secs(2), self.events.recv())
            .await
            .expect("timed out waiting for an engine event")
            .expect("engine stopped")
    }

    async fn expect(&mut self, expected: &[VoiceEvent]) {
        for want in expected {
            assert_eq!(&self.next().await, want);
        }
    }

    async fn expect_error(&mut self, code: VoiceErrorCode) {
        match self.next().await {
            VoiceEvent::Error { code: actual, .. } => assert_eq!(actual, code),
            other => panic!("expected {code:?} error, got {other:?}"),
        }
    }

    async fn join(&mut self) {
        self.send(VoiceCommand::Join {
            server_url: "ws://sfu".into(),
            token: "t".into(),
        });
        self.expect(&[
            VoiceEvent::ConnectionStateChanged(ConnectionState::Connecting),
            VoiceEvent::MicrophoneActiveChanged(true),
            VoiceEvent::ConnectionStateChanged(ConnectionState::Connected),
        ])
        .await;
    }

    fn room_event(&self, event: TransportEvent) {
        self.transport
            .room
            .lock()
            .unwrap()
            .events
            .as_ref()
            .unwrap()
            .send(event)
            .unwrap();
    }

    fn take_microphone(&self) -> mpsc::Receiver<Frame> {
        self.transport
            .room
            .lock()
            .unwrap()
            .microphone
            .take()
            .unwrap()
    }

    fn speak(&self, value: i16, frames: usize) {
        let mut capture = self.devices.capture.lock().unwrap();
        for _ in 0..frames {
            capture.push_back([value; FRAME_SAMPLES]);
        }
    }

    async fn wait_until(&self, what: &str, condition: impl Fn(&Harness) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !condition(self) {
            assert!(Instant::now() < deadline, "timed out waiting until {what}");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}

// ---------- tests ----------

#[tokio::test]
async fn join_publishes_microphone_audio() {
    let mut h = start(Devices::default(), FakeTransport::default());
    h.join().await;
    let mut microphone = h.take_microphone();

    h.speak(7, 3);
    let frame = tokio::time::timeout(Duration::from_secs(2), microphone.recv())
        .await
        .unwrap()
        .unwrap();

    assert!(frame.iter().all(|&s| s == 7));
}

#[tokio::test]
async fn muted_microphone_sends_nothing_and_signals_mute() {
    let mut h = start(Devices::default(), FakeTransport::default());
    h.join().await;
    let mut microphone = h.take_microphone();

    h.send(VoiceCommand::SetMicrophoneMuted(true));
    h.expect(&[VoiceEvent::MicrophoneMutedChanged(true)]).await;
    h.speak(7, 5);
    tokio::time::sleep(Duration::from_millis(100)).await;

    assert!(microphone.try_recv().is_err());
    assert_eq!(
        h.transport.room.lock().unwrap().mute_calls.last(),
        Some(&true)
    );
    assert!(
        h.devices.capture.lock().unwrap().is_empty(),
        "capture is still drained while muted"
    );
}

#[tokio::test]
async fn remote_participants_are_mixed_into_playout() {
    let mut h = start(Devices::default(), FakeTransport::default());
    h.join().await;
    let (a_tx, a_rx) = mpsc::channel(20);
    let (b_tx, b_rx) = mpsc::channel(20);

    h.room_event(TransportEvent::ParticipantJoined {
        id: ParticipantId("a".into()),
        name: "Ada".into(),
    });
    h.expect(&[VoiceEvent::ParticipantJoined {
        id: ParticipantId("a".into()),
        name: "Ada".into(),
    }])
    .await;
    h.room_event(TransportEvent::RemoteAudioStarted {
        id: ParticipantId("a".into()),
        frames: a_rx,
    });
    h.room_event(TransportEvent::RemoteAudioStarted {
        id: ParticipantId("b".into()),
        frames: b_rx,
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    a_tx.send([100; FRAME_SAMPLES]).await.unwrap();
    b_tx.send([20; FRAME_SAMPLES]).await.unwrap();

    h.wait_until("the mixed frame is played", |h| {
        h.devices
            .played
            .lock()
            .unwrap()
            .iter()
            .any(|f| f.iter().all(|&s| s == 120))
    })
    .await;
}

#[tokio::test]
async fn deafened_plays_silence_and_mutes_the_microphone() {
    let mut h = start(Devices::default(), FakeTransport::default());
    h.join().await;
    let mut microphone = h.take_microphone();
    let (a_tx, a_rx) = mpsc::channel(20);
    h.room_event(TransportEvent::RemoteAudioStarted {
        id: ParticipantId("a".into()),
        frames: a_rx,
    });

    h.send(VoiceCommand::SetDeafened(true));
    h.expect(&[VoiceEvent::DeafenedChanged(true)]).await;
    h.devices.played.lock().unwrap().clear();
    for _ in 0..5 {
        a_tx.send([500; FRAME_SAMPLES]).await.unwrap();
    }
    h.speak(9, 5);
    tokio::time::sleep(Duration::from_millis(150)).await;

    assert!(
        h.devices
            .played
            .lock()
            .unwrap()
            .iter()
            .all(|f| f.iter().all(|&s| s == 0))
    );
    assert!(microphone.try_recv().is_err());
    assert_eq!(
        h.transport.room.lock().unwrap().mute_calls.last(),
        Some(&true)
    );
}

#[tokio::test]
async fn joins_listen_only_when_the_microphone_cannot_open() {
    let devices = Devices {
        input_fails: true,
        ..Devices::default()
    };
    let mut h = start(devices, FakeTransport::default());

    h.send(VoiceCommand::Join {
        server_url: "ws://sfu".into(),
        token: "t".into(),
    });
    h.expect(&[VoiceEvent::ConnectionStateChanged(
        ConnectionState::Connecting,
    )])
    .await;
    h.expect_error(VoiceErrorCode::DeviceUnavailable).await;
    h.expect(&[
        VoiceEvent::MicrophoneActiveChanged(false),
        VoiceEvent::ConnectionStateChanged(ConnectionState::Connected),
    ])
    .await;
}

#[tokio::test]
async fn failed_connection_returns_to_disconnected() {
    let transport = FakeTransport {
        fail: true,
        ..FakeTransport::default()
    };
    let mut h = start(Devices::default(), transport);

    h.send(VoiceCommand::Join {
        server_url: "ws://sfu".into(),
        token: "t".into(),
    });
    h.expect(&[VoiceEvent::ConnectionStateChanged(
        ConnectionState::Connecting,
    )])
    .await;
    h.expect_error(VoiceErrorCode::TransportUnavailable).await;
    h.expect(&[VoiceEvent::ConnectionStateChanged(
        ConnectionState::Disconnected,
    )])
    .await;
}

#[tokio::test]
async fn join_twice_is_rejected_and_leave_is_idempotent() {
    let mut h = start(Devices::default(), FakeTransport::default());
    h.join().await;

    h.send(VoiceCommand::Join {
        server_url: "ws://sfu".into(),
        token: "t".into(),
    });
    h.expect_error(VoiceErrorCode::InvalidState).await;
    h.send(VoiceCommand::Leave);
    h.expect(&[VoiceEvent::ConnectionStateChanged(
        ConnectionState::Disconnected,
    )])
    .await;
    h.send(VoiceCommand::Leave);
    h.send(VoiceCommand::SetMicrophoneMuted(true));

    // The second Leave emits nothing; the next event is the mute confirmation.
    h.expect(&[VoiceEvent::MicrophoneMutedChanged(true)]).await;
    assert!(h.transport.room.lock().unwrap().closed);
}

#[tokio::test]
async fn server_disconnect_is_reported_and_cleaned_up() {
    let mut h = start(Devices::default(), FakeTransport::default());
    h.join().await;

    h.room_event(TransportEvent::Reconnecting);
    h.expect(&[VoiceEvent::ConnectionStateChanged(
        ConnectionState::Reconnecting,
    )])
    .await;
    h.room_event(TransportEvent::Disconnected {
        reason: "server shutdown".into(),
    });
    h.expect_error(VoiceErrorCode::TransportUnavailable).await;
    h.expect(&[VoiceEvent::ConnectionStateChanged(
        ConnectionState::Disconnected,
    )])
    .await;
    assert!(h.transport.room.lock().unwrap().closed);
}

#[tokio::test]
async fn dropping_the_handle_leaves_the_room() {
    let mut h = start(Devices::default(), FakeTransport::default());
    h.join().await;
    let room = h.transport.room.clone();

    drop(h.handle);

    let deadline = Instant::now() + Duration::from_secs(2);
    while !room.lock().unwrap().closed {
        assert!(Instant::now() < deadline, "session was not closed");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test]
async fn refresh_devices_lists_inputs_and_outputs() {
    let mut h = start(Devices::default(), FakeTransport::default());

    h.send(VoiceCommand::RefreshDevices);

    match h.next().await {
        VoiceEvent::DevicesChanged { inputs, outputs } => {
            assert_eq!(inputs[0].name, "Fake mic");
            assert_eq!(outputs[0].name, "Fake speaker");
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test]
async fn stats_report_network_and_dropped_remote_audio() {
    let mut h = start(Devices::default(), FakeTransport::default());
    h.join().await;
    let (tx, rx) = mpsc::channel(64);
    h.room_event(TransportEvent::RemoteAudioStarted {
        id: ParticipantId("a".into()),
        frames: rx,
    });
    // Far more than the engine keeps queued, faster than playout drains it.
    for _ in 0..60 {
        tx.send([1; FRAME_SAMPLES]).await.unwrap();
    }

    let stats = loop {
        if let VoiceEvent::StatsUpdated(stats) = h.next_any().await {
            break stats;
        }
    };

    assert_eq!(stats.network.round_trip_ms, Some(42));
    assert_eq!(stats.network.inbound_concealed_ms, 30);
    assert!(
        stats.remote_frames_dropped > 0,
        "excess remote frames are counted: {stats:?}"
    );
}

#[tokio::test]
async fn no_stats_outside_a_room() {
    let mut h = start(Devices::default(), FakeTransport::default());
    h.join().await;
    h.send(VoiceCommand::Leave);
    h.expect(&[VoiceEvent::ConnectionStateChanged(
        ConnectionState::Disconnected,
    )])
    .await;

    let next = tokio::time::timeout(Duration::from_millis(1500), h.events.recv()).await;
    assert!(next.is_err(), "unexpected event after leaving: {next:?}");
}

#[tokio::test]
async fn speaking_is_reported_for_self_and_remotes_and_cleared_when_muted() {
    let mut h = start(Devices::default(), FakeTransport::default());
    h.join().await;

    h.speak(3000, 4);
    h.expect(&[VoiceEvent::SpeakingChanged {
        participant: None,
        speaking: true,
    }])
    .await;
    h.send(VoiceCommand::SetMicrophoneMuted(true));
    h.expect(&[
        VoiceEvent::SpeakingChanged {
            participant: None,
            speaking: false,
        },
        VoiceEvent::MicrophoneMutedChanged(true),
    ])
    .await;

    let (tx, rx) = mpsc::channel(20);
    h.room_event(TransportEvent::RemoteAudioStarted {
        id: ParticipantId("a".into()),
        frames: rx,
    });
    for _ in 0..4 {
        tx.send([3000; FRAME_SAMPLES]).await.unwrap();
    }
    h.expect(&[VoiceEvent::SpeakingChanged {
        participant: Some(ParticipantId("a".into())),
        speaking: true,
    }])
    .await;
    h.room_event(TransportEvent::RemoteAudioEnded {
        id: ParticipantId("a".into()),
    });
    h.expect(&[VoiceEvent::SpeakingChanged {
        participant: Some(ParticipantId("a".into())),
        speaking: false,
    }])
    .await;
}

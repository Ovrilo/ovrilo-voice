//! Engine behavior with fake devices and a fake transport (no audio hardware or network needed).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ovrilo_voice::audio::FRAME_SAMPLES;
use ovrilo_voice::{
    AudioBackend, AudioDevice, AudioProcessor, CaptureStream, Connected, ConnectionState, DeviceId,
    Frame, NetworkStats, NoProcessing, NoiseSuppression, ParticipantId, PlayoutStream,
    ProcessorFactory, ScreenShare, ScreenSource, ScreenSourceId, ScreenSourceKind,
    ShareOptimization, SharePreset, Transport, TransportEvent, TransportSession, VoiceCommand,
    VoiceEngine, VoiceEngineHandle, VoiceError, VoiceErrorCode, VoiceEvent,
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
    /// Source of the running fake screen share.
    sharing: Option<ScreenSourceId>,
    /// Participants whose screen audio is subscribed.
    screen_audio: Vec<ParticipantId>,
}

fn window_source() -> ScreenSource {
    ScreenSource {
        id: ScreenSourceId("window:7".into()),
        kind: ScreenSourceKind::Window,
        title: "Game".into(),
        application: Some("game.exe".into()),
        audio_available: true,
    }
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

    fn screen_sources(&self) -> Result<Vec<ScreenSource>, VoiceError> {
        Ok(vec![window_source()])
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

    async fn start_screen_share(
        &mut self,
        source: &ScreenSourceId,
        preset: SharePreset,
        optimization: ShareOptimization,
        audio: bool,
    ) -> Result<ScreenShare, VoiceError> {
        if *source != window_source().id {
            return Err(VoiceError::capture("source not found"));
        }
        self.0.lock().unwrap().sharing = Some(source.clone());
        Ok(ScreenShare {
            source: source.clone(),
            title: "Game".into(),
            preset,
            optimization,
            audio,
        })
    }

    async fn stop_screen_share(&mut self) {
        self.0.lock().unwrap().sharing = None;
    }

    async fn set_screen_audio_subscribed(&mut self, participant: &ParticipantId, subscribed: bool) {
        let mut room = self.0.lock().unwrap();
        room.screen_audio.retain(|p| p != participant);
        if subscribed {
            room.screen_audio.push(participant.clone());
        }
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
    start_with(devices, transport, Box::new(|| Box::new(NoProcessing)))
}

fn start_with(devices: Devices, transport: FakeTransport, processor: ProcessorFactory) -> Harness {
    let (handle, events) =
        VoiceEngine::spawn(FakeBackend(devices.clone()), transport.clone(), processor);
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
        VoiceEvent::TransmittingChanged(false),
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

#[tokio::test]
async fn push_to_talk_transmits_only_while_the_key_is_held() {
    let mut h = start(Devices::default(), FakeTransport::default());
    h.join().await;
    let mut microphone = h.take_microphone();

    h.send(VoiceCommand::SetPushToTalk(true));
    h.expect(&[
        VoiceEvent::PushToTalkChanged(true),
        VoiceEvent::TransmittingChanged(false),
    ])
    .await;

    // Key up: the stream continues, but only with silence, and nobody is "speaking".
    h.speak(3000, 5);
    for _ in 0..5 {
        let frame = tokio::time::timeout(Duration::from_secs(2), microphone.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(frame.iter().all(|&s| s == 0), "silence while the key is up");
    }

    h.send(VoiceCommand::SetPushToTalkPressed(true));
    h.expect(&[VoiceEvent::TransmittingChanged(true)]).await;
    h.speak(3000, 5);
    h.expect(&[VoiceEvent::SpeakingChanged {
        participant: None,
        speaking: true,
    }])
    .await;
    let mut voiced = 0;
    while let Ok(Some(frame)) =
        tokio::time::timeout(Duration::from_millis(200), microphone.recv()).await
    {
        if frame.iter().all(|&s| s == 3000) {
            voiced += 1;
        }
    }
    assert_eq!(voiced, 5, "held key transmits the microphone");

    h.send(VoiceCommand::SetPushToTalkPressed(false));
    h.expect(&[VoiceEvent::TransmittingChanged(false)]).await;
    // The release tail keeps the end of the word...
    h.speak(3000, 3);
    for _ in 0..3 {
        let frame = microphone.recv().await.unwrap();
        assert!(frame.iter().all(|&s| s == 3000), "release tail transmits");
    }
}

#[tokio::test]
async fn push_to_talk_key_is_ignored_when_push_to_talk_is_off() {
    let mut h = start(Devices::default(), FakeTransport::default());
    h.join().await;
    h.send(VoiceCommand::SetPushToTalkPressed(true));
    h.send(VoiceCommand::SetMicrophoneMuted(true));
    // No TransmittingChanged(true) from the key: the first event is the mute.
    h.expect(&[
        VoiceEvent::MicrophoneMutedChanged(true),
        VoiceEvent::TransmittingChanged(false),
    ])
    .await;
}

#[tokio::test]
async fn participant_volume_scales_playout_and_survives_rejoin() {
    let mut h = start(Devices::default(), FakeTransport::default());
    h.join().await;
    let a = ParticipantId("a".into());

    // Set before their audio starts (e.g. restored from settings).
    h.send(VoiceCommand::SetParticipantVolume {
        participant: a.clone(),
        percent: 50,
    });
    h.expect(&[VoiceEvent::ParticipantVolumeChanged {
        participant: a.clone(),
        percent: 50,
    }])
    .await;
    let (tx, rx) = mpsc::channel(20);
    h.room_event(TransportEvent::RemoteAudioStarted {
        id: a.clone(),
        frames: rx,
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    tx.send([200; FRAME_SAMPLES]).await.unwrap();
    h.wait_until("half-volume audio is played", |h| {
        h.devices
            .played
            .lock()
            .unwrap()
            .iter()
            .any(|f| f.iter().all(|&s| s == 100))
    })
    .await;

    // Over-range requests are clamped.
    h.send(VoiceCommand::SetParticipantVolume {
        participant: a.clone(),
        percent: 900,
    });
    h.expect(&[VoiceEvent::ParticipantVolumeChanged {
        participant: a,
        percent: 200,
    }])
    .await;
    tx.send([200; FRAME_SAMPLES]).await.unwrap();
    h.wait_until("double-volume audio is played", |h| {
        h.devices
            .played
            .lock()
            .unwrap()
            .iter()
            .any(|f| f.iter().all(|&s| s == 400))
    })
    .await;
}

#[tokio::test]
async fn remote_microphone_mute_is_forwarded() {
    let mut h = start(Devices::default(), FakeTransport::default());
    h.join().await;
    h.room_event(TransportEvent::ParticipantMuted {
        id: ParticipantId("a".into()),
        muted: true,
    });
    h.expect(&[VoiceEvent::ParticipantMutedChanged {
        participant: ParticipantId("a".into()),
        muted: true,
    }])
    .await;
}

// ---------- screen sharing (ADR-0017) ----------

fn game_share(audio: bool) -> ScreenShare {
    ScreenShare {
        source: window_source().id,
        title: "Game".into(),
        preset: SharePreset::FullHd1080p30,
        optimization: ShareOptimization::Motion,
        audio,
    }
}

fn start_game_share(h: &Harness) {
    h.send(VoiceCommand::StartScreenShare {
        source: window_source().id,
        preset: SharePreset::FullHd1080p30,
        optimization: ShareOptimization::Motion,
        audio: true,
    });
}

#[tokio::test]
async fn screen_sources_are_listed_and_sharing_needs_a_room() {
    let mut h = start(Devices::default(), FakeTransport::default());

    h.send(VoiceCommand::ListScreenSources);
    h.expect(&[VoiceEvent::ScreenSourcesListed(vec![window_source()])])
        .await;

    start_game_share(&h);
    h.expect_error(VoiceErrorCode::InvalidState).await;
}

#[tokio::test]
async fn screen_share_starts_stops_and_ends_with_the_room() {
    let mut h = start(Devices::default(), FakeTransport::default());
    h.join().await;

    start_game_share(&h);
    h.expect(&[VoiceEvent::ScreenShareChanged(Some(game_share(true)))])
        .await;
    assert_eq!(
        h.transport.room.lock().unwrap().sharing,
        Some(window_source().id)
    );

    h.send(VoiceCommand::StopScreenShare);
    h.expect(&[VoiceEvent::ScreenShareChanged(None)]).await;
    assert_eq!(h.transport.room.lock().unwrap().sharing, None);

    // Stopping again reports nothing; leaving while sharing ends the share first.
    h.send(VoiceCommand::StopScreenShare);
    start_game_share(&h);
    h.expect(&[VoiceEvent::ScreenShareChanged(Some(game_share(true)))])
        .await;
    h.send(VoiceCommand::Leave);
    h.expect(&[
        VoiceEvent::ScreenShareChanged(None),
        VoiceEvent::ConnectionStateChanged(ConnectionState::Disconnected),
    ])
    .await;
}

#[tokio::test]
async fn failed_and_lost_screen_shares_are_reported() {
    let mut h = start(Devices::default(), FakeTransport::default());
    h.join().await;

    h.send(VoiceCommand::StartScreenShare {
        source: ScreenSourceId("window:404".into()),
        preset: SharePreset::Hd720p30,
        optimization: ShareOptimization::Detail,
        audio: false,
    });
    h.expect_error(VoiceErrorCode::CaptureUnavailable).await;

    start_game_share(&h);
    h.expect(&[VoiceEvent::ScreenShareChanged(Some(game_share(true)))])
        .await;
    h.room_event(TransportEvent::ScreenShareEnded {
        reason: "the shared window was closed".into(),
    });
    h.expect_error(VoiceErrorCode::CaptureUnavailable).await;
    h.expect(&[VoiceEvent::ScreenShareChanged(None)]).await;
}

#[tokio::test]
async fn watched_screen_audio_is_played_but_never_counts_as_speaking() {
    let mut h = start(Devices::default(), FakeTransport::default());
    h.join().await;
    let ada = ParticipantId("a".into());

    h.room_event(TransportEvent::ParticipantScreenShare {
        id: ada.clone(),
        sharing: true,
        audio: true,
    });
    h.expect(&[VoiceEvent::ParticipantScreenShareChanged {
        participant: ada.clone(),
        sharing: true,
        audio: true,
    }])
    .await;

    h.send(VoiceCommand::WatchScreenShare {
        participant: ada.clone(),
        watching: true,
    });
    h.wait_until("screen audio is subscribed", |h| {
        h.transport.room.lock().unwrap().screen_audio == [ParticipantId("a".into())]
    })
    .await;

    let (stream_tx, stream_rx) = mpsc::channel(20);
    h.room_event(TransportEvent::RemoteScreenAudioStarted {
        id: ada.clone(),
        frames: stream_rx,
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    for _ in 0..30 {
        stream_tx.send([3000; FRAME_SAMPLES]).await.unwrap();
    }
    h.wait_until("the screen audio is played", |h| {
        h.devices
            .played
            .lock()
            .unwrap()
            .iter()
            .any(|f| f.iter().all(|&s| s == 3000))
    })
    .await;

    // Loud stream audio is not speech: no speaking indicator.
    let quiet = tokio::time::timeout(Duration::from_millis(300), async {
        loop {
            if let Some(VoiceEvent::SpeakingChanged { .. }) = h.events.recv().await {
                return;
            }
        }
    })
    .await;
    assert!(quiet.is_err(), "screen audio was reported as speaking");

    h.send(VoiceCommand::WatchScreenShare {
        participant: ada,
        watching: false,
    });
    h.wait_until("screen audio is dropped", |h| {
        h.transport.room.lock().unwrap().screen_audio.is_empty()
    })
    .await;
}

/// Supports Standard and Enhanced; Enhanced falls back to Standard after a few frames, like a
/// computer that cannot keep up.
struct FallbackProcessor {
    mode: NoiseSuppression,
    enhanced_frames: u32,
}

impl AudioProcessor for FallbackProcessor {
    fn process_render(&mut self, _frame: &mut Frame) {}

    fn process_capture(&mut self, _frame: &mut Frame, _playout_delay_ms: u32) {
        if self.mode == NoiseSuppression::Enhanced {
            self.enhanced_frames += 1;
            if self.enhanced_frames >= 3 {
                self.mode = NoiseSuppression::Standard;
            }
        }
    }

    fn set_noise_suppression(&mut self, mode: NoiseSuppression) {
        self.mode = mode;
        self.enhanced_frames = 0;
    }

    fn noise_suppression(&self) -> NoiseSuppression {
        self.mode
    }
}

#[tokio::test]
async fn noise_suppression_reports_the_mode_that_actually_runs() {
    // Without a suppressor, any request is answered with Off.
    let mut plain = start(Devices::default(), FakeTransport::default());
    plain.send(VoiceCommand::SetNoiseSuppression(
        NoiseSuppression::Enhanced,
    ));
    plain
        .expect(&[VoiceEvent::NoiseSuppressionChanged(NoiseSuppression::Off)])
        .await;

    let mut h = start_with(
        Devices::default(),
        FakeTransport::default(),
        Box::new(|| {
            Box::new(FallbackProcessor {
                mode: NoiseSuppression::Standard,
                enhanced_frames: 0,
            })
        }),
    );
    // Requests are always answered, even when nothing changes.
    h.send(VoiceCommand::SetNoiseSuppression(
        NoiseSuppression::Standard,
    ));
    h.expect(&[VoiceEvent::NoiseSuppressionChanged(
        NoiseSuppression::Standard,
    )])
    .await;
    h.send(VoiceCommand::SetNoiseSuppression(
        NoiseSuppression::Enhanced,
    ));
    h.expect(&[VoiceEvent::NoiseSuppressionChanged(
        NoiseSuppression::Enhanced,
    )])
    .await;

    // In a room the processor gives up on Enhanced by itself; the host is told.
    h.join().await;
    let _microphone = h.take_microphone();
    h.speak(3000, 5);
    loop {
        match h.next().await {
            VoiceEvent::NoiseSuppressionChanged(mode) => {
                assert_eq!(mode, NoiseSuppression::Standard);
                break;
            }
            _ => continue,
        }
    }
}

#[tokio::test]
async fn microphone_volume_scales_what_is_sent() {
    let mut h = start(Devices::default(), FakeTransport::default());
    h.send(VoiceCommand::SetInputVolume(50));
    h.expect(&[VoiceEvent::InputVolumeChanged(50)]).await;
    h.send(VoiceCommand::SetOutputVolume(500));
    h.expect(&[VoiceEvent::OutputVolumeChanged(200)]).await;

    h.join().await;
    let mut microphone = h.take_microphone();
    h.speak(3000, 1);
    let frame = tokio::time::timeout(Duration::from_secs(2), microphone.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(frame.iter().all(|&s| s == 1500), "half volume");
}

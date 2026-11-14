//! End-to-end SFU media test through the live signaling server.
//!
//! Two in-process clients connect over a real WebSocket, negotiate with the
//! server-side relay peers, and exchange RTP. This is the closest automated
//! equivalent of the 3-client Phase 2 spike: it exercises signaling, offer /
//! answer, trickle ICE, publication registration, subscription selection and
//! packet forwarding through the `server` crate.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sharescreen_core::{Codec, ConnectionState, PublishedTrackKind, PublishedVideoLayer};
use sharescreen_server::app_with;
use sharescreen_server::turn::TurnConfig;
use sharescreen_signaling::{Incoming, SignalMessage, SignalingClient, SignalingSender};
use sharescreen_webrtc::{
    IceCandidate, PeerConfig, PeerEvent, RemoteTrack, RemoteTrackKind, RtpPacket,
    SessionDescription, WebRtcPeer,
};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::timeout;

fn drain_ice(candidate: &IceCandidate) -> bool {
    !candidate.candidate.is_empty()
}

/// A client peer plus the task that pumps its signaling and WebRTC events.
struct TestClient {
    peer: Arc<WebRtcPeer>,
    sender: SignalingSender,
    remote: mpsc::UnboundedReceiver<RemoteTrack>,
    room: Arc<Mutex<Option<String>>>,
    subscriptions: Arc<Mutex<Vec<String>>>,
    answers: Arc<AtomicUsize>,
    errors: Arc<Mutex<Vec<String>>>,
    driver: JoinHandle<()>,
}

impl TestClient {
    async fn connect(url: &str) -> Self {
        let client = SignalingClient::connect(url).await.expect("signaling");
        let sender = client.sender();
        let mut incoming = client.incoming;

        let (event_tx, mut events) = mpsc::unbounded_channel::<PeerEvent>();
        let (remote_tx, remote) = mpsc::unbounded_channel::<RemoteTrack>();
        let peer = Arc::new(
            WebRtcPeer::new(
                PeerConfig::default(),
                Codec::H264,
                Arc::new(move |event: PeerEvent| {
                    let _ = event_tx.send(event);
                }),
            )
            .await
            .expect("peer"),
        );
        let errors = Arc::new(Mutex::new(Vec::new()));
        let driver_errors = errors.clone();
        let room = Arc::new(Mutex::new(None));
        let driver_room = room.clone();
        let subscriptions = Arc::new(Mutex::new(Vec::new()));
        let driver_subscriptions = subscriptions.clone();
        let answers = Arc::new(AtomicUsize::new(0));
        let driver_answers = answers.clone();
        let driver_sender = sender.clone();
        let driver_peer = peer.clone();

        let driver = tokio::spawn(async move {
            let mut pending_ice = Vec::<IceCandidate>::new();
            let mut remote_ready = false;
            loop {
                tokio::select! {
                    message = incoming.recv() => {
                        let Some(Incoming::Message(message)) = message else {
                            return;
                        };
                        match message {
                            SignalMessage::RoomCreated { room_id }
                            | SignalMessage::Joined { room_id, .. } => {
                                *driver_room.lock().unwrap() = Some(room_id);
                            }
                            SignalMessage::SfuAnswer { sdp } => {
                                let Ok(description) = serde_json::from_str::<SessionDescription>(&sdp)
                                else {
                                    driver_errors
                                        .lock()
                                        .unwrap()
                                        .push("invalid SFU answer".into());
                                    continue;
                                };
                                if let Err(error) = driver_peer
                                    .set_remote_description(&description)
                                    .await
                                {
                                    driver_errors
                                        .lock()
                                        .unwrap()
                                        .push(format!("set_remote_description: {error}"));
                                    continue;
                                }
                                remote_ready = true;
                                driver_answers.fetch_add(1, Ordering::Release);
                                for candidate in pending_ice.drain(..) {
                                    let _ = driver_peer.add_ice_candidate(candidate).await;
                                }
                            }
                            SignalMessage::SfuIceCandidate { candidate } => {
                                let Ok(candidate) =
                                    serde_json::from_str::<IceCandidate>(&candidate)
                                else {
                                    continue;
                                };
                                if remote_ready {
                                    let _ = driver_peer.add_ice_candidate(candidate).await;
                                } else {
                                    pending_ice.push(candidate);
                                }
                            }
                            SignalMessage::SfuSubscriptions { tracks, .. } => {
                                *driver_subscriptions.lock().unwrap() = tracks;
                            }
                            SignalMessage::Error { message } => {
                                driver_errors.lock().unwrap().push(message);
                            }
                            _ => {}
                        }
                    }
                    event = events.recv() => {
                        match event {
                            Some(PeerEvent::IceCandidate(candidate)) => {
                                if drain_ice(&candidate) {
                                    if let Ok(json) = serde_json::to_string(&candidate) {
                                        let _ = driver_sender.send(
                                            SignalMessage::SfuIceCandidate { candidate: json },
                                        );
                                    }
                                }
                            }
                            Some(PeerEvent::ConnectionState(state)) => {
                                if state == ConnectionState::Failed {
                                    driver_errors
                                        .lock()
                                        .unwrap()
                                        .push("ICE failed".into());
                                }
                            }
                            Some(PeerEvent::TrackReceived { track }) => {
                                let _ = remote_tx.send(track);
                            }
                            Some(_) => {}
                            None => return,
                        }
                    }
                }
            }
        });

        Self {
            peer,
            sender,
            remote,
            room,
            subscriptions,
            answers,
            errors,
            driver,
        }
    }

    fn send(&self, message: SignalMessage) {
        self.sender.send(message).expect("signaling send");
    }

    async fn offer(&self) -> SessionDescription {
        let offer = self.peer.create_offer().await.expect("offer");
        self.peer
            .set_local_description(&offer)
            .await
            .expect("local description");
        offer
    }

    async fn next_track(&mut self) -> RemoteTrack {
        timeout(Duration::from_secs(20), self.remote.recv())
            .await
            .expect("remote track should arrive")
            .expect("remote track channel should stay open")
    }

    async fn wait_room(&self) -> String {
        timeout(Duration::from_secs(10), async {
            loop {
                if let Some(room) = self.room.lock().unwrap().clone() {
                    return room;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("room id should be announced")
    }

    async fn wait_subscription(&self, track_id: &str) {
        timeout(Duration::from_secs(10), async {
            loop {
                if self
                    .subscriptions
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|track| track == track_id)
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("subscription {track_id} should arrive"));
    }

    async fn wait_answers(&self, count: usize) {
        timeout(Duration::from_secs(15), async {
            while self.answers.load(Ordering::Acquire) < count {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("expected {count} SFU answers"));
    }

    fn assert_no_errors(&self) {
        let errors = self.errors.lock().unwrap();
        assert!(errors.is_empty(), "client errors: {errors:?}");
    }

    async fn close(self) {
        self.sender.close();
        self.driver.abort();
        let _ = self.peer.close().await;
    }
}

async fn start_server() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let turn = TurnConfig {
        urls: vec![],
        secret: "secret".to_string(),
        ttl_secs: 3600,
        user: "user".to_string(),
    };
    tokio::spawn(async move {
        axum::serve(listener, app_with(turn)).await.unwrap();
    });
    format!("ws://{addr}/ws")
}

#[tokio::test]
async fn sfu_relays_screen_and_microphone_to_a_late_joiner() {
    let base = start_server().await;
    let publisher = TestClient::connect(&format!("{base}?id=a&name=Alice")).await;
    let mut subscriber = TestClient::connect(&format!("{base}?id=b&name=Bob")).await;

    // Alice creates the room and registers two publications.
    publisher.send(SignalMessage::CreateRoom);
    let screen_track = publisher
        .peer
        .add_relay_track("a", "a-screen", Codec::H264)
        .await
        .unwrap();
    let mic_track = publisher
        .peer
        .add_relay_track("a", "a-mic", Codec::Opus)
        .await
        .unwrap();
    for (track_id, kind, layer) in [
        ("a-mic", PublishedTrackKind::Audio, None),
        (
            "a-screen",
            PublishedTrackKind::Screen,
            Some(PublishedVideoLayer::Full),
        ),
    ] {
        publisher.send(SignalMessage::PublishTrack {
            track_id: track_id.into(),
            kind,
            layer,
        });
    }
    let offer = publisher.offer().await;
    publisher.send(SignalMessage::SfuOffer {
        sdp: serde_json::to_string(&offer).unwrap(),
        video_codec: "h264".into(),
    });

    // Keep the publisher writing: forwarding only starts once the ingress peer
    // has received a packet, and the relay must keep a live stream afterwards.
    let running = Arc::new(AtomicBool::new(true));
    let writer_running = running.clone();
    let writer = tokio::spawn(async move {
        let mut sequence = 0u16;
        while writer_running.load(Ordering::Acquire) {
            let _ = screen_track
                .write_rtp(RtpPacket {
                    payload: vec![0x3C; 40],
                    marker: sequence.is_multiple_of(15),
                    payload_type: 96,
                    sequence_number: sequence,
                    timestamp: u32::from(sequence) * 3_000,
                    ssrc: 4242,
                })
                .await;
            let _ = mic_track
                .write_rtp(RtpPacket {
                    payload: vec![0x7E; 20],
                    marker: false,
                    payload_type: 111,
                    sequence_number: sequence,
                    timestamp: u32::from(sequence) * 960,
                    ssrc: 4343,
                })
                .await;
            sequence = sequence.wrapping_add(1);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });

    // Bob joins later, reserves receive slots and offers.
    let room_id = publisher.wait_room().await;
    subscriber.send(SignalMessage::JoinRoom { room_id });
    subscriber
        .peer
        .add_recvonly_transceiver(RemoteTrackKind::Audio)
        .await
        .unwrap();
    subscriber
        .peer
        .add_recvonly_transceiver(RemoteTrackKind::Video)
        .await
        .unwrap();
    let offer = subscriber.offer().await;
    subscriber.send(SignalMessage::SfuOffer {
        sdp: serde_json::to_string(&offer).unwrap(),
        video_codec: "h264".into(),
    });

    // Bob must receive both relayed tracks from Alice.
    let mut audio: Option<RemoteTrack> = None;
    let mut video: Option<RemoteTrack> = None;
    while audio.is_none() || video.is_none() {
        let track = subscriber.next_track().await;
        match track.kind().await {
            RemoteTrackKind::Audio => audio = Some(track),
            RemoteTrackKind::Video => video = Some(track),
        }
    }
    let audio = audio.unwrap();
    let video = video.unwrap();
    assert_eq!(audio.stream_id().await, "a");
    assert_eq!(video.stream_id().await, "a");

    let packet = timeout(Duration::from_secs(10), video.next_packet())
        .await
        .expect("relayed video packet should arrive")
        .expect("video track should stay open");
    assert_eq!(packet.payload, vec![0x3C; 40]);
    assert_ne!(packet.ssrc, 4242, "egress SSRC is rewritten");
    assert!(
        packet.timestamp.is_multiple_of(3_000),
        "timestamp is preserved"
    );

    let audio_packet = timeout(Duration::from_secs(10), audio.next_packet())
        .await
        .expect("relayed audio packet should arrive")
        .expect("audio track should stay open");
    assert_eq!(audio_packet.payload, vec![0x7E; 20]);

    running.store(false, Ordering::Release);
    writer.abort();
    publisher.assert_no_errors();
    subscriber.assert_no_errors();
    publisher.close().await;
    subscriber.close().await;
}

/// Two publishers, one viewer. The viewer starts subscribed to the fallback
/// full track, then switches focus to the second publisher. The SFU must add
/// the second egress track on a free receive slot and relay its payload.
#[tokio::test]
async fn focus_switch_subscribes_to_the_second_publisher() {
    let base = start_server().await;
    let publisher_a = TestClient::connect(&format!("{base}?id=a&name=Alice")).await;
    let mut viewer = TestClient::connect(&format!("{base}?id=b&name=Bob")).await;
    let publisher_c = TestClient::connect(&format!("{base}?id=c&name=Carol")).await;

    publisher_a.send(SignalMessage::CreateRoom);
    let a_screen = publisher_a
        .peer
        .add_relay_track("a", "a-screen", Codec::H264)
        .await
        .unwrap();
    publisher_a.send(SignalMessage::PublishTrack {
        track_id: "a-screen".into(),
        kind: PublishedTrackKind::Screen,
        layer: Some(PublishedVideoLayer::Full),
    });
    let offer = publisher_a.offer().await;
    publisher_a.send(SignalMessage::SfuOffer {
        sdp: serde_json::to_string(&offer).unwrap(),
        video_codec: "h264".into(),
    });
    let room_id = publisher_a.wait_room().await;

    // Reserve more than one slot per kind; a focus swap needs a fresh m-line
    // for the newly focused publisher.
    let running = Arc::new(AtomicBool::new(true));
    let a_running = running.clone();
    let a_writer = tokio::spawn(async move {
        let mut sequence = 0u16;
        while a_running.load(Ordering::Acquire) {
            let _ = a_screen
                .write_rtp(RtpPacket {
                    payload: vec![0xA1; 32],
                    marker: sequence.is_multiple_of(15),
                    payload_type: 96,
                    sequence_number: sequence,
                    timestamp: u32::from(sequence) * 3_000,
                    ssrc: 1111,
                })
                .await;
            sequence = sequence.wrapping_add(1);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });

    viewer.send(SignalMessage::JoinRoom {
        room_id: room_id.clone(),
    });
    for _ in 0..3 {
        viewer
            .peer
            .add_recvonly_transceiver(RemoteTrackKind::Audio)
            .await
            .unwrap();
        viewer
            .peer
            .add_recvonly_transceiver(RemoteTrackKind::Video)
            .await
            .unwrap();
    }
    let offer = viewer.offer().await;
    viewer.send(SignalMessage::SfuOffer {
        sdp: serde_json::to_string(&offer).unwrap(),
        video_codec: "h264".into(),
    });

    // The fallback drops the viewer onto Alice's full track.
    viewer.wait_subscription("a-screen").await;
    let first = viewer.next_track().await;
    assert_eq!(first.stream_id().await, "a");
    assert_eq!(first.kind().await, RemoteTrackKind::Video);

    publisher_c.send(SignalMessage::JoinRoom {
        room_id: room_id.clone(),
    });
    let c_screen = publisher_c
        .peer
        .add_relay_track("c", "c-screen", Codec::H264)
        .await
        .unwrap();
    publisher_c.send(SignalMessage::PublishTrack {
        track_id: "c-screen".into(),
        kind: PublishedTrackKind::Screen,
        layer: Some(PublishedVideoLayer::Full),
    });
    let offer = publisher_c.offer().await;
    publisher_c.send(SignalMessage::SfuOffer {
        sdp: serde_json::to_string(&offer).unwrap(),
        video_codec: "h264".into(),
    });
    let c_running = running.clone();
    let c_writer = tokio::spawn(async move {
        let mut sequence = 0u16;
        while c_running.load(Ordering::Acquire) {
            let _ = c_screen
                .write_rtp(RtpPacket {
                    payload: vec![0xC3; 32],
                    marker: sequence.is_multiple_of(15),
                    payload_type: 96,
                    sequence_number: sequence,
                    timestamp: u32::from(sequence) * 3_000,
                    ssrc: 2222,
                })
                .await;
            sequence = sequence.wrapping_add(1);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });

    // The publication is registered on Carol's connection before her offer is
    // answered. Waiting for that answer removes the cross-connection race with
    // Bob's focus request.
    publisher_c.wait_answers(1).await;

    viewer.send(SignalMessage::SetFocus {
        publisher: Some("c".into()),
    });
    viewer.wait_subscription("c-screen").await;

    // The desktop client renegotiates when its subscription set changes.
    let offer = viewer.offer().await;
    viewer.send(SignalMessage::SfuOffer {
        sdp: serde_json::to_string(&offer).unwrap(),
        video_codec: "h264".into(),
    });

    let focus_track = timeout(Duration::from_secs(20), async {
        loop {
            let track = viewer.next_track().await;
            if track.stream_id().await == "c" {
                return track;
            }
        }
    })
    .await
    .expect("the focused publisher track should be added");

    let packet = timeout(Duration::from_secs(10), focus_track.next_packet())
        .await
        .expect("focused publisher packet should arrive")
        .expect("focused track should stay open");
    assert_eq!(packet.payload, vec![0xC3; 32]);

    // The previous focus must stop forwarding. Its writer stays negotiated, so
    // re-focusing it later does not need another m-line.
    tokio::time::sleep(Duration::from_millis(300)).await;
    while timeout(Duration::from_millis(50), first.next_packet())
        .await
        .is_ok()
    {}
    assert!(
        timeout(Duration::from_millis(700), first.next_packet())
            .await
            .is_err(),
        "the unfocused publisher must stop forwarding"
    );

    running.store(false, Ordering::Release);
    a_writer.abort();
    c_writer.abort();
    publisher_a.assert_no_errors();
    viewer.assert_no_errors();
    publisher_c.assert_no_errors();
    publisher_a.close().await;
    viewer.close().await;
    publisher_c.close().await;
}

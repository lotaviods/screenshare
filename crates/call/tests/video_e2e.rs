//! Full end-to-end video test: two CallSessions through the real signaling server,
//! A sends real H.264 frames and B receives them via `shared_screen_frame`.

use std::time::Duration;

use sharescreen_call::{CallConfig, CallSession};
use sharescreen_core::{Codec, EncodedSample, MediaKind};
use sharescreen_server::app_with;
use sharescreen_server::turn::TurnConfig;

const H264_SAMPLE: &[u8] = include_bytes!("fixtures/sample.h264");

fn encoded_frames(count: usize) -> Vec<Vec<u8>> {
    (0..count).map(|_| H264_SAMPLE.to_vec()).collect()
}

#[tokio::test]
async fn full_call_video_reaches_viewer() {
    // Start a real signaling server on a random port.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let turn = TurnConfig {
        urls: vec!["turn:127.0.0.1:3478".to_string()],
        secret: "test-secret".to_string(),
        ttl_secs: 3600,
        user: "test-user".to_string(),
    };
    let server = tokio::spawn(async move {
        axum::serve(listener, app_with(turn)).await.unwrap();
    });
    let _ = tokio::time::sleep(Duration::from_millis(100)).await;

    let ws_url = format!("ws://{addr}/ws");
    let peer_config = sharescreen_webrtc::PeerConfig::default();

    // Alice – creates a room.
    let (a_events, mut a_rx) =
        tokio::sync::mpsc::unbounded_channel::<sharescreen_core::CallEvent>();
    let a = CallSession::start(
        CallConfig {
            self_id: "alice".to_string(),
            self_name: "Alice".to_string(),
            signaling_url: format!("{ws_url}?id=alice&name=Alice"),
            peer_config: peer_config.clone(),
            video_codec: Codec::H264,
        },
        a_events,
    )
    .await
    .expect("alice session start");

    a.create_room().expect("create room");
    let room = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(ev) = a_rx.recv().await {
            if let sharescreen_core::CallEvent::RoomCreated { room_id } = ev {
                return room_id;
            }
        }
        panic!("no RoomCreated");
    })
    .await
    .expect("room created within timeout");

    // Bob – join the room.
    let (b_events, mut b_rx) =
        tokio::sync::mpsc::unbounded_channel::<sharescreen_core::CallEvent>();
    let b = CallSession::start(
        CallConfig {
            self_id: "bob".to_string(),
            self_name: "Bob".to_string(),
            signaling_url: format!("{ws_url}?id=bob&name=Bob"),
            peer_config: peer_config.clone(),
            video_codec: Codec::H264,
        },
        b_events,
    )
    .await
    .expect("bob session start");

    b.join_room(room.clone()).expect("join room");
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(ev) = b_rx.recv().await {
            if let sharescreen_core::CallEvent::RoomJoined { room_id: r } = ev {
                if r == room {
                    return;
                }
            }
        }
        panic!("no RoomJoined");
    })
    .await
    .expect("bob joined the room");

    // Alice learns Bob joined.
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(ev) = a_rx.recv().await {
            if let sharescreen_core::CallEvent::ParticipantJoined { participant: p } = ev {
                if p.id == "bob" {
                    return;
                }
            }
        }
        panic!("no ParticipantJoined");
    })
    .await
    .expect("alice learned bob joined");

    // Give ICE / DTLS a moment to bring the peer connection up.
    tokio::time::sleep(Duration::from_secs(2)).await;

    let frames = encoded_frames(20);

    // Alice sends real H.264 frames into her screen sender.
    tokio::spawn({
        let frames = frames.clone();
        let tx = a.screen_sender();
        async move {
            for (i, data) in frames.into_iter().enumerate() {
                let _ = tx
                    .send(EncodedSample {
                        kind: MediaKind::Video,
                        codec: Codec::H264,
                        data,
                        timestamp_micros: (i as u64) * 33_333,
                        duration_micros: 33_333,
                        keyframe: i == 0,
                    })
                    .await;
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    });

    a.set_screen_sharing(true);

    // Bob waits until he has a decoded frame.
    let mut frame = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while tokio::time::Instant::now() < deadline {
        let f = b.shared_screen_frame();
        if f.is_some() {
            frame = f;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    a.disconnect().await;
    b.disconnect().await;
    server.abort();

    let frame = frame.expect("Bob received a decoded JPEG frame from Alice");
    assert!(frame.len() > 100, "frame too small: {} bytes", frame.len());
    assert_eq!(&frame[0..2], &[0xff, 0xd8], "not a JPEG");
}

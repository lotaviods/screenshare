//! Signaling lifecycle regression: a disconnected session leaves the room,
//! and a replacement session can rejoin and advertise its media state.
use std::time::Duration;

use sharescreen_call::{CallConfig, CallSession};
use sharescreen_core::{CallEvent, Codec};
use sharescreen_server::{app_with, turn::TurnConfig};
use sharescreen_webrtc::PeerConfig;
use tokio::sync::mpsc;

async fn event_matching(
    events: &mut mpsc::UnboundedReceiver<CallEvent>,
    predicate: impl Fn(&CallEvent) -> bool,
) -> CallEvent {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let event = events.recv().await.expect("event channel closed");
            if predicate(&event) {
                return event;
            }
        }
    })
    .await
    .expect("timed out waiting for call event")
}

fn config(base: &str, id: &str, codec: Codec) -> CallConfig {
    CallConfig {
        self_id: id.into(),
        self_name: id.into(),
        signaling_url: format!("{base}?id={id}&name={id}"),
        peer_config: PeerConfig::default(),
        video_codec: codec,
    }
}

#[tokio::test]
async fn disconnect_closes_signaling_and_replacement_can_rejoin() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let turn = TurnConfig {
        urls: vec![],
        secret: "secret".into(),
        ttl_secs: 3600,
        user: "user".into(),
    };
    let server = tokio::spawn(async move { axum::serve(listener, app_with(turn)).await.unwrap() });
    let base = format!("ws://{address}/ws");
    let (a_tx, mut a_events) = mpsc::unbounded_channel();
    let a = CallSession::start(config(&base, "a", Codec::H264), a_tx)
        .await
        .unwrap();
    let (b_tx, mut b_events) = mpsc::unbounded_channel();
    let b = CallSession::start(config(&base, "b", Codec::H264), b_tx)
        .await
        .unwrap();
    a.create_room().unwrap();
    let CallEvent::RoomCreated { room_id } = event_matching(&mut a_events, |e| {
        matches!(e, CallEvent::RoomCreated { .. })
    })
    .await
    else {
        unreachable!()
    };
    b.join_room(room_id.clone()).unwrap();
    event_matching(&mut b_events, |e| matches!(e, CallEvent::RoomJoined { .. })).await;
    event_matching(
        &mut a_events,
        |e| matches!(e, CallEvent::ParticipantJoined { participant } if participant.id == "b"),
    )
    .await;
    a.disconnect().await;
    event_matching(
        &mut b_events,
        |e| matches!(e, CallEvent::ParticipantLeft { id } if id == "a"),
    )
    .await;

    let (replacement_tx, mut replacement_events) = mpsc::unbounded_channel();
    let replacement = CallSession::start(config(&base, "a2", Codec::Vp8), replacement_tx)
        .await
        .unwrap();
    assert_eq!(replacement.video_codec(), Codec::Vp8);
    replacement.set_mic_on(true);
    replacement.set_screen_sharing(true);
    replacement.join_room(room_id).unwrap();
    event_matching(&mut replacement_events, |e| {
        matches!(e, CallEvent::RoomJoined { .. })
    })
    .await;
    event_matching(&mut b_events, |e| matches!(e, CallEvent::ParticipantUpdated { participant } if participant.id == "a2" && participant.mic_on && participant.screen_on)).await;
    replacement.disconnect().await;
    b.disconnect().await;
    server.abort();
}

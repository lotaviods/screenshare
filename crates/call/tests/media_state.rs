//! End-to-end test for the peer media-state relay: two clients join a room
//! and each learns whether the other is sharing a screen.

use std::time::Duration;

use sharescreen_core::{PeerPayload, SignalMessage};
use sharescreen_server::app_with;
use sharescreen_server::turn::TurnConfig;
use sharescreen_signaling::{Incoming, SignalingClient};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::timeout;

async fn recv_msg(rx: &mut UnboundedReceiver<Incoming>) -> SignalMessage {
    match timeout(Duration::from_secs(5), rx.recv()).await {
        Ok(Some(Incoming::Message(m))) => m,
        Ok(Some(Incoming::Disconnected)) => panic!("unexpected disconnect"),
        Ok(None) => panic!("channel closed"),
        Err(_) => panic!("timed out waiting for signaling message"),
    }
}

#[tokio::test]
async fn media_state_is_relayed_between_peers() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let turn = TurnConfig {
        urls: vec!["turn:turn.example.com:3478".to_string()],
        secret: "secret".to_string(),
        ttl_secs: 3600,
        user: "user".to_string(),
    };
    tokio::spawn(async move {
        axum::serve(listener, app_with(turn)).await.unwrap();
    });

    let base = format!("ws://{addr}/ws");
    let mut a = SignalingClient::connect(&format!("{base}?id=a&name=Alice"))
        .await
        .unwrap();
    let mut b = SignalingClient::connect(&format!("{base}?id=b&name=Bob"))
        .await
        .unwrap();

    a.send(SignalMessage::CreateRoom).unwrap();
    let room_id = match recv_msg(&mut a.incoming).await {
        SignalMessage::RoomCreated { room_id } => room_id,
        other => panic!("expected RoomCreated, got {other:?}"),
    };

    b.send(SignalMessage::JoinRoom {
        room_id: room_id.clone(),
    })
    .unwrap();
    match recv_msg(&mut b.incoming).await {
        SignalMessage::Joined {
            room_id: rid,
            peers,
        } => {
            assert_eq!(rid, room_id);
            assert_eq!(peers.len(), 1);
        }
        other => panic!("expected Joined, got {other:?}"),
    }
    // The server tells A about the newcomer.
    match recv_msg(&mut a.incoming).await {
        SignalMessage::PeerJoined { peer } => assert_eq!(peer.id, "b"),
        other => panic!("expected PeerJoined, got {other:?}"),
    }

    // A tells B that it started sharing its screen.
    a.send(SignalMessage::ToPeer {
        to: "b".into(),
        payload: PeerPayload::MediaState {
            mic_on: false,
            screen_on: true,
        },
    })
    .unwrap();
    match recv_msg(&mut b.incoming).await {
        SignalMessage::FromPeer { from, payload } => {
            assert_eq!(from, "a");
            assert_eq!(
                payload,
                PeerPayload::MediaState {
                    mic_on: false,
                    screen_on: true
                }
            );
        }
        other => panic!("expected FromPeer, got {other:?}"),
    }

    // B answers with its own state.
    b.send(SignalMessage::ToPeer {
        to: "a".into(),
        payload: PeerPayload::MediaState {
            mic_on: true,
            screen_on: false,
        },
    })
    .unwrap();
    match recv_msg(&mut a.incoming).await {
        SignalMessage::FromPeer { from, payload } => {
            assert_eq!(from, "b");
            assert_eq!(
                payload,
                PeerPayload::MediaState {
                    mic_on: true,
                    screen_on: false
                }
            );
        }
        other => panic!("expected FromPeer, got {other:?}"),
    }
}

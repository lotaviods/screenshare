//! End-to-end signaling test: start the server, connect two clients, create a
//! room, join it, and relay an offer/answer between the peers.

use std::time::Duration;

use sharescreen_server::app_with;
use sharescreen_server::turn::TurnConfig;
use sharescreen_signaling::{Incoming, PeerInfo, PeerPayload, SignalMessage, SignalingClient};
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
async fn two_peers_create_join_and_relay() {
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

    // A creates a room.
    a.send(SignalMessage::CreateRoom).unwrap();
    let room_id = match recv_msg(&mut a.incoming).await {
        SignalMessage::RoomCreated { room_id } => room_id,
        other => panic!("expected RoomCreated, got {other:?}"),
    };

    // B joins the room.
    b.send(SignalMessage::JoinRoom {
        room_id: room_id.clone(),
    })
    .unwrap();

    // B receives the roster (just A).
    match recv_msg(&mut b.incoming).await {
        SignalMessage::Joined { room_id: rid, peers } => {
            assert_eq!(rid, room_id);
            assert_eq!(peers, vec![PeerInfo { id: "a".into(), name: "Alice".into() }]);
        }
        other => panic!("expected Joined, got {other:?}"),
    }

    // A is notified that B joined.
    match recv_msg(&mut a.incoming).await {
        SignalMessage::PeerJoined { peer } => assert_eq!(peer.id, "b"),
        other => panic!("expected PeerJoined, got {other:?}"),
    }

    // A relays an offer to B.
    let offer = PeerPayload::Offer { sdp: "{}".into() };
    a.send(SignalMessage::ToPeer {
        to: "b".into(),
        payload: offer.clone(),
    })
    .unwrap();
    match recv_msg(&mut b.incoming).await {
        SignalMessage::FromPeer { from, payload } => {
            assert_eq!(from, "a");
            assert_eq!(payload, offer);
        }
        other => panic!("expected FromPeer, got {other:?}"),
    }

    // B answers back to A.
    let answer = PeerPayload::Answer { sdp: "{}".into() };
    b.send(SignalMessage::ToPeer {
        to: "a".into(),
        payload: answer.clone(),
    })
    .unwrap();
    match recv_msg(&mut a.incoming).await {
        SignalMessage::FromPeer { from, payload } => {
            assert_eq!(from, "b");
            assert_eq!(payload, answer);
        }
        other => panic!("expected FromPeer, got {other:?}"),
    }
}

#[tokio::test]
async fn room_creator_deletes_room_for_everyone() {
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
    assert_eq!(room_id.len(), 6, "room id should be a short code");

    b.send(SignalMessage::JoinRoom {
        room_id: room_id.clone(),
    })
    .unwrap();
    assert!(matches!(recv_msg(&mut b.incoming).await, SignalMessage::Joined { .. }));
    assert!(matches!(recv_msg(&mut a.incoming).await, SignalMessage::PeerJoined { .. }));

    // Any member can delete the room, including a peer that rejoined after a
    // reconnect (it no longer shares the creator's peer id).
    b.send(SignalMessage::DeleteRoom).unwrap();
    for client in [&mut a, &mut b] {
        match recv_msg(&mut client.incoming).await {
            SignalMessage::RoomDeleted { room_id: rid } => assert_eq!(rid, room_id),
            other => panic!("expected RoomDeleted, got {other:?}"),
        }
    }

    // The room is gone: joining it now fails.
    let mut c = SignalingClient::connect(&format!("{base}?id=c&name=Carol"))
        .await
        .unwrap();
    c.send(SignalMessage::JoinRoom { room_id }).unwrap();
    match recv_msg(&mut c.incoming).await {
        SignalMessage::Error { message } => assert!(message.contains("not found")),
        other => panic!("expected Error, got {other:?}"),
    }
}

#[tokio::test]
async fn server_issues_turn_credentials() {
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

    let mut client = SignalingClient::connect(&format!("ws://{addr}/ws?id=a&name=Alice"))
        .await
        .unwrap();

    client.send(SignalMessage::GetTurnCredentials).unwrap();

    match recv_msg(&mut client.incoming).await {
        SignalMessage::TurnCredentials {
            urls,
            username,
            credential,
        } => {
            assert_eq!(urls, vec!["turn:turn.example.com:3478".to_string()]);
            // "<expiry>:<user>" with an HMAC-SHA1 over the shared secret.
            let expiry: u64 = username.split(':').next().unwrap().parse().unwrap();
            assert!(expiry > 1_700_000_000, "unexpected expiry: {username}");
            assert!(username.ends_with(":user"));
            assert!(!credential.is_empty());
        }
        other => panic!("expected TurnCredentials, got {other:?}"),
    }
}

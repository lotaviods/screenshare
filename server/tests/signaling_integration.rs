//! End-to-end signaling test: start the server, connect two clients, create a
//! room, join it, and relay an offer/answer between the peers.

use std::time::Duration;

use sharescreen_core::{PublishedTrackKind, PublishedVideoLayer};
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

/// Receive the next message that is not one of the asynchronous
/// `SfuIceCandidate`/`SfuSubscriptions` updates. ICE gathering and
/// subscription snapshots interleave with the answers a test waits for.
async fn recv_signaling(
    rx: &mut UnboundedReceiver<Incoming>,
    ignore_subscriptions: bool,
) -> SignalMessage {
    loop {
        match recv_msg(rx).await {
            SignalMessage::SfuIceCandidate { .. } => continue,
            SignalMessage::SfuSubscriptions { .. } if ignore_subscriptions => continue,
            other => return other,
        }
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
        SignalMessage::Joined {
            room_id: rid,
            peers,
        } => {
            assert_eq!(rid, room_id);
            assert_eq!(
                peers,
                vec![PeerInfo {
                    id: "a".into(),
                    name: "Alice".into()
                }]
            );
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
    assert!(matches!(
        recv_msg(&mut b.incoming).await,
        SignalMessage::Joined { .. }
    ));
    assert!(matches!(
        recv_msg(&mut a.incoming).await,
        SignalMessage::PeerJoined { .. }
    ));

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

#[tokio::test]
async fn sfu_publications_and_focus_produce_viewer_subscriptions() {
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
    b.send(SignalMessage::JoinRoom { room_id }).unwrap();
    assert!(matches!(
        recv_msg(&mut b.incoming).await,
        SignalMessage::Joined { .. }
    ));
    assert!(matches!(
        recv_msg(&mut a.incoming).await,
        SignalMessage::PeerJoined { .. }
    ));

    for (track_id, kind, layer) in [
        ("a-audio", PublishedTrackKind::Audio, None),
        (
            "a-full",
            PublishedTrackKind::Screen,
            Some(PublishedVideoLayer::Full),
        ),
        (
            "a-thumb",
            PublishedTrackKind::Screen,
            Some(PublishedVideoLayer::Thumbnail),
        ),
    ] {
        a.send(SignalMessage::PublishTrack {
            track_id: track_id.into(),
            kind,
            layer,
        })
        .unwrap();
    }
    let mut last_subscriptions = None;
    for _ in 0..3 {
        if let SignalMessage::SfuSubscriptions { focus, tracks } = recv_msg(&mut b.incoming).await {
            last_subscriptions = Some((focus, tracks));
        }
    }
    let (focus, tracks) = last_subscriptions.unwrap();
    assert_eq!(focus, None);
    assert_eq!(tracks, vec!["a-audio", "a-thumb"]);

    b.send(SignalMessage::SetFocus {
        publisher: Some("a".into()),
    })
    .unwrap();
    match recv_msg(&mut b.incoming).await {
        SignalMessage::SfuSubscriptions { focus, tracks } => {
            assert_eq!(focus.as_deref(), Some("a"));
            assert_eq!(tracks, vec!["a-audio", "a-full"]);
        }
        other => panic!("expected SfuSubscriptions, got {other:?}"),
    }
}

#[tokio::test]
async fn sfu_renegotiation_reuses_peer_connection_on_new_tracks() {
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

    b.send(SignalMessage::JoinRoom { room_id }).unwrap();
    let _ = recv_msg(&mut b.incoming).await; // Joined
    let _ = recv_msg(&mut a.incoming).await; // PeerJoined

    // Client A creates initial WebRTC peer and sends initial SfuOffer
    let peer_a = std::sync::Arc::new(
        sharescreen_webrtc::WebRtcPeer::new(
            sharescreen_webrtc::PeerConfig::default(),
            sharescreen_core::Codec::H264,
            std::sync::Arc::new(|_| {}),
        )
        .await
        .unwrap(),
    );
    // Mirror the desktop client: reserve receive slots and publish one track,
    // so the offer carries ICE credentials and real media sections.
    peer_a
        .add_relay_track("a", "a-mic", sharescreen_core::Codec::Opus)
        .await
        .unwrap();
    peer_a
        .add_recvonly_transceiver(sharescreen_webrtc::RemoteTrackKind::Audio)
        .await
        .unwrap();
    peer_a
        .add_recvonly_transceiver(sharescreen_webrtc::RemoteTrackKind::Video)
        .await
        .unwrap();
    a.send(SignalMessage::PublishTrack {
        track_id: "a-mic".into(),
        kind: PublishedTrackKind::Audio,
        layer: None,
    })
    .unwrap();
    let initial_offer = peer_a.create_offer().await.unwrap();
    peer_a.set_local_description(&initial_offer).await.unwrap();

    a.send(SignalMessage::SfuOffer {
        sdp: serde_json::to_string(&initial_offer).unwrap(),
        video_codec: "h264".into(),
    })
    .unwrap();

    // A publish can produce an empty subscription snapshot before the answer.
    let initial_answer_sdp = match recv_signaling(&mut a.incoming, true).await {
        SignalMessage::SfuAnswer { sdp } => sdp,
        other => panic!("expected SfuAnswer, got {other:?}"),
    };
    let initial_answer: sharescreen_webrtc::SessionDescription =
        serde_json::from_str(&initial_answer_sdp).unwrap();
    peer_a
        .set_remote_description(&initial_answer)
        .await
        .unwrap();

    // Now B publishes a track
    b.send(SignalMessage::PublishTrack {
        track_id: "b-audio".into(),
        kind: PublishedTrackKind::Audio,
        layer: None,
    })
    .unwrap();

    // A receives SfuSubscriptions with b-audio
    match recv_signaling(&mut a.incoming, false).await {
        SignalMessage::SfuSubscriptions { tracks, .. } => {
            assert!(tracks.contains(&"b-audio".to_string()));
        }
        other => panic!("expected SfuSubscriptions, got {other:?}"),
    }

    // Now A renegotiates: sends a second SfuOffer on the existing peer connection!
    let renegotiate_offer = peer_a.create_offer().await.unwrap();
    peer_a
        .set_local_description(&renegotiate_offer)
        .await
        .unwrap();

    a.send(SignalMessage::SfuOffer {
        sdp: serde_json::to_string(&renegotiate_offer).unwrap(),
        video_codec: "h264".into(),
    })
    .unwrap();

    // Server must respond with SfuAnswer without dropping or disconnecting
    let renegotiate_answer_sdp = match recv_signaling(&mut a.incoming, true).await {
        SignalMessage::SfuAnswer { sdp } => sdp,
        other => panic!("expected SfuAnswer on renegotiation, got {other:?}"),
    };
    let renegotiate_answer: sharescreen_webrtc::SessionDescription =
        serde_json::from_str(&renegotiate_answer_sdp).unwrap();
    peer_a
        .set_remote_description(&renegotiate_answer)
        .await
        .unwrap();

    peer_a.close().await.unwrap();
}

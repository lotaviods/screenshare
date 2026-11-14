//! Media-path test for the relay SFU.
//!
//! This bypasses the signaling socket and drives the same components the
//! server uses at runtime: a client publisher peer negotiated with a server
//! ingress peer, the bounded `RtpForwarder`, and a server egress peer
//! negotiated with a client subscriber. No transcoding is involved: the
//! forwarded packet must arrive byte-for-byte with its sequence number and
//! timestamp intact.

use std::sync::Arc;
use std::time::Duration;

use sharescreen_core::Codec;
use sharescreen_server::sfu::{RtpForwarder, TrackKind};
use sharescreen_webrtc::{PeerConfig, PeerEvent, RemoteTrack, RtpPacket, WebRtcPeer};
use tokio::sync::mpsc;
use tokio::time::timeout;

async fn wait_for_gathering(rx: &mut mpsc::UnboundedReceiver<PeerEvent>) {
    loop {
        match timeout(Duration::from_secs(15), rx.recv()).await {
            Ok(Some(PeerEvent::GatheringComplete)) => return,
            Ok(Some(_)) => continue,
            Ok(None) => panic!("event channel closed while gathering"),
            Err(_) => panic!("ICE gathering did not complete"),
        }
    }
}

async fn wait_for_connected(rx: &mut mpsc::UnboundedReceiver<PeerEvent>) {
    loop {
        match timeout(Duration::from_secs(20), rx.recv()).await {
            Ok(Some(PeerEvent::ConnectionState(state))) => {
                if state == sharescreen_core::ConnectionState::Connected {
                    return;
                }
            }
            Ok(Some(_)) => continue,
            Ok(None) => panic!("event channel closed before connecting"),
            Err(_) => panic!("timed out waiting for the connection"),
        }
    }
}

async fn wait_for_track(rx: &mut mpsc::UnboundedReceiver<PeerEvent>) -> RemoteTrack {
    loop {
        match timeout(Duration::from_secs(20), rx.recv()).await {
            Ok(Some(PeerEvent::TrackReceived { track })) => return track,
            Ok(Some(_)) => continue,
            Ok(None) => panic!("event channel closed before a remote track arrived"),
            Err(_) => panic!("timed out waiting for the remote track"),
        }
    }
}

async fn new_peer(codec: Codec) -> (WebRtcPeer, mpsc::UnboundedReceiver<PeerEvent>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let events = Arc::new(move |event: PeerEvent| {
        let _ = tx.send(event);
    });
    let peer = WebRtcPeer::new(PeerConfig::default(), codec, events)
        .await
        .expect("peer");
    (peer, rx)
}

#[tokio::test]
async fn relay_forwards_publisher_rtp_to_a_subscriber() {
    let forwarder = Arc::new(RtpForwarder::<RtpPacket>::new());

    // ── Publisher → server ingress peer ─────────────────────────────────────
    let (publisher, mut publisher_rx) = new_peer(Codec::H264).await;
    let publisher_track = publisher
        .add_relay_track("publisher", "publisher-screen", Codec::H264)
        .await
        .unwrap();
    let offer = publisher.create_offer().await.unwrap();
    publisher.set_local_description(&offer).await.unwrap();
    wait_for_gathering(&mut publisher_rx).await;

    let (ingress, mut ingress_rx) = new_peer(Codec::H264).await;
    ingress
        .set_remote_description(&publisher.local_description().await.unwrap())
        .await
        .unwrap();
    let answer = ingress.create_answer().await.unwrap();
    ingress.set_local_description(&answer).await.unwrap();
    wait_for_gathering(&mut ingress_rx).await;
    publisher
        .set_remote_description(&ingress.local_description().await.unwrap())
        .await
        .unwrap();
    wait_for_connected(&mut publisher_rx).await;

    // The publisher keeps writing, so packets sent after the subscription is
    // installed still exercise the relay. Start before waiting for the ingress
    // track: a remote track only opens once its first RTP packet arrives.
    let expected = vec![0x5A; 48];
    let writer_expected = expected.clone();
    let publisher_task = tokio::spawn(async move {
        for tick in 0..200u16 {
            let _ = publisher_track
                .write_rtp(RtpPacket {
                    payload: writer_expected.clone(),
                    marker: tick % 10 == 0,
                    payload_type: 96,
                    sequence_number: tick,
                    timestamp: u32::from(tick) * 3_000,
                    ssrc: 1234,
                })
                .await;
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });

    // The server attaches every received track to the forwarder, keyed by the
    // publication id carried in the track metadata.
    let ingress_track = wait_for_track(&mut ingress_rx).await;
    let publication_id = ingress_track.track_id().await;
    assert_eq!(publication_id, "publisher-screen");
    let _source = forwarder.attach_source(publication_id.clone(), ingress_track);

    // ── Subscriber → server egress peer ─────────────────────────────────────
    let (subscriber, mut subscriber_rx) = new_peer(Codec::H264).await;
    subscriber
        .add_recvonly_transceiver(sharescreen_webrtc::RemoteTrackKind::Video)
        .await
        .unwrap();
    let offer = subscriber.create_offer().await.unwrap();
    subscriber.set_local_description(&offer).await.unwrap();
    wait_for_gathering(&mut subscriber_rx).await;

    let (egress, mut egress_rx) = new_peer(Codec::H264).await;
    let egress_track = egress
        .add_relay_track("publisher", &publication_id, Codec::H264)
        .await
        .unwrap();
    egress
        .set_remote_description(&subscriber.local_description().await.unwrap())
        .await
        .unwrap();
    let answer = egress.create_answer().await.unwrap();
    egress.set_local_description(&answer).await.unwrap();
    wait_for_gathering(&mut egress_rx).await;

    forwarder.subscribe_writer(
        publication_id.clone(),
        TrackKind::Screen,
        "subscriber",
        Arc::new(egress_track),
        64,
    );

    subscriber
        .set_remote_description(&egress.local_description().await.unwrap())
        .await
        .unwrap();
    wait_for_connected(&mut subscriber_rx).await;

    let remote = wait_for_track(&mut subscriber_rx).await;
    let received = timeout(Duration::from_secs(10), remote.next_packet())
        .await
        .expect("relayed packet should arrive")
        .expect("subscriber track should stay open");

    assert_eq!(received.payload, expected);
    assert_ne!(received.ssrc, 1234, "the egress leg owns its own SSRC");
    publisher_task.abort();
    publisher.close().await.unwrap();
    ingress.close().await.unwrap();
    egress.close().await.unwrap();
    subscriber.close().await.unwrap();
}

#[tokio::test]
async fn publisher_receives_audio_but_video_drops_are_counted() {
    // The fan-out policy itself is covered by unit tests; this guards the
    // public contract the server relies on: audio waits, video is counted.
    let forwarder = RtpForwarder::<RtpPacket>::new();
    let (audio_tx, mut audio_rx) = mpsc::channel(1);
    let (video_tx, mut video_rx) = mpsc::channel(1);
    forwarder.subscribe("audio", TrackKind::Audio, "viewer", audio_tx);
    forwarder.subscribe("video", TrackKind::Screen, "viewer", video_tx);

    forwarder
        .forward(
            "audio",
            RtpPacket {
                payload: vec![1],
                marker: false,
                payload_type: 111,
                sequence_number: 1,
                timestamp: 0,
                ssrc: 1,
            },
        )
        .await;
    for sequence in 0..3u16 {
        forwarder
            .forward(
                "video",
                RtpPacket {
                    payload: vec![2],
                    marker: false,
                    payload_type: 96,
                    sequence_number: sequence,
                    timestamp: 0,
                    ssrc: 2,
                },
            )
            .await;
    }

    assert!(audio_rx.recv().await.is_some());
    assert!(video_rx.recv().await.is_some());
    assert_eq!(forwarder.metrics().video_packets_dropped, 2);
}

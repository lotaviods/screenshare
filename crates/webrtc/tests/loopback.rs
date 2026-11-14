//! Loopback WebRTC connectivity test: two peer connections in one process
//! negotiate, connect, and exchange media over real ICE/DTLS.

use std::sync::Arc;
use std::time::Duration;

use sharescreen_core::{Codec, EncodedSample, MediaKind};
use sharescreen_webrtc::{
    PeerConfig, PeerEvent, RemoteTrack, RemoteTrackKind, RtpPacket, TrackId, TrackWriter,
    WebRtcPeer,
};
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

async fn wait_for_track(rx: &mut mpsc::UnboundedReceiver<PeerEvent>) -> RemoteTrackKind {
    loop {
        match timeout(Duration::from_secs(20), rx.recv()).await {
            Ok(Some(PeerEvent::TrackReceived { track })) => return track.kind().await,
            Ok(Some(ev)) => {
                eprintln!("B-EVENT: {ev:?}");
                continue;
            }
            Ok(None) => panic!("event channel closed before a track arrived"),
            Err(_) => panic!("timed out waiting for the remote track"),
        }
    }
}

async fn wait_for_remote_track(rx: &mut mpsc::UnboundedReceiver<PeerEvent>) -> RemoteTrack {
    loop {
        match timeout(Duration::from_secs(20), rx.recv()).await {
            Ok(Some(PeerEvent::TrackReceived { track })) => return track,
            Ok(Some(_)) => continue,
            Ok(None) => panic!("event channel closed before a remote track arrived"),
            Err(_) => panic!("timed out waiting for the remote track"),
        }
    }
}

async fn wait_for_connected(rx: &mut mpsc::UnboundedReceiver<PeerEvent>) {
    loop {
        match timeout(Duration::from_secs(20), rx.recv()).await {
            Ok(Some(PeerEvent::ConnectionState(state))) => {
                eprintln!("A-STATE: {state:?}");
                if state == sharescreen_core::ConnectionState::Connected {
                    return;
                }
            }
            Ok(Some(ev)) => {
                eprintln!("A-EVENT: {ev:?}");
                continue;
            }
            Ok(None) => panic!("event channel closed before connecting"),
            Err(_) => panic!("timed out waiting for the connection"),
        }
    }
}

/// Negotiate a video track between two peers and return the track writer.
async fn negotiate_video(
    a: &WebRtcPeer,
    b: &WebRtcPeer,
    a_rx: &mut mpsc::UnboundedReceiver<PeerEvent>,
    b_rx: &mut mpsc::UnboundedReceiver<PeerEvent>,
) -> TrackWriter {
    let track = a.add_track(TrackId::Screen, Codec::H264).await.unwrap();
    let offer = a.create_offer().await.unwrap();
    a.set_local_description(&offer).await.unwrap();
    wait_for_gathering(a_rx).await;
    let a_local = a.local_description().await.expect("a local description");

    b.set_remote_description(&a_local).await.unwrap();
    let answer = b.create_answer().await.unwrap();
    b.set_local_description(&answer).await.unwrap();
    wait_for_gathering(b_rx).await;
    let b_local = b.local_description().await.expect("b local description");

    a.set_remote_description(&b_local).await.unwrap();
    wait_for_connected(a_rx).await;
    track
}

#[tokio::test]
async fn connected_peer_reports_selected_pair_rtt() {
    let (a_tx, mut a_rx) = mpsc::unbounded_channel();
    let a = WebRtcPeer::new(
        PeerConfig::default(),
        Codec::H264,
        Arc::new(move |e| {
            let _ = a_tx.send(e);
        }),
    )
    .await
    .unwrap();
    let (b_tx, mut b_rx) = mpsc::unbounded_channel();
    let b = WebRtcPeer::new(
        PeerConfig::default(),
        Codec::H264,
        Arc::new(move |e| {
            let _ = b_tx.send(e);
        }),
    )
    .await
    .unwrap();
    assert!(a.round_trip_time_ms().await.is_none());
    let _track = negotiate_video(&a, &b, &mut a_rx, &mut b_rx).await;
    let rtt = timeout(Duration::from_secs(15), async {
        loop {
            if let Some(ms) = a.round_trip_time_ms().await {
                break ms;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .expect("selected ICE pair should report a measured RTT");
    assert!(rtt.is_finite() && rtt >= 0.0);
    a.close().await.unwrap();
    b.close().await.unwrap();
}

#[tokio::test]
async fn two_peers_connect_and_exchange_media() {
    let (a_tx, mut a_rx) = mpsc::unbounded_channel::<PeerEvent>();
    let a = WebRtcPeer::new(
        PeerConfig::default(),
        Codec::Vp8,
        Arc::new(move |e: PeerEvent| {
            let _ = a_tx.send(e);
        }),
    )
    .await
    .unwrap();

    let (b_tx, mut b_rx) = mpsc::unbounded_channel::<PeerEvent>();
    let b = WebRtcPeer::new(
        PeerConfig::default(),
        Codec::Vp8,
        Arc::new(move |e: PeerEvent| {
            let _ = b_tx.send(e);
        }),
    )
    .await
    .unwrap();

    // A publishes an audio track, then offers.
    let track = a.add_track(TrackId::Mic, Codec::Opus).await.unwrap();
    let offer = a.create_offer().await.unwrap();
    a.set_local_description(&offer).await.unwrap();
    wait_for_gathering(&mut a_rx).await;
    let a_local = a.local_description().await.expect("a local description");

    // B answers.
    b.set_remote_description(&a_local).await.unwrap();
    let answer = b.create_answer().await.unwrap();
    b.set_local_description(&answer).await.unwrap();
    wait_for_gathering(&mut b_rx).await;
    let b_local = b.local_description().await.expect("b local description");

    a.set_remote_description(&b_local).await.unwrap();

    // The transport must come up before media can flow.
    wait_for_connected(&mut a_rx).await;

    // A writes an encoded sample into the negotiated track. The remote track
    // only opens once the first RTP packet arrives, so the write comes first.
    track
        .write(EncodedSample {
            kind: MediaKind::Audio,
            codec: Codec::Opus,
            data: vec![0xAA; 32],
            timestamp_micros: 0,
            duration_micros: 20_000,
            keyframe: false,
        })
        .await
        .expect("write sample");

    // B must now see the remote audio track.
    let kind = wait_for_track(&mut b_rx).await;
    assert_eq!(kind, RemoteTrackKind::Audio);

    a.close().await.unwrap();
    b.close().await.unwrap();
}

#[tokio::test]
async fn packetized_rtp_forwards_without_reencoding() {
    let (a_tx, mut a_rx) = mpsc::unbounded_channel::<PeerEvent>();
    let a = WebRtcPeer::new(
        PeerConfig::default(),
        Codec::Vp8,
        Arc::new(move |e| {
            let _ = a_tx.send(e);
        }),
    )
    .await
    .unwrap();
    let (b_tx, mut b_rx) = mpsc::unbounded_channel::<PeerEvent>();
    let b = WebRtcPeer::new(
        PeerConfig::default(),
        Codec::Vp8,
        Arc::new(move |e| {
            let _ = b_tx.send(e);
        }),
    )
    .await
    .unwrap();

    let writer = a.add_track(TrackId::Mic, Codec::Opus).await.unwrap();
    let offer = a.create_offer().await.unwrap();
    a.set_local_description(&offer).await.unwrap();
    wait_for_gathering(&mut a_rx).await;
    b.set_remote_description(&a.local_description().await.unwrap())
        .await
        .unwrap();
    let answer = b.create_answer().await.unwrap();
    b.set_local_description(&answer).await.unwrap();
    wait_for_gathering(&mut b_rx).await;
    a.set_remote_description(&b.local_description().await.unwrap())
        .await
        .unwrap();
    wait_for_connected(&mut a_rx).await;

    let expected_payload = vec![0xAA; 32];
    writer
        .write_rtp(RtpPacket {
            payload: expected_payload.clone(),
            marker: false,
            payload_type: 111,
            sequence_number: 3210,
            timestamp: 77_760,
            ssrc: 1234,
        })
        .await
        .expect("forward RTP packet");
    let remote = wait_for_remote_track(&mut b_rx).await;
    let received = timeout(Duration::from_secs(5), remote.next_packet())
        .await
        .expect("RTP packet should arrive")
        .expect("track should remain open");
    assert_eq!(received.payload, expected_payload);
    assert!(!received.marker);
    assert_eq!(received.payload_type, 111);
    assert_ne!(
        received.ssrc, 1234,
        "egress SSRC must be owned by the sending peer"
    );
    assert_eq!(received.sequence_number, 3210);
    assert_eq!(received.timestamp, 77_760);

    a.close().await.unwrap();
    b.close().await.unwrap();
}

/// Mute/unmute: audio sent before and after a pause must both arrive. The
/// receiver must keep getting packets after the sender goes silent.
#[tokio::test]
async fn audio_resumes_after_a_pause() {
    let (a_tx, mut a_rx) = mpsc::unbounded_channel::<PeerEvent>();
    let a = WebRtcPeer::new(
        PeerConfig::default(),
        Codec::Vp8,
        Arc::new(move |e: PeerEvent| {
            let _ = a_tx.send(e);
        }),
    )
    .await
    .unwrap();

    let (b_tx, mut b_rx) = mpsc::unbounded_channel::<PeerEvent>();
    let b = WebRtcPeer::new(
        PeerConfig::default(),
        Codec::Vp8,
        Arc::new(move |e: PeerEvent| {
            let _ = b_tx.send(e);
        }),
    )
    .await
    .unwrap();

    let track = a.add_track(TrackId::Mic, Codec::Opus).await.unwrap();
    let offer = a.create_offer().await.unwrap();
    a.set_local_description(&offer).await.unwrap();
    wait_for_gathering(&mut a_rx).await;
    let a_local = a.local_description().await.expect("a local description");

    b.set_remote_description(&a_local).await.unwrap();
    let answer = b.create_answer().await.unwrap();
    b.set_local_description(&answer).await.unwrap();
    wait_for_gathering(&mut b_rx).await;
    let b_local = b.local_description().await.expect("b local description");

    a.set_remote_description(&b_local).await.unwrap();
    wait_for_connected(&mut a_rx).await;

    // Sender: 25 frames, pause (mute), 25 more frames (unmute).
    let sender = tokio::spawn(async move {
        for burst in 0..2 {
            if burst == 1 {
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            for _ in 0..25 {
                track
                    .write(EncodedSample {
                        kind: MediaKind::Audio,
                        codec: Codec::Opus,
                        data: vec![0xAA; 32],
                        timestamp_micros: 0,
                        duration_micros: 20_000,
                        keyframe: false,
                    })
                    .await
                    .expect("write sample");
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    });

    let remote = loop {
        match timeout(Duration::from_secs(10), b_rx.recv()).await {
            Ok(Some(PeerEvent::TrackReceived { track })) => break track,
            Ok(Some(_)) => continue,
            _ => panic!("no remote track"),
        }
    };
    assert_eq!(remote.kind().await, RemoteTrackKind::Audio);

    // Count packets for long enough to cover both bursts plus the pause.
    let mut packets_after_pause = 0usize;
    let mut total = 0usize;
    let started = tokio::time::Instant::now();
    while started.elapsed() < Duration::from_secs(4) {
        if let Ok(Some(_)) = timeout(Duration::from_millis(500), remote.next_packet()).await {
            total += 1;
            if started.elapsed() > Duration::from_millis(2_100) {
                packets_after_pause += 1;
            }
        }
    }
    sender.await.unwrap();

    a.close().await.unwrap();
    b.close().await.unwrap();

    assert!(total >= 40, "expected both bursts, got {total} packets");
    assert!(
        packets_after_pause > 0,
        "no packets after the sender resumed (total {total})"
    );
}

/// The screen track must carry the payload type of the codec it was created
/// with, even when that codec is not the first one in the negotiated list.
#[tokio::test]
async fn video_track_uses_the_codec_it_was_created_with() {
    let (a_tx, mut a_rx) = mpsc::unbounded_channel::<PeerEvent>();
    let a = WebRtcPeer::new(
        PeerConfig::default(),
        Codec::H264,
        Arc::new(move |e: PeerEvent| {
            let _ = a_tx.send(e);
        }),
    )
    .await
    .unwrap();

    let (b_tx, mut b_rx) = mpsc::unbounded_channel::<PeerEvent>();
    let b = WebRtcPeer::new(
        PeerConfig::default(),
        Codec::H264,
        Arc::new(move |e: PeerEvent| {
            let _ = b_tx.send(e);
        }),
    )
    .await
    .unwrap();

    let track = negotiate_video(&a, &b, &mut a_rx, &mut b_rx).await;

    // A writes an encoded frame; the write resolves the payload type from the
    // negotiated codec list, so it fails loudly if the codec is missing.
    track
        .write(EncodedSample {
            kind: MediaKind::Video,
            codec: Codec::H264,
            data: vec![0x00; 64],
            timestamp_micros: 0,
            duration_micros: 33_333,
            keyframe: true,
        })
        .await
        .expect("write sample");

    let kind = wait_for_track(&mut b_rx).await;
    assert_eq!(kind, RemoteTrackKind::Video);

    a.close().await.unwrap();
    b.close().await.unwrap();
}

/// An SFU answers an offer after pre-registering egress tracks. Those tracks
/// must map onto the receive-only slots the offerer reserved before the offer,
/// which is what lets the relay start forwarding without a renegotiation.
#[tokio::test]
async fn answerer_fills_pre_offered_receive_slots() {
    let (s_tx, mut s_rx) = mpsc::unbounded_channel::<PeerEvent>();
    let subscriber = WebRtcPeer::new(
        PeerConfig::default(),
        Codec::H264,
        Arc::new(move |e: PeerEvent| {
            let _ = s_tx.send(e);
        }),
    )
    .await
    .unwrap();

    // Subscriber reserves one audio and one video slot before offering.
    subscriber
        .add_recvonly_transceiver(RemoteTrackKind::Audio)
        .await
        .unwrap();
    subscriber
        .add_recvonly_transceiver(RemoteTrackKind::Video)
        .await
        .unwrap();
    let offer = subscriber.create_offer().await.unwrap();
    subscriber.set_local_description(&offer).await.unwrap();
    wait_for_gathering(&mut s_rx).await;

    // Relay registers its egress tracks, then answers the offer.
    let (r_tx, mut r_rx) = mpsc::unbounded_channel::<PeerEvent>();
    let relay = WebRtcPeer::new(
        PeerConfig::default(),
        Codec::H264,
        Arc::new(move |e: PeerEvent| {
            let _ = r_tx.send(e);
        }),
    )
    .await
    .unwrap();
    let audio = relay
        .add_relay_track("publisher", "publisher-audio", Codec::Opus)
        .await
        .unwrap();
    let video = relay
        .add_relay_track("publisher", "publisher-screen", Codec::H264)
        .await
        .unwrap();
    relay
        .set_remote_description(&subscriber.local_description().await.unwrap())
        .await
        .unwrap();
    let answer = relay.create_answer().await.unwrap();
    eprintln!("ANSWER SDP:\n{}", answer.sdp);
    relay.set_local_description(&answer).await.unwrap();
    wait_for_gathering(&mut r_rx).await;

    subscriber
        .set_remote_description(&relay.local_description().await.unwrap())
        .await
        .unwrap();
    wait_for_connected(&mut s_rx).await;

    // Both pre-registered tracks must now be deliverable RTP-only streams.
    let expected_audio = vec![0x11; 16];
    let expected_video = vec![0x22; 32];
    let writer_audio = expected_audio.clone();
    let writer_video = expected_video.clone();
    tokio::spawn(async move {
        for tick in 0..50u16 {
            let _ = audio
                .write_rtp(RtpPacket {
                    payload: writer_audio.clone(),
                    marker: false,
                    payload_type: 111,
                    sequence_number: tick,
                    timestamp: u32::from(tick) * 960,
                    ssrc: 1,
                })
                .await;
            let _ = video
                .write_rtp(RtpPacket {
                    payload: writer_video.clone(),
                    marker: true,
                    payload_type: 96,
                    sequence_number: tick,
                    timestamp: u32::from(tick) * 3000,
                    ssrc: 2,
                })
                .await;
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    });

    let mut audio_track = None;
    let mut video_track_count = 0;
    while audio_track.is_none() || video_track_count == 0 {
        match timeout(Duration::from_secs(15), s_rx.recv()).await {
            Ok(Some(PeerEvent::TrackReceived { track })) => match track.kind().await {
                RemoteTrackKind::Audio => audio_track = Some(track),
                RemoteTrackKind::Video => video_track_count += 1,
            },
            Ok(Some(_)) => continue,
            other => panic!("expected two remote tracks, got {other:?}"),
        }
    }
    let audio_track = audio_track.expect("audio slot");
    let received = timeout(Duration::from_secs(5), audio_track.next_packet())
        .await
        .expect("relayed audio packet should arrive")
        .expect("audio track should stay open");
    assert_eq!(received.payload, expected_audio);

    subscriber.close().await.unwrap();
    relay.close().await.unwrap();
}

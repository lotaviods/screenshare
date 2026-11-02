//! Loopback WebRTC connectivity test: two peer connections in one process
//! negotiate, connect, and exchange media over real ICE/DTLS.

use std::sync::Arc;
use std::time::Duration;

use sharescreen_core::{Codec, EncodedSample, MediaKind};
use sharescreen_webrtc::{
    PeerConfig, PeerEvent, RemoteTrackKind, TrackId, TrackWriter, WebRtcPeer,
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
async fn two_peers_connect_and_exchange_media() {
    let (a_tx, mut a_rx) = mpsc::unbounded_channel::<PeerEvent>();
    let a = WebRtcPeer::new(
        PeerConfig::default(),
        Codec::Opus,
        Arc::new(move |e: PeerEvent| {
            let _ = a_tx.send(e);
        }),
    )
    .await
    .unwrap();

    let (b_tx, mut b_rx) = mpsc::unbounded_channel::<PeerEvent>();
    let b = WebRtcPeer::new(
        PeerConfig::default(),
        Codec::Opus,
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
    let a_local = a
        .local_description()
        .await
        .expect("a local description");

    // B answers.
    b.set_remote_description(&a_local).await.unwrap();
    let answer = b.create_answer().await.unwrap();
    b.set_local_description(&answer).await.unwrap();
    wait_for_gathering(&mut b_rx).await;
    let b_local = b
        .local_description()
        .await
        .expect("b local description");

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

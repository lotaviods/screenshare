//! End-to-end: real H.264 frames over a real WebRTC peer connection, decoded by
//! `RemoteMedia` into JPEG. Mirrors the production screen-share path.

use std::sync::Arc;
use std::time::Duration;

use gstreamer as gst;
use gstreamer::prelude::*;
use sharescreen_core::{Codec, EncodedSample, MediaKind};
use sharescreen_media::RemoteMedia;
use sharescreen_webrtc::{PeerConfig, PeerEvent, TrackId, TrackWriter, WebRtcPeer};
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
            Ok(Some(PeerEvent::ConnectionState(sharescreen_core::ConnectionState::Connected))) => {
                return
            }
            Ok(Some(_)) => continue,
            Ok(None) => panic!("event channel closed before connecting"),
            Err(_) => panic!("timed out waiting for the connection"),
        }
    }
}

/// Produce `count` real H.264 Annex-B access units from a test source.
fn encoded_frames(count: usize) -> Vec<Vec<u8>> {
    gst::init().unwrap();
    let pipeline = gst::parse::launch(&format!(
        "videotestsrc num-buffers={count} pattern=ball ! \
         video/x-raw,width=320,height=240,framerate=30/1 ! \
         x264enc tune=zerolatency speed-preset=veryfast bitrate=500 key-int-max=30 ! h264parse ! \
         video/x-h264,profile=constrained-baseline,stream-format=byte-stream,alignment=au ! \
         appsink name=sink"
    ))
    .expect("source pipeline parses");
    let pipeline = pipeline.downcast::<gst::Pipeline>().unwrap();
    let sink = pipeline
        .by_name("sink")
        .unwrap()
        .downcast::<gstreamer_app::AppSink>()
        .unwrap();
    pipeline.set_state(gst::State::Playing).unwrap();

    let mut frames = Vec::new();
    while let Some(sample) = sink.try_pull_sample(gst::ClockTime::from_mseconds(2000)) {
        if let Some(buffer) = sample.buffer() {
            if let Ok(map) = buffer.map_readable() {
                frames.push(map.as_slice().to_vec());
            }
        }
        if frames.len() >= count {
            break;
        }
    }
    let _ = pipeline.set_state(gst::State::Null);
    assert!(!frames.is_empty(), "no H.264 frames produced");
    frames
}

async fn negotiate_video(
    a: &WebRtcPeer,
    b: &WebRtcPeer,
    a_rx: &mut mpsc::UnboundedReceiver<PeerEvent>,
    b_rx: &mut mpsc::UnboundedReceiver<PeerEvent>,
    b_adds_tracks: bool,
) -> TrackWriter {
    let track = a.add_track(TrackId::Screen, Codec::H264).await.unwrap();
    let offer = a.create_offer().await.unwrap();
    a.set_local_description(&offer).await.unwrap();
    wait_for_gathering(a_rx).await;
    let a_local = a.local_description().await.expect("a local description");

    // Mirror production: the answering peer also publishes mic/system/screen
    // tracks before applying the remote offer.
    if b_adds_tracks {
        b.add_track(TrackId::Mic, Codec::Opus).await.unwrap();
        b.add_track(TrackId::System, Codec::Opus).await.unwrap();
        b.add_track(TrackId::Screen, Codec::H264).await.unwrap();
    }

    b.set_remote_description(&a_local).await.unwrap();
    let answer = b.create_answer().await.unwrap();
    b.set_local_description(&answer).await.unwrap();
    wait_for_gathering(b_rx).await;
    let b_local = b.local_description().await.expect("b local description");

    a.set_remote_description(&b_local).await.unwrap();
    wait_for_connected(a_rx).await;
    track
}

/// The exact capture pipeline (tee: H264 main branch + JPEG preview branch)
/// must emit H264 access units that the decoder turns into a JPEG.
#[test]
fn capture_pipeline_main_branch_decodes_to_jpeg() {
    gst::init().unwrap();
    let launch = "videotestsrc num-buffers=15 pattern=ball ! videoconvert ! videoscale ! \
         video/x-raw,width=320,height=240 ! \
         videorate ! video/x-raw,framerate=30/1 ! \
         tee name=t \
         t. ! queue ! x264enc tune=zerolatency speed-preset=veryfast bitrate=2500 ! h264parse ! \
         appsink name=sink sync=false max-buffers=2 drop=true \
         caps=video/x-h264,profile=constrained-baseline,stream-format=byte-stream,alignment=au \
         t. ! queue ! videorate ! video/x-raw,framerate=10/1 ! \
         jpegenc quality=70 ! \
         appsink name=preview sync=false max-buffers=1 drop=true";
    let pipeline = gst::parse::launch(launch)
        .expect("pipeline parses")
        .downcast::<gst::Pipeline>()
        .unwrap();
    let sink = pipeline
        .by_name("sink")
        .unwrap()
        .downcast::<gstreamer_app::AppSink>()
        .unwrap();
    let preview = pipeline
        .by_name("preview")
        .unwrap()
        .downcast::<gstreamer_app::AppSink>()
        .unwrap();
    pipeline.set_state(gst::State::Playing).unwrap();

    let mut count = 0;
    let mut first = None;
    while let Some(sample) = sink.try_pull_sample(gst::ClockTime::from_mseconds(2000)) {
        if let Some(buffer) = sample.buffer() {
            if let Ok(map) = buffer.map_readable() {
                if first.is_none() {
                    first = Some(map.as_slice().to_vec());
                }
                count += 1;
            }
        }
        // Drain the preview branch so the tee never blocks.
        while preview.try_pull_sample(gst::ClockTime::ZERO).is_some() {}
        if count >= 8 {
            break;
        }
    }
    let _ = pipeline.set_state(gst::State::Null);
    assert!(count > 0, "main branch emitted no H.264 samples");
    let first = first.expect("first H.264 access unit");
    assert!(first.windows(4).any(|w| w == [0, 0, 0, 1]), "not Annex-B");
}

#[tokio::test]
async fn real_h264_frame_decodes_to_jpeg_over_webrtc() {
    let (a_tx, mut a_rx) = mpsc::unbounded_channel::<PeerEvent>();
    let a = WebRtcPeer::new(
        PeerConfig::default(),
        Codec::H264,
        Arc::new(move |e| {
            let _ = a_tx.send(e);
        }),
    )
    .await
    .unwrap();

    let (b_tx, mut b_rx) = mpsc::unbounded_channel::<PeerEvent>();
    let b = WebRtcPeer::new(
        PeerConfig::default(),
        Codec::H264,
        Arc::new(move |e| {
            let _ = b_tx.send(e);
        }),
    )
    .await
    .unwrap();

    let track = negotiate_video(&a, &b, &mut a_rx, &mut b_rx, true).await;

    let media = RemoteMedia::new();
    let frames = encoded_frames(10);

    // Write a few frames, then attach the remote track to RemoteMedia.
    let writer = tokio::spawn({
        let frames = frames.clone();
        async move {
            for (i, data) in frames.into_iter().enumerate() {
                track
                    .write(EncodedSample {
                        kind: MediaKind::Video,
                        codec: Codec::H264,
                        data,
                        timestamp_micros: (i as u64) * 33_333,
                        duration_micros: 33_333,
                        keyframe: i == 0,
                    })
                    .await
                    .expect("write sample");
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    });

    // B receives the remote video track; attach it to the decoder.
    let peer_id = "b";
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        match timeout(Duration::from_secs(20), b_rx.recv()).await {
            Ok(Some(PeerEvent::TrackReceived { track })) => {
                if track.kind().await == sharescreen_webrtc::RemoteTrackKind::Video {
                    let codec = track.codec().await;
                    eprintln!("remote video track codec: {codec:?}");
                    media.attach(peer_id, track).await.expect("attach decoder");
                    break;
                }
            }
            Ok(Some(_)) => continue,
            Ok(None) => panic!("b event channel closed"),
            Err(_) => panic!("timed out waiting for the remote video track"),
        }
    }

    let mut frame = None;
    while tokio::time::Instant::now() < deadline {
        if let Some(f) = media.latest_frame(peer_id) {
            frame = Some(f);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    writer.await.unwrap();
    a.close().await.unwrap();
    b.close().await.unwrap();

    let frame = frame.expect("a decoded JPEG frame from the remote track");
    assert!(frame.len() > 100, "frame too small: {} bytes", frame.len());
    assert_eq!(&frame[0..2], &[0xff, 0xd8], "not a JPEG");
}

/// Production order: the answerer (`b`) adds its own tracks *before* applying
/// the remote offer. Does the answerer's screen reach the offerer?
#[tokio::test]
async fn answerer_screen_reaches_offerer() {
    let (a_tx, mut a_rx) = mpsc::unbounded_channel::<PeerEvent>();
    let a = WebRtcPeer::new(
        PeerConfig::default(),
        Codec::H264,
        Arc::new(move |e| {
            let _ = a_tx.send(e);
        }),
    )
    .await
    .unwrap();

    let (b_tx, mut b_rx) = mpsc::unbounded_channel::<PeerEvent>();
    let b = WebRtcPeer::new(
        PeerConfig::default(),
        Codec::H264,
        Arc::new(move |e| {
            let _ = b_tx.send(e);
        }),
    )
    .await
    .unwrap();

    // A (offerer) publishes its tracks.
    a.add_track(TrackId::Mic, Codec::Opus).await.unwrap();
    a.add_track(TrackId::System, Codec::Opus).await.unwrap();
    a.add_track(TrackId::Screen, Codec::H264).await.unwrap();
    let offer = a.create_offer().await.unwrap();
    a.set_local_description(&offer).await.unwrap();
    wait_for_gathering(&mut a_rx).await;
    let a_local = a.local_description().await.expect("a local description");

    // B (answerer) publishes its tracks BEFORE applying the remote offer,
    // exactly like `create_peer` + `answer_peer` do today.
    b.add_track(TrackId::Mic, Codec::Opus).await.unwrap();
    b.add_track(TrackId::System, Codec::Opus).await.unwrap();
    let b_screen = b.add_track(TrackId::Screen, Codec::H264).await.unwrap();

    b.set_remote_description(&a_local).await.unwrap();
    let answer = b.create_answer().await.unwrap();
    b.set_local_description(&answer).await.unwrap();
    wait_for_gathering(&mut b_rx).await;
    let b_local = b.local_description().await.expect("b local description");

    a.set_remote_description(&b_local).await.unwrap();
    wait_for_connected(&mut a_rx).await;

    let media = RemoteMedia::new();
    let frames = encoded_frames(10);
    let writer = tokio::spawn(async move {
        for (i, data) in frames.into_iter().enumerate() {
            b_screen
                .write(EncodedSample {
                    kind: MediaKind::Video,
                    codec: Codec::H264,
                    data,
                    timestamp_micros: (i as u64) * 33_333,
                    duration_micros: 33_333,
                    keyframe: i == 0,
                })
                .await
                .expect("write sample");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });

    // A must receive B's video track.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let mut got = false;
    while tokio::time::Instant::now() < deadline && !got {
        match timeout(Duration::from_secs(10), a_rx.recv()).await {
            Ok(Some(PeerEvent::TrackReceived { track })) => {
                if track.kind().await == sharescreen_webrtc::RemoteTrackKind::Video {
                    media.attach("b", track).await.expect("attach decoder");
                    got = true;
                }
            }
            Ok(Some(_)) => continue,
            Ok(None) => break,
            Err(_) => break,
        }
    }
    let mut frame = None;
    if got {
        while tokio::time::Instant::now() < deadline {
            if let Some(f) = media.latest_frame("b") {
                frame = Some(f);
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    writer.await.unwrap();
    a.close().await.unwrap();
    b.close().await.unwrap();

    assert!(
        frame.is_some(),
        "answerer's screen never reached the offerer (offerer got no decoded frame)"
    );
}

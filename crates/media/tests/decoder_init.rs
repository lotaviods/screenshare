//! Regression: `Decoder::new` must construct a native decoder without any
//! prior initialization.

#[test]
fn decoder_constructs_without_prior_init() {
    let decoder = sharescreen_media::Decoder::new(
        sharescreen_webrtc::RemoteTrackKind::Video,
        sharescreen_core::Codec::H264,
    )
    .expect("video decoder constructs without a panic");
    decoder.stop();
}

#[test]
fn video_decoder_lifecycle_repeated_open_and_close() {
    for _ in 0..100 {
        let decoder = sharescreen_media::Decoder::new(
            sharescreen_webrtc::RemoteTrackKind::Video,
            sharescreen_core::Codec::H264,
        )
        .expect("video decoder must construct cleanly");
        decoder.push_rtp(vec![0x00, 0x00, 0x00, 0x01, 0x07]); // partial NAL
        decoder.stop();
    }
}

#[test]
fn audio_decoder_lifecycle_thread_exit_and_teardown() {
    for _ in 0..100 {
        let decoder = sharescreen_media::Decoder::new(
            sharescreen_webrtc::RemoteTrackKind::Audio,
            sharescreen_core::Codec::Opus,
        )
        .expect("audio decoder must construct without panicking");
        decoder.push_rtp(vec![
            0x80, 0x70, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ]);
        decoder.stop();
    }
}

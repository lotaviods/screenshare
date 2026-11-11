//! Regression: `Decoder::new` must construct a native decoder without any
//! prior initialization. It used to panic while building GStreamer caps before
//! `gst::init`.

#[test]
fn decoder_constructs_without_prior_init() {
    let decoder = sharescreen_media::Decoder::new(
        sharescreen_webrtc::RemoteTrackKind::Video,
        sharescreen_core::Codec::H264,
    )
    .expect("video decoder constructs without a panic");
    decoder.stop();
}

//! Regression: `Decoder::new` must work in a process where GStreamer has not
//! been initialized yet (a viewer that never captures). It used to panic while
//! building `gst::Caps` before `gst::init`.

#[test]
fn decoder_initializes_gstreamer_without_prior_init() {
    let decoder = sharescreen_media::Decoder::new(
        sharescreen_webrtc::RemoteTrackKind::Video,
        sharescreen_core::Codec::H264,
    )
    .expect("video decoder constructs without a panic");
    decoder.stop();
}

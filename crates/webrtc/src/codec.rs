//! Codec registration and `Codec` → RTP codec mapping.

use rtc::peer_connection::configuration::media_engine::{
    MediaEngine, MIME_TYPE_H264, MIME_TYPE_OPUS, MIME_TYPE_VP8, MIME_TYPE_VP9,
};
use rtc::rtp_transceiver::rtp_sender::{RtpCodecKind, RTCRtpCodec, RTCRtpCodecParameters};
use webrtc::error::Result;

use sharescreen_core::Codec;

/// Register the codecs Sharescreen uses: Opus audio plus exactly one video
/// codec.
///
/// Only one video codec is registered on purpose: capture encodes with a
/// single codec, so the SDP must offer only that codec. Otherwise a peer could
/// negotiate a different one and receive samples it cannot decode.
pub fn register_default_codecs(media_engine: &mut MediaEngine, video: Codec) -> Result<()> {
    media_engine.register_codec(
        RTCRtpCodecParameters {
            rtp_codec: RTCRtpCodec {
                mime_type: MIME_TYPE_OPUS.to_owned(),
                clock_rate: 48_000,
                channels: 2,
                sdp_fmtp_line: "minptime=10;useinbandfec=1".to_owned(),
                rtcp_feedback: vec![],
            },
            payload_type: 111,
            ..Default::default()
        },
        RtpCodecKind::Audio,
    )?;

    // H264 is registered as Baseline profile (42e01f); the capture crate must
    // encode Baseline when H264 is selected, or the SDP profile won't match the
    // stream. See ARCHITECTURE.md.
    let (mime, pt, fmtp) = match video {
        Codec::Vp9 => (MIME_TYPE_VP9, 98, ""),
        Codec::H264 => (
            MIME_TYPE_H264,
            102,
            "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f",
        ),
        _ => (MIME_TYPE_VP8, 96, ""),
    };
    media_engine.register_codec(
        RTCRtpCodecParameters {
            rtp_codec: RTCRtpCodec {
                mime_type: mime.to_owned(),
                clock_rate: 90_000,
                channels: 0,
                sdp_fmtp_line: fmtp.to_owned(),
                rtcp_feedback: vec![],
            },
            payload_type: pt,
            ..Default::default()
        },
        RtpCodecKind::Video,
    )?;
    Ok(())
}

/// The RTP mime type a codec is carried as.
pub fn codec_mime(codec: Codec) -> &'static str {
    match codec {
        Codec::Opus => MIME_TYPE_OPUS,
        Codec::Vp9 => MIME_TYPE_VP9,
        Codec::H264 => MIME_TYPE_H264,
        _ => MIME_TYPE_VP8,
    }
}

/// Map a Sharescreen [`Codec`] to the RTP codec capability used when creating a
/// local track. Unknown codecs fall back to VP8.
pub fn codec_to_rtp(codec: Codec) -> RTCRtpCodec {
    match codec {
        Codec::Opus => RTCRtpCodec {
            mime_type: MIME_TYPE_OPUS.to_owned(),
            clock_rate: 48_000,
            channels: 2,
            sdp_fmtp_line: String::new(),
            rtcp_feedback: vec![],
        },
        Codec::Vp9 => RTCRtpCodec {
            mime_type: MIME_TYPE_VP9.to_owned(),
            clock_rate: 90_000,
            channels: 0,
            sdp_fmtp_line: String::new(),
            rtcp_feedback: vec![],
        },
        Codec::H264 => RTCRtpCodec {
            mime_type: MIME_TYPE_H264.to_owned(),
            clock_rate: 90_000,
            channels: 0,
            sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
                .to_owned(),
            rtcp_feedback: vec![],
        },
        _ => RTCRtpCodec {
            mime_type: MIME_TYPE_VP8.to_owned(),
            clock_rate: 90_000,
            channels: 0,
            sdp_fmtp_line: String::new(),
            rtcp_feedback: vec![],
        },
    }
}

/// Whether a codec is audio (Opus).
pub fn is_audio(codec: Codec) -> bool {
    matches!(codec, Codec::Opus)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opus_maps_to_audio() {
        assert!(is_audio(Codec::Opus));
        assert!(!is_audio(Codec::Vp8));
        assert!(!is_audio(Codec::H264));
    }

    #[test]
    fn codec_mime_types_are_correct() {
        assert_eq!(codec_to_rtp(Codec::Opus).mime_type, "audio/opus");
        assert_eq!(codec_to_rtp(Codec::Vp8).mime_type, "video/VP8");
        assert_eq!(codec_to_rtp(Codec::Vp9).mime_type, "video/VP9");
        assert_eq!(codec_to_rtp(Codec::H264).mime_type, "video/H264");
        assert_eq!(codec_to_rtp(Codec::H265).mime_type, "video/VP8"); // fallback
    }
}

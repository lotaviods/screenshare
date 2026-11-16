//! WebRTC transport on `webrtcbin`.
//!
//! One [`Transport`] owns one peer connection and one pipeline per peer. It
//! exposes exactly what the signaling layer already carries — SDP text with an
//! offer/answer type, and trickled ICE candidates — while media enters and
//! leaves through request pads, so the caller decides how to encode and how to
//! decode.

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_sdp as sdp;
use gstreamer_webrtc::{WebRTCICEGatheringState, WebRTCSDPType, WebRTCSessionDescription};
use std::time::{Duration, Instant};

/// Offer or answer, as carried by the signaling protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SdpType {
    Offer,
    Answer,
}

impl SdpType {
    fn to_gst(self) -> WebRTCSDPType {
        match self {
            Self::Offer => WebRTCSDPType::Offer,
            Self::Answer => WebRTCSDPType::Answer,
        }
    }

    /// Parse the `type` field of a signaling description.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "offer" => Some(Self::Offer),
            "answer" => Some(Self::Answer),
            _ => None,
        }
    }
}

/// A peer connection built on `webrtcbin`.
pub struct Transport {
    pipeline: gst::Pipeline,
    webrtc: gst::Element,
}

impl Transport {
    /// Build a peer connection named `name` (used in pipeline logs).
    pub fn new(name: &str) -> Result<Self, String> {
        crate::init()?;
        let pipeline = gst::Pipeline::new();
        let webrtc = gst::ElementFactory::make("webrtcbin")
            .name(name)
            .property_from_str("bundle-policy", "max-bundle")
            .property("latency", 0u32)
            .build()
            .map_err(|error| format!("webrtcbin unavailable: {error}"))?;
        pipeline
            .add(&webrtc)
            .map_err(|error| format!("cannot add webrtcbin: {error}"))?;
        Ok(Self { pipeline, webrtc })
    }

    /// The underlying element, for request-pad linking and signals.
    pub fn element(&self) -> &gst::Element {
        &self.webrtc
    }

    /// Add a helper element to this peer's pipeline.
    pub fn add(&self, element: &gst::Element) -> Result<(), String> {
        self.pipeline
            .add(element)
            .map_err(|error| format!("cannot add {}: {error}", element.name()))
    }

    /// Start the connection. SDP can be exchanged before or after.
    pub fn play(&self) -> Result<(), String> {
        self.pipeline
            .set_state(gst::State::Playing)
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    pub fn stop(&self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }

    /// Create the local offer and install it as the local description.
    pub fn create_offer(&self) -> Result<String, String> {
        let offer = self.create_description("create-offer")?;
        let description = WebRTCSessionDescription::new(WebRTCSDPType::Offer, offer);
        self.set_local_description(&description);
        description
            .sdp()
            .as_text()
            .map_err(|error| error.to_string())
    }

    /// Create the local answer to an installed remote offer.
    pub fn create_answer(&self) -> Result<String, String> {
        let answer = self.create_description("create-answer")?;
        let description = WebRTCSessionDescription::new(WebRTCSDPType::Answer, answer);
        self.set_local_description(&description);
        description
            .sdp()
            .as_text()
            .map_err(|error| error.to_string())
    }

    fn create_description(&self, signal: &str) -> Result<sdp::SDPMessage, String> {
        let (tx, rx) = std::sync::mpsc::channel();
        let promise = gst::Promise::with_change_func(move |reply| {
            let result = reply
                .map_err(|error| format!("{error:?}"))
                .and_then(|structure| {
                    let structure = structure
                        .ok_or_else(|| "no description in the promise reply".to_string())?;
                    structure
                        .get::<WebRTCSessionDescription>("offer")
                        .or_else(|_| structure.get::<WebRTCSessionDescription>("answer"))
                        .map(|description| description.sdp().to_owned())
                        .map_err(|error| error.to_string())
                });
            let _ = tx.send(result);
        });
        self.webrtc
            .emit_by_name::<()>(signal, &[&None::<gst::Structure>, &promise]);
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(result) => result,
            Err(_) => Err(format!("{signal} timed out")),
        }
    }

    fn set_local_description(&self, description: &WebRTCSessionDescription) {
        self.webrtc.emit_by_name::<()>(
            "set-local-description",
            &[description, &None::<gst::Promise>],
        );
    }

    /// Install the remote description. `sdp_type` is the type the sender used.
    pub fn set_remote_description(&self, sdp_type: SdpType, text: &str) -> Result<(), String> {
        let message = sdp::SDPMessage::parse_buffer(text.as_bytes())
            .map_err(|error| format!("invalid SDP: {error:?}"))?;
        let description = WebRTCSessionDescription::new(sdp_type.to_gst(), message);
        self.webrtc.emit_by_name::<()>(
            "set-remote-description",
            &[&description, &None::<gst::Promise>],
        );
        Ok(())
    }

    /// Add one trickled candidate. `mline` is the media-line index.
    pub fn add_ice_candidate(&self, mline: u32, candidate: &str) {
        self.webrtc
            .emit_by_name::<()>("add-ice-candidate", &[&mline, &candidate]);
    }

    /// The current local SDP, including every gathered candidate.
    pub fn local_sdp(&self) -> Result<String, String> {
        let description = self
            .webrtc
            .property::<Option<WebRTCSessionDescription>>("local-description")
            .ok_or_else(|| "no local description yet".to_string())?;
        description
            .sdp()
            .as_text()
            .map_err(|error| error.to_string())
    }

    /// Wait until ICE gathering finished and return the complete local SDP.
    ///
    /// Non-trickle mode: the full candidate list rides in the SDP, so one
    /// signaling message is enough to connect. Trickle stays available through
    /// [`Self::add_ice_candidate`].
    pub fn gather(&self, timeout: Duration) -> Result<String, String> {
        let deadline = Instant::now() + timeout;
        let mut last = String::new();
        while Instant::now() < deadline {
            if let Ok(text) = self.local_sdp() {
                let complete = self
                    .webrtc
                    .property::<WebRTCICEGatheringState>("ice-gathering-state")
                    == WebRTCICEGatheringState::Complete;
                if complete && text.contains("a=candidate") {
                    return Ok(text);
                }
                last = text;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Err(format!(
            "ICE gathering did not finish in {timeout:?}; last SDP: {last}"
        ))
    }

    /// Request a sink pad (`sink_%u`) for another outbound stream.
    pub fn request_sink_pad(&self) -> Result<gst::Pad, String> {
        self.webrtc
            .request_pad_simple("sink_%u")
            .ok_or_else(|| "webrtcbin refused a sink pad".to_string())
    }
}

impl Drop for Transport {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn element(factory: &str) -> gst::Element {
        gst::ElementFactory::make(factory)
            .build()
            .unwrap_or_else(|error| panic!("{factory}: {error}"))
    }

    /// Link two elements through their always pads.
    fn link(src: &gst::Element, sink: &gst::Element) {
        let sink_pad = sink.static_pad("sink").expect("sink pads exist");
        src.static_pad("src")
            .expect("src pads exist")
            .link(&sink_pad)
            .expect("pads link");
    }

    /// videotestsrc → VP8 → RTP → a new webrtcbin sink pad.
    fn send_test_video(transport: &Transport) {
        let src = element("videotestsrc");
        src.set_property("num-buffers", 30i32);
        src.set_property("is-live", false);
        let raw = element("capsfilter");
        raw.set_property(
            "caps",
            gst::Caps::builder("video/x-raw")
                .field("width", 320i32)
                .field("height", 240i32)
                .field("framerate", gst::Fraction::new(30, 1))
                .build(),
        );
        let enc = element("vp8enc");
        enc.set_property("deadline", 1i64);
        let pay = element("rtpvp8pay");
        pay.set_property("pt", 96u32);
        let rtp = element("capsfilter");
        rtp.set_property(
            "caps",
            gst::Caps::builder("application/x-rtp")
                .field("media", "video")
                .field("encoding-name", "VP8")
                .field("payload", 96i32)
                .build(),
        );

        for element in [&src, &raw, &enc, &pay, &rtp] {
            transport.add(element).expect("add to pipeline");
        }
        link(&src, &raw);
        link(&raw, &enc);
        link(&enc, &pay);
        link(&pay, &rtp);

        let sink_pad = transport.request_sink_pad().expect("sink pad");
        rtp.static_pad("src")
            .expect("rtp src")
            .link(&sink_pad)
            .expect("link into webrtcbin");
    }

    /// Receive decoded VP8 into an appsink and return it.
    fn receive_video(transport: &Transport) -> gst::Element {
        let depay = element("rtpvp8depay");
        let dec = element("vp8dec");
        let convert = element("videoconvert");
        let sink = element("appsink");
        sink.set_property("sync", false);
        sink.set_property("max-buffers", 8u32);
        sink.set_property("drop", true);
        for element in [&depay, &dec, &convert, &sink] {
            transport.add(element).expect("add to pipeline");
        }
        link(&depay, &dec);
        link(&dec, &convert);
        link(&convert, &sink);

        // webrtcbin creates its src pads when incoming media appears, so the
        // receive chain is linked from `pad-added` rather than requested.
        let depay_for_pad = depay.downgrade();
        transport.element().connect_pad_added(move |_, pad| {
            let Some(depay) = depay_for_pad.upgrade() else {
                return;
            };
            let Some(depay_sink) = depay.static_pad("sink") else {
                return;
            };
            if !depay_sink.is_linked() {
                let _ = pad.link(&depay_sink);
            }
        });
        sink
    }

    #[test]
    fn sdp_types_parse() {
        assert_eq!(SdpType::parse("offer"), Some(SdpType::Offer));
        assert_eq!(SdpType::parse("answer"), Some(SdpType::Answer));
        assert_eq!(SdpType::parse("rollback"), None);
    }

    /// End to end: two peer connections in one process negotiate VP8 and the
    /// answerer decodes real frames. This is the interop the call path needs.
    #[test]
    fn two_peers_exchange_vp8_media() {
        if !crate::available() || !crate::has_element("webrtcbin") {
            return;
        }
        let offerer = Transport::new("offerer").expect("offerer transport");
        let answerer = Transport::new("answerer").expect("answerer transport");
        let frames = receive_video(&answerer);
        send_test_video(&offerer);

        offerer.play().expect("offerer plays");
        offerer.create_offer().expect("offer");
        let offer = offerer
            .gather(Duration::from_secs(5))
            .expect("offerer gathers candidates");
        assert!(offer.contains("a=setup:actpass"), "offer SDP: {offer}");

        answerer.play().expect("answerer plays");
        answerer
            .set_remote_description(SdpType::Offer, &offer)
            .expect("remote offer");
        answerer.create_answer().expect("answer");
        let answer = answerer
            .gather(Duration::from_secs(5))
            .expect("answerer gathers candidates");

        offerer
            .set_remote_description(SdpType::Answer, &answer)
            .expect("remote answer");

        let deadline = Instant::now() + Duration::from_secs(10);
        let mut decoded = 0;
        while Instant::now() < deadline && decoded == 0 {
            if let Some(sample) = frames
                .downcast_ref::<gstreamer_app::AppSink>()
                .expect("appsink")
                .try_pull_sample(gst::ClockTime::from_mseconds(200))
            {
                assert!(sample.buffer().is_some());
                decoded += 1;
            }
        }
        assert!(decoded > 0, "no decoded video frame arrived");
    }
}

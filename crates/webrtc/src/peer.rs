//! Peer connection wrapper and local track writers.

use std::fmt;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use bytes::Bytes;
use rtc::interceptor::{IntervalPliInterceptor, Registry, Slot};
use rtc::media::Sample;
use rtc::media_stream::MediaStreamTrack;
use rtc::peer_connection::configuration::interceptor_registry::register_default_interceptors;
use rtc::peer_connection::configuration::media_engine::MediaEngine;
use rtc::peer_connection::configuration::RTCConfigurationBuilder;
use rtc::peer_connection::sdp::RTCSessionDescription;
use rtc::peer_connection::transport::{RTCIceCandidateInit, RTCIceServer};
use rtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use rtc::rtp_transceiver::rtp_sender::{
    RTCRtpCodingParameters, RTCRtpEncodingParameters, RtpCodecKind,
};
use rtc::rtp_transceiver::PayloadType;
use rtc::statistics::{report::RTCStatsReportEntry, StatsSelector};
use serde::{Deserialize, Serialize};
use webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample;
use webrtc::media_stream::track_local::TrackLocal;
use webrtc::media_stream::track_remote::{TrackRemote, TrackRemoteEvent};
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCIceGatheringState,
    RTCPeerConnectionIceEvent, RTCPeerConnectionState,
};
use webrtc::rtp_transceiver::RtpSender;
use webrtc::runtime::default_runtime;

use sharescreen_core::{Codec, ConnectionState, EncodedSample};

use crate::codec;
use crate::{Result, WebRtcError};

/// An ICE candidate, in the JSON shape used by the signaling layer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IceCandidate {
    pub candidate: String,
    #[serde(rename = "sdpMid", default)]
    pub sdp_mid: Option<String>,
    #[serde(rename = "sdpMLineIndex", default)]
    pub sdp_mline_index: Option<u16>,
    #[serde(rename = "usernameFragment", default)]
    pub username_fragment: Option<String>,
}

/// A session description (offer/answer).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionDescription {
    /// "offer", "answer", "pranswer" or "rollback".
    #[serde(rename = "type")]
    pub sdp_type: String,
    pub sdp: String,
}

/// The kind of media a remote track carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteTrackKind {
    Audio,
    Video,
}

/// One RTP packet received on a remote track.
#[derive(Debug, Clone)]
pub struct RtpPacket {
    pub payload: Vec<u8>,
    pub marker: bool,
    pub payload_type: u8,
    pub sequence_number: u16,
    pub timestamp: u32,
    pub ssrc: u32,
}

impl RtpPacket {
    /// Rebuild the full RTP packet (fixed header + payload) as it appears on
    /// the wire. Contributing sources and header extensions are not carried by
    /// our own streams, so they are omitted.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(12 + self.payload.len());
        out.push(0x80); // version 2, no padding / extension / CSRC
        let mut second = self.payload_type & 0x7f;
        if self.marker {
            second |= 0x80;
        }
        out.push(second);
        out.extend_from_slice(&self.sequence_number.to_be_bytes());
        out.extend_from_slice(&self.timestamp.to_be_bytes());
        out.extend_from_slice(&self.ssrc.to_be_bytes());
        out.extend_from_slice(&self.payload);
        out
    }
}

/// A remote track delivering RTP packets.
pub struct RemoteTrack {
    inner: Arc<dyn TrackRemote>,
}

impl fmt::Debug for RemoteTrack {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RemoteTrack")
    }
}

impl RemoteTrack {
    fn new(inner: Arc<dyn TrackRemote>) -> Self {
        Self { inner }
    }

    /// The media kind of this track.
    pub async fn kind(&self) -> RemoteTrackKind {
        match self.inner.kind().await {
            RtpCodecKind::Audio => RemoteTrackKind::Audio,
            _ => RemoteTrackKind::Video,
        }
    }

    /// The negotiated codec, if known.
    pub async fn codec(&self) -> Option<Codec> {
        self.inner
            .codings()
            .await
            .first()
            .and_then(|c| codec_from_mime(&c.codec.mime_type))
    }

    /// Wait for the next RTP packet. Returns `None` when the track ends.
    pub async fn next_packet(&self) -> Option<RtpPacket> {
        loop {
            match self.inner.poll().await? {
                TrackRemoteEvent::OnRtpPacket(pkt) => {
                    return Some(RtpPacket {
                        payload: pkt.payload.to_vec(),
                        marker: pkt.header.marker,
                        payload_type: pkt.header.payload_type,
                        sequence_number: pkt.header.sequence_number,
                        timestamp: pkt.header.timestamp,
                        ssrc: pkt.header.ssrc,
                    });
                }
                _ => continue,
            }
        }
    }

    /// Ask the remote sender for a keyframe (RTCP PLI).
    ///
    /// A receiver that joins mid-GOP, or loses the initial keyframe before
    /// DTLS is up, cannot decode until the sender emits another IDR with its
    /// SPS/PPS. Senders such as Android only do that when asked.
    ///
    /// `media_ssrc` is the SSRC of the RTP stream already seen on this track;
    /// it is preferred over the negotiated coding because some peers omit SSRC
    /// attributes from the SDP.
    pub async fn request_keyframe(&self, media_ssrc: Option<u32>) -> Result<()> {
        let media_ssrc = match media_ssrc {
            Some(ssrc) => ssrc,
            None => match self
                .inner
                .codings()
                .await
                .first()
                .and_then(|coding| coding.rtp_coding_parameters.ssrc)
            {
                Some(ssrc) => ssrc,
                None => return Ok(()),
            },
        };
        self.inner
            .write_rtcp(vec![Box::new(PictureLossIndication {
                sender_ssrc: 0,
                media_ssrc,
            })])
            .await?;
        Ok(())
    }
}

/// Map an RTP codec mime type onto the crate's codec vocabulary.
fn codec_from_mime(mime: &str) -> Option<Codec> {
    match mime.to_ascii_lowercase().as_str() {
        "audio/opus" => Some(Codec::Opus),
        "video/vp8" => Some(Codec::Vp8),
        "video/vp9" => Some(Codec::Vp9),
        "video/h264" => Some(Codec::H264),
        "video/h265" | "video/hevc" => Some(Codec::H265),
        "video/av1" => Some(Codec::Av1),
        _ => None,
    }
}

/// Which local media stream a track belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TrackId {
    Mic,
    System,
    Screen,
}

/// Events emitted by a peer connection, delivered to the callback passed to
/// [`WebRtcPeer::new`].
#[derive(Debug)]
pub enum PeerEvent {
    IceCandidate(IceCandidate),
    GatheringComplete,
    ConnectionState(ConnectionState),
    NegotiationNeeded,
    TrackReceived { track: RemoteTrack },
}

/// A STUN/TURN server for ICE gathering.
#[derive(Debug, Clone, Default)]
pub struct IceServer {
    pub urls: Vec<String>,
    pub username: String,
    pub credential: String,
}

/// Configuration for a peer connection.
#[derive(Debug, Clone, Default)]
pub struct PeerConfig {
    pub ice_servers: Vec<IceServer>,
}

/// One peer connection. Owns the WebRTC stack; local media is written through
/// [`TrackWriter`] handles returned by [`Self::add_track`].
pub struct WebRtcPeer {
    pc: Arc<dyn PeerConnection>,
}

/// A handle for writing encoded samples to a specific local track.
pub struct TrackWriter {
    track: Arc<TrackLocalStaticSample>,
    sender: Arc<dyn RtpSender>,
    ssrc: u32,
    /// The codec the samples are encoded with. The payload type must match it,
    /// so it is resolved by mime type rather than by list position.
    codec: Codec,
    payload_type: tokio::sync::OnceCell<Option<PayloadType>>,
}

#[derive(Clone)]
struct Handler {
    events: Arc<dyn Fn(PeerEvent) + Send + Sync>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_ice_candidate(&self, event: RTCPeerConnectionIceEvent) {
        if let Ok(init) = event.candidate.to_json() {
            (self.events)(PeerEvent::IceCandidate(IceCandidate {
                candidate: init.candidate,
                sdp_mid: init.sdp_mid,
                sdp_mline_index: init.sdp_mline_index,
                username_fragment: init.username_fragment,
            }));
        }
    }

    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            (self.events)(PeerEvent::GatheringComplete);
        }
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        (self.events)(PeerEvent::ConnectionState(map_state(state)));
    }

    async fn on_negotiation_needed(&self) {
        (self.events)(PeerEvent::NegotiationNeeded);
    }

    async fn on_track(&self, track: Arc<dyn TrackRemote>) {
        (self.events)(PeerEvent::TrackReceived {
            track: RemoteTrack::new(track),
        });
    }
}

/// Build the codec set and interceptor chain for a peer connection.
fn media_and_registry(video_codec: Codec) -> Result<(MediaEngine, Registry)> {
    let mut media_engine = MediaEngine::default();
    codec::register_default_codecs(&mut media_engine, video_codec)?;
    let registry = register_default_interceptors(Registry::new(), &mut media_engine)?;
    // The default chain never asks the remote for a keyframe. Senders that
    // only emit an IDR on request (Android) would then be undecodable when
    // their first keyframe is dropped before DTLS is up. A zero interval
    // sends one PLI as soon as a video stream is bound, without forcing
    // periodic keyframes; the media crate asks again if decoding stalls.
    let registry = registry.with(
        Slot::IntervalPli,
        IntervalPliInterceptor::new(Duration::ZERO),
    );
    Ok((media_engine, registry))
}

impl WebRtcPeer {
    /// Most recent STUN round-trip time for the selected ICE pair, in milliseconds.
    /// This is network RTT to this peer, not one-way audio or video latency.
    pub async fn round_trip_time_ms(&self) -> Option<f64> {
        let report = self.pc.get_stats(Instant::now(), StatsSelector::None).await;
        let rtt = report.iter().find_map(|entry| {
            let RTCStatsReportEntry::Transport(transport) = entry else {
                return None;
            };
            let RTCStatsReportEntry::IceCandidatePair(pair) =
                report.get(&transport.selected_candidate_pair_id)?
            else {
                return None;
            };
            let ms = pair.current_round_trip_time * 1000.0;
            (pair.responses_received > 0 && ms.is_finite() && ms >= 0.0).then_some(ms)
        });
        rtt
    }

    /// Build a peer connection with the given ICE servers and event callback.
    ///
    /// Only `video_codec` is registered as a video codec, so the negotiated
    /// codec always matches the one the local capture pipeline encodes.
    pub async fn new(
        config: PeerConfig,
        video_codec: Codec,
        events: Arc<dyn Fn(PeerEvent) + Send + Sync>,
    ) -> Result<Self> {
        let (media_engine, registry) = media_and_registry(video_codec)?;

        let ice_servers = config
            .ice_servers
            .into_iter()
            .map(|s| RTCIceServer {
                urls: s.urls,
                username: s.username,
                credential: s.credential,
            })
            .collect();

        let rtc_config = RTCConfigurationBuilder::new()
            .with_ice_servers(ice_servers)
            .build();

        let runtime = default_runtime().ok_or(WebRtcError::NoRuntime)?;

        let pc = PeerConnectionBuilder::new()
            .with_configuration(rtc_config)
            .with_media_engine(media_engine)
            .with_interceptor_registry(registry)
            .with_handler(Arc::new(Handler { events }) as Arc<dyn PeerConnectionEventHandler>)
            .with_runtime(runtime)
            .with_udp_addrs(vec!["0.0.0.0:0", "[::]:0"])
            .build()
            .await?;

        Ok(Self {
            pc: Arc::new(pc) as Arc<dyn PeerConnection>,
        })
    }

    /// Add a local track and return a writer for it. `codec` selects the
    /// encoding (Opus for audio, VP8/VP9/H264 for video).
    pub async fn add_track(&self, id: TrackId, codec: Codec) -> Result<TrackWriter> {
        let kind = if codec::is_audio(codec) {
            RtpCodecKind::Audio
        } else {
            RtpCodecKind::Video
        };
        let rtp_codec = codec::codec_to_rtp(codec);
        let ssrc = next_ssrc();

        let (stream_id, track_id, label) = match id {
            TrackId::Mic => ("sharescreen-mic-stream", "sharescreen-mic-track", "mic"),
            TrackId::System => (
                "sharescreen-system-stream",
                "sharescreen-system-track",
                "system",
            ),
            TrackId::Screen => (
                "sharescreen-screen-stream",
                "sharescreen-screen-track",
                "screen",
            ),
        };

        let track = Arc::new(TrackLocalStaticSample::new(
            Instant::now(),
            MediaStreamTrack::new(
                stream_id.to_owned(),
                track_id.to_owned(),
                label.to_owned(),
                kind,
                vec![RTCRtpEncodingParameters {
                    rtp_coding_parameters: RTCRtpCodingParameters {
                        ssrc: Some(ssrc),
                        ..Default::default()
                    },
                    codec: rtp_codec,
                    ..Default::default()
                }],
            ),
        )?);

        let sender = self
            .pc
            .add_track(track.clone() as Arc<dyn TrackLocal>)
            .await?;

        Ok(TrackWriter {
            track,
            sender,
            ssrc,
            codec,
            payload_type: tokio::sync::OnceCell::new(),
        })
    }

    pub async fn create_offer(&self) -> Result<SessionDescription> {
        to_session_description(self.pc.create_offer(None).await?)
    }

    pub async fn create_answer(&self) -> Result<SessionDescription> {
        to_session_description(self.pc.create_answer(None).await?)
    }

    pub async fn set_local_description(&self, desc: &SessionDescription) -> Result<()> {
        Ok(self
            .pc
            .set_local_description(from_session_description(desc)?)
            .await?)
    }

    pub async fn set_remote_description(&self, desc: &SessionDescription) -> Result<()> {
        Ok(self
            .pc
            .set_remote_description(from_session_description(desc)?)
            .await?)
    }

    pub async fn add_ice_candidate(&self, candidate: IceCandidate) -> Result<()> {
        Ok(self
            .pc
            .add_ice_candidate(RTCIceCandidateInit {
                candidate: candidate.candidate,
                sdp_mid: candidate.sdp_mid,
                sdp_mline_index: candidate.sdp_mline_index,
                username_fragment: candidate.username_fragment,
                url: None,
            })
            .await?)
    }

    pub async fn local_description(&self) -> Option<SessionDescription> {
        self.pc
            .local_description()
            .await
            .and_then(|d| to_session_description(d).ok())
    }

    pub async fn close(&self) -> Result<()> {
        Ok(self.pc.close().await?)
    }
}

/// Convert the WebRTC session description into the crate's own serde type.
fn to_session_description(desc: RTCSessionDescription) -> Result<SessionDescription> {
    let value = serde_json::to_value(&desc)?;
    Ok(SessionDescription {
        sdp_type: value
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("offer")
            .to_string(),
        sdp: value
            .get("sdp")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
    })
}

fn from_session_description(desc: &SessionDescription) -> Result<RTCSessionDescription> {
    let value = serde_json::json!({ "type": desc.sdp_type, "sdp": desc.sdp });
    Ok(serde_json::from_value(value)?)
}

impl TrackWriter {
    /// Write one encoded sample to the track. The payload type is resolved
    /// lazily after negotiation completes.
    pub async fn write(&self, sample: EncodedSample) -> Result<()> {
        let pt = *self
            .payload_type
            .get_or_init(|| async {
                let params = self.sender.get_parameters().await.ok()?;
                let mime = codec::codec_mime(self.codec);
                params
                    .rtp_parameters
                    .codecs
                    .iter()
                    // The negotiated codec list is ordered by the remote
                    // description, so match on the mime type instead of
                    // taking the first entry.
                    .find(|c| c.rtp_codec.mime_type.eq_ignore_ascii_case(mime))
                    .map(|c| c.payload_type)
            })
            .await;

        let Some(pt) = pt else {
            return Err(WebRtcError::NoMatchingCodec(self.codec));
        };

        let data = Bytes::from(sample.data);
        let rtc_sample = Sample {
            data,
            duration: Duration::from_micros(sample.duration_micros.max(1)),
            ..Sample::new(Instant::now())
        };

        self.track
            .sample_writer(self.ssrc, pt)
            .write_sample(&rtc_sample)
            .await?;
        Ok(())
    }
}

fn map_state(state: RTCPeerConnectionState) -> ConnectionState {
    use RTCPeerConnectionState as S;
    match state {
        S::Unspecified | S::New => ConnectionState::New,
        S::Connecting => ConnectionState::Connecting,
        S::Connected => ConnectionState::Connected,
        S::Disconnected => ConnectionState::Disconnected,
        S::Failed => ConnectionState::Failed,
        S::Closed => ConnectionState::Closed,
        _ => ConnectionState::Disconnected,
    }
}

static NEXT_SSRC: OnceLock<AtomicU32> = OnceLock::new();

/// Generate a process-unique SSRC. Seeded from the clock to avoid collisions
/// between peers on the same host; a cryptographic-random SSRC would be
/// marginally better but is not required for correctness.
fn next_ssrc() -> u32 {
    let counter = NEXT_SSRC.get_or_init(|| {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0x5eed_0000);
        AtomicU32::new(seed)
    });
    counter.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interceptor_chain_requests_a_keyframe_on_bind() {
        let (_, registry) = media_and_registry(Codec::H264).unwrap();
        let slots = registry.slots();
        assert!(
            slots
                .iter()
                .any(|(slot, name)| *slot == Slot::IntervalPli && name == "IntervalPliInterceptor"),
            "PLI interceptor missing from {slots:?}"
        );
    }
}

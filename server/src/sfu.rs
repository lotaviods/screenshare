//! Subscription policy and bounded fan-out for the relay SFU.
//!
//! The network transport is the same `WebRtcPeer` wrapper the desktop client
//! uses (D18): the server owns one answerer peer per participant and forwards
//! already packetized RTP between them.

use sharescreen_core::Codec;
use sharescreen_webrtc::{RemoteTrack, RtpPacket, TrackWriter};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;
use tokio::sync::mpsc;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SubscriptionError {
    #[error("track id is already published")]
    DuplicateTrack,
    #[error("screen publisher is not active in this room")]
    UnknownScreenPublisher,
    #[error("participant is not in this room")]
    UnknownViewer,
    #[error("video codec mismatch: room uses {expected:?}, client offered {actual:?}")]
    CodecMismatch { expected: Codec, actual: Codec },
}

/// Room-scoped publication roster and per-viewer focus selections.
#[derive(Debug, Default)]
pub struct SfuRoom {
    publications: BTreeMap<String, PublishedTrack>,
    focus_by_viewer: BTreeMap<String, String>,
    participants: BTreeSet<String>,
    /// The RTP payload is forwarded without transcoding, so every participant
    /// must negotiate the same video codec. The first SFU offer fixes it.
    video_codec: Option<Codec>,
}

impl SfuRoom {
    pub fn join(&mut self, participant: impl Into<String>) {
        self.participants.insert(participant.into());
    }

    /// Pin the room to the first offered video codec. A different codec would
    /// make forwarded payloads undecodable for existing viewers.
    pub fn negotiate_video_codec(&mut self, codec: Codec) -> Result<(), SubscriptionError> {
        match self.video_codec {
            Some(expected) if expected != codec => Err(SubscriptionError::CodecMismatch {
                expected,
                actual: codec,
            }),
            Some(_) => Ok(()),
            None => {
                self.video_codec = Some(codec);
                Ok(())
            }
        }
    }

    pub fn video_codec(&self) -> Option<Codec> {
        self.video_codec
    }

    pub fn publish(&mut self, track: PublishedTrack) -> Result<(), SubscriptionError> {
        if !self.participants.contains(&track.publisher) {
            return Err(SubscriptionError::UnknownViewer);
        }
        let layer_valid = match track.kind {
            TrackKind::Audio => track.layer.is_none(),
            TrackKind::Screen => track.layer.is_some(),
        };
        if !layer_valid {
            return Err(SubscriptionError::UnknownScreenPublisher);
        }
        if self.publications.contains_key(&track.id) {
            return Err(SubscriptionError::DuplicateTrack);
        }
        self.publications.insert(track.id.clone(), track);
        Ok(())
    }

    pub fn unpublish(&mut self, track_id: &str) -> bool {
        let Some(track) = self.publications.remove(track_id) else {
            return false;
        };
        if track.kind == TrackKind::Screen
            && track.layer == Some(VideoLayer::Full)
            && !self.publications.values().any(|candidate| {
                candidate.publisher == track.publisher
                    && candidate.kind == TrackKind::Screen
                    && candidate.layer == Some(VideoLayer::Full)
            })
        {
            self.focus_by_viewer
                .retain(|_, publisher| publisher != &track.publisher);
        }
        true
    }

    pub fn track_publisher(&self, track_id: &str) -> Option<&str> {
        self.publications
            .get(track_id)
            .map(|track| track.publisher.as_str())
    }

    pub fn publication(&self, track_id: &str) -> Option<&PublishedTrack> {
        self.publications.get(track_id)
    }

    pub fn publications(&self) -> impl Iterator<Item = &PublishedTrack> {
        self.publications.values()
    }

    pub fn has_publications(&self) -> bool {
        !self.publications.is_empty()
    }

    pub fn set_focus(
        &mut self,
        viewer: &str,
        publisher: Option<&str>,
    ) -> Result<(), SubscriptionError> {
        if !self.participants.contains(viewer) {
            return Err(SubscriptionError::UnknownViewer);
        }
        match publisher {
            Some(publisher)
                if publisher != viewer
                    && self.publications.values().any(|track| {
                        track.publisher == publisher
                            && track.kind == TrackKind::Screen
                            && track.layer == Some(VideoLayer::Full)
                    }) =>
            {
                self.focus_by_viewer
                    .insert(viewer.to_owned(), publisher.to_owned());
            }
            Some(_) => return Err(SubscriptionError::UnknownScreenPublisher),
            None => {
                self.focus_by_viewer.remove(viewer);
            }
        }
        Ok(())
    }

    pub fn subscriptions_for(&self, viewer: &str) -> SubscriptionSet {
        subscriptions_for(
            viewer,
            self.focus_by_viewer.get(viewer).map(String::as_str),
            &self.publications.values().cloned().collect::<Vec<_>>(),
        )
    }

    /// Remove a participant and all state that can reference its streams.
    pub fn leave(&mut self, participant: &str) {
        self.participants.remove(participant);
        self.publications
            .retain(|_, track| track.publisher != participant);
        self.focus_by_viewer
            .retain(|viewer, publisher| viewer != participant && publisher != participant);
    }
}

/// Bounded, codec-agnostic RTP fan-out queues.
///
/// Each subscriber owns a receiver task that writes packets to its negotiated
/// WebRTC track. Audio forwarding applies backpressure and never drops a
/// packet. Video forwarding uses `try_send` so a slow subscriber cannot stall
/// other tracks; its overflow is counted and the stale packet is dropped.
pub struct RtpForwarder<T> {
    tracks: RwLock<HashMap<String, FanoutTrack<T>>>,
    epochs: RwLock<HashMap<String, u64>>,
    next_epoch: AtomicU64,
    audio_packets_queued: AtomicU64,
    video_packets_dropped: AtomicU64,
}

struct FanoutTrack<T> {
    kind: TrackKind,
    subscribers: HashMap<String, mpsc::Sender<T>>,
    epoch: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RelayMetrics {
    pub audio_packets_queued: u64,
    pub video_packets_dropped: u64,
}

impl<T: Clone + Send + 'static> RtpForwarder<T> {
    pub fn new() -> Self {
        Self {
            tracks: RwLock::new(HashMap::new()),
            epochs: RwLock::new(HashMap::new()),
            next_epoch: AtomicU64::new(1),
            audio_packets_queued: AtomicU64::new(0),
            video_packets_dropped: AtomicU64::new(0),
        }
    }

    fn ensure_epoch(&self, track_id: &str) -> u64 {
        let mut epochs = self.epochs.write().unwrap();
        *epochs
            .entry(track_id.to_owned())
            .or_insert_with(|| self.next_epoch.fetch_add(1, Ordering::Relaxed))
    }

    /// Register a bounded writer queue for one viewer and source track.
    pub fn subscribe(
        &self,
        track_id: impl Into<String>,
        kind: TrackKind,
        viewer: impl Into<String>,
        sender: mpsc::Sender<T>,
    ) {
        let track_id = track_id.into();
        let epoch = self.ensure_epoch(&track_id);
        let mut tracks = self.tracks.write().unwrap();
        let track = tracks.entry(track_id).or_insert_with(|| FanoutTrack {
            kind,
            subscribers: HashMap::new(),
            epoch,
        });
        track.kind = kind;
        track.subscribers.insert(viewer.into(), sender);
    }

    pub fn unsubscribe(&self, track_id: &str, viewer: &str) {
        let mut tracks = self.tracks.write().unwrap();
        if let Some(track) = tracks.get_mut(track_id) {
            track.subscribers.remove(viewer);
            if track.subscribers.is_empty() {
                tracks.remove(track_id);
            }
        }
    }

    pub fn remove_viewer(&self, viewer: &str) {
        self.tracks.write().unwrap().retain(|_, track| {
            track.subscribers.remove(viewer);
            !track.subscribers.is_empty()
        });
    }

    /// Return `true` when the viewer already has a live subscription on this
    /// track. Used during renegotiation to skip tracks that were negotiated in
    /// a previous offer/answer round.
    pub fn has_subscriber(&self, track_id: &str, viewer: &str) -> bool {
        self.tracks
            .read()
            .unwrap()
            .get(track_id)
            .map(|t| t.subscribers.contains_key(viewer))
            .unwrap_or(false)
    }

    /// Remove a publication and invalidate packets from its previous source.
    pub fn remove_track(&self, track_id: &str) {
        self.tracks.write().unwrap().remove(track_id);
        let next = self.next_epoch.fetch_add(1, Ordering::Relaxed);
        self.epochs
            .write()
            .unwrap()
            .insert(track_id.to_owned(), next);
    }

    /// Fan one already packetized RTP payload to the current subscribers.
    pub async fn forward(&self, track_id: &str, packet: T) {
        let epoch = self.epochs.read().unwrap().get(track_id).copied();
        let Some(epoch) = epoch else {
            return;
        };
        self.forward_at(track_id, epoch, packet).await;
    }

    async fn forward_at(&self, track_id: &str, epoch: u64, packet: T) {
        let Some((kind, subscribers)) =
            self.tracks.read().unwrap().get(track_id).and_then(|track| {
                if track.epoch != epoch {
                    return None;
                }
                Some((
                    track.kind,
                    track.subscribers.values().cloned().collect::<Vec<_>>(),
                ))
            })
        else {
            return;
        };

        match kind {
            TrackKind::Audio => {
                let writes = subscribers.into_iter().map(|sender| {
                    let packet = packet.clone();
                    async move { sender.send(packet).await }
                });
                let results = futures_util::future::join_all(writes).await;
                let queued = results.iter().filter(|result| result.is_ok()).count() as u64;
                self.audio_packets_queued
                    .fetch_add(queued, Ordering::Relaxed);
            }
            TrackKind::Screen => {
                let mut dropped = 0u64;
                for sender in subscribers {
                    if sender.try_send(packet.clone()).is_err() {
                        dropped += 1;
                    }
                }
                self.video_packets_dropped
                    .fetch_add(dropped, Ordering::Relaxed);
            }
        }
    }

    pub fn metrics(&self) -> RelayMetrics {
        RelayMetrics {
            audio_packets_queued: self.audio_packets_queued.load(Ordering::Relaxed),
            video_packets_dropped: self.video_packets_dropped.load(Ordering::Relaxed),
        }
    }
}

impl<T: Clone + Send + 'static> Default for RtpForwarder<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl RtpForwarder<RtpPacket> {
    /// Register a negotiated egress track and spawn its bounded RTP writer.
    pub fn subscribe_writer(
        self: &std::sync::Arc<Self>,
        track_id: impl Into<String>,
        kind: TrackKind,
        viewer: impl Into<String>,
        writer: std::sync::Arc<TrackWriter>,
        queue_capacity: usize,
    ) {
        let (sender, mut receiver) = mpsc::channel(queue_capacity.max(1));
        self.subscribe(track_id, kind, viewer, sender);
        tokio::spawn(async move {
            while let Some(packet) = receiver.recv().await {
                if let Err(error) = writer.write_rtp(packet).await {
                    tracing::warn!(%error, "SFU egress RTP write failed");
                }
            }
        });
    }

    /// Read a received WebRTC track and feed each already packetized RTP
    /// payload to its current viewer queues.
    pub fn attach_source(
        self: &std::sync::Arc<Self>,
        track_id: impl Into<String>,
        track: RemoteTrack,
    ) -> tokio::task::JoinHandle<()> {
        let track_id = track_id.into();
        let epoch = self.ensure_epoch(&track_id);
        let forwarder = self.clone();
        tokio::spawn(async move {
            while let Some(packet) = track.next_packet().await {
                forwarder.forward_at(&track_id, epoch, packet).await;
            }
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackKind {
    Audio,
    Screen,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoLayer {
    Full,
    Thumbnail,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedTrack {
    pub id: String,
    pub publisher: String,
    pub kind: TrackKind,
    /// Audio has no layer. Each screen publication has a full and thumbnail
    /// track, as selected by D16.
    pub layer: Option<VideoLayer>,
}

/// Track identifiers a viewer should receive for the current roster/focus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubscriptionSet {
    pub focus: Option<String>,
    pub tracks: BTreeSet<String>,
}

/// Select every other participant's audio, the focused screen's full track,
/// and thumbnails for every other screen publisher. Until a publisher offers
/// a thumbnail layer, keep one full track selected when focus is cleared so a
/// single-share call does not go blank in grid mode.
pub fn subscriptions_for(
    viewer: &str,
    focus: Option<&str>,
    publications: &[PublishedTrack],
) -> SubscriptionSet {
    let mut tracks = BTreeSet::new();
    let fallback_full = focus
        .is_none()
        .then(|| {
            publications
                .iter()
                .filter(|track| {
                    track.publisher != viewer
                        && track.kind == TrackKind::Screen
                        && track.layer == Some(VideoLayer::Full)
                        && !publications.iter().any(|candidate| {
                            candidate.publisher == track.publisher
                                && candidate.kind == TrackKind::Screen
                                && candidate.layer == Some(VideoLayer::Thumbnail)
                        })
                })
                .map(|track| track.publisher.as_str())
                .min()
        })
        .flatten();
    for track in publications {
        if track.publisher == viewer {
            continue;
        }
        match track.kind {
            TrackKind::Audio => {
                tracks.insert(track.id.clone());
            }
            TrackKind::Screen => {
                let wanted = if focus == Some(track.publisher.as_str())
                    || fallback_full == Some(track.publisher.as_str())
                {
                    VideoLayer::Full
                } else {
                    VideoLayer::Thumbnail
                };
                if track.layer == Some(wanted) {
                    tracks.insert(track.id.clone());
                }
            }
        }
    }
    SubscriptionSet {
        focus: focus.map(str::to_owned),
        tracks,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn audio(id: &str, publisher: &str) -> PublishedTrack {
        PublishedTrack {
            id: id.into(),
            publisher: publisher.into(),
            kind: TrackKind::Audio,
            layer: None,
        }
    }

    fn screen(id: &str, publisher: &str, layer: VideoLayer) -> PublishedTrack {
        PublishedTrack {
            id: id.into(),
            publisher: publisher.into(),
            kind: TrackKind::Screen,
            layer: Some(layer),
        }
    }

    #[test]
    fn focus_gets_full_track_and_all_other_screens_get_thumbnails() {
        let publications = vec![
            audio("a-audio", "a"),
            audio("b-audio", "b"),
            audio("c-audio", "c"),
            screen("b-full", "b", VideoLayer::Full),
            screen("b-thumb", "b", VideoLayer::Thumbnail),
            screen("c-full", "c", VideoLayer::Full),
            screen("c-thumb", "c", VideoLayer::Thumbnail),
        ];

        let selected = subscriptions_for("a", Some("b"), &publications);
        assert_eq!(selected.focus.as_deref(), Some("b"));
        assert_eq!(
            selected.tracks,
            ["b-audio", "b-full", "c-audio", "c-thumb"]
                .into_iter()
                .map(str::to_owned)
                .collect()
        );
    }

    #[test]
    fn no_focus_keeps_audio_and_only_thumbnails() {
        let publications = vec![
            audio("b-audio", "b"),
            screen("b-full", "b", VideoLayer::Full),
            screen("b-thumb", "b", VideoLayer::Thumbnail),
        ];
        let selected = subscriptions_for("a", None, &publications);
        assert_eq!(
            selected.tracks,
            ["b-audio", "b-thumb"]
                .into_iter()
                .map(str::to_owned)
                .collect()
        );
    }

    #[test]
    fn viewer_never_subscribes_to_its_own_publications() {
        let publications = vec![
            audio("a-audio", "a"),
            screen("a-full", "a", VideoLayer::Full),
            screen("a-thumb", "a", VideoLayer::Thumbnail),
        ];
        assert!(subscriptions_for("a", Some("a"), &publications)
            .tracks
            .is_empty());
    }

    #[test]
    fn room_focus_updates_subscriptions_and_leave_cleans_tracks() {
        let mut room = SfuRoom::default();
        room.join("viewer");
        room.join("publisher");
        room.publish(audio("publisher-audio", "publisher")).unwrap();
        room.publish(screen("full", "publisher", VideoLayer::Full))
            .unwrap();
        room.publish(screen("thumb", "publisher", VideoLayer::Thumbnail))
            .unwrap();

        assert_eq!(room.set_focus("viewer", Some("publisher")), Ok(()));
        assert_eq!(
            room.subscriptions_for("viewer").tracks,
            ["publisher-audio", "full"]
                .into_iter()
                .map(str::to_owned)
                .collect()
        );

        room.leave("publisher");
        assert!(room.publications.is_empty());
        assert!(room.subscriptions_for("viewer").tracks.is_empty());
    }

    #[test]
    fn room_rejects_focus_without_a_screen_publication() {
        let mut room = SfuRoom::default();
        room.join("viewer");
        room.join("audio-only");
        room.publish(audio("track", "audio-only")).unwrap();
        assert_eq!(
            room.set_focus("viewer", Some("audio-only")),
            Err(SubscriptionError::UnknownScreenPublisher)
        );
    }

    #[test]
    fn room_pins_the_first_video_codec_and_rejects_others() {
        let mut room = SfuRoom::default();
        assert_eq!(room.negotiate_video_codec(Codec::H264), Ok(()));
        assert_eq!(room.negotiate_video_codec(Codec::H264), Ok(()));
        assert_eq!(
            room.negotiate_video_codec(Codec::Vp8),
            Err(SubscriptionError::CodecMismatch {
                expected: Codec::H264,
                actual: Codec::Vp8,
            })
        );
        assert_eq!(room.video_codec(), Some(Codec::H264));
    }

    #[tokio::test]
    async fn audio_fanout_waits_for_capacity_without_dropping() {
        let forwarder = std::sync::Arc::new(RtpForwarder::new());
        let (tx, mut rx) = mpsc::channel(1);
        forwarder.subscribe("audio", TrackKind::Audio, "viewer", tx);
        forwarder.forward("audio", 1_u8).await;

        let task_forwarder = forwarder.clone();
        let mut second = tokio::spawn(async move { task_forwarder.forward("audio", 2_u8).await });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), &mut second)
                .await
                .is_err()
        );
        assert_eq!(rx.recv().await, Some(1));
        tokio::time::timeout(std::time::Duration::from_secs(1), second)
            .await
            .expect("audio forwarding should resume when capacity opens")
            .unwrap();
        assert_eq!(rx.recv().await, Some(2));
        assert_eq!(forwarder.metrics().audio_packets_queued, 2);
    }

    #[tokio::test]
    async fn video_fanout_drops_on_full_queue_and_counts_it() {
        let forwarder = RtpForwarder::new();
        let (tx, mut rx) = mpsc::channel(1);
        forwarder.subscribe("screen", TrackKind::Screen, "viewer", tx);
        forwarder.forward("screen", 1_u8).await;
        forwarder.forward("screen", 2_u8).await;
        assert_eq!(rx.recv().await, Some(1));
        assert_eq!(rx.try_recv(), Err(mpsc::error::TryRecvError::Empty));
        assert_eq!(forwarder.metrics().video_packets_dropped, 1);
    }
}

//! Call session: signaling + WebRTC mesh + local media routing.

mod hub;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use sharescreen_core::{
    CallEvent, Codec, ConnectionMode, ConnectionState, EncodedSample, Participant,
    PublishedTrackKind,
};
use sharescreen_media::LocalPreview;
use sharescreen_media::RemoteMedia;
use sharescreen_signaling::{
    Incoming, PeerInfo, PeerPayload, SignalMessage, SignalingClient, SignalingSender,
};
use sharescreen_webrtc::{
    IceCandidate, PeerConfig, PeerEvent, SessionDescription, TrackId, WebRtcPeer,
};
use tokio::sync::mpsc;

use crate::hub::MediaHub;

const SFU_MAX_REMOTE_PARTICIPANTS: usize = 7;
const SFU_AUDIO_TRACKS_PER_PARTICIPANT: usize = 2;
const MAX_MESH_PARTICIPANTS: usize = 3;

#[derive(Debug, thiserror::Error)]
pub enum CallError {
    #[error("signaling: {0}")]
    Signaling(#[from] sharescreen_signaling::SignalingError),
    #[error("webrtc: {0}")]
    Webrtc(#[from] sharescreen_webrtc::WebRtcError),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone)]
pub struct CallConfig {
    pub self_id: String,
    pub self_name: String,
    /// Full WebSocket URL of the signaling server.
    pub signaling_url: String,
    /// ICE servers used for every peer connection.
    pub peer_config: PeerConfig,
    /// Video codec used for the screen-share track.
    pub video_codec: Codec,
    pub connection_mode: ConnectionMode,
}

/// A live call: one peer connection per remote participant, plus the local
/// media channels that feed them.
pub struct CallSession {
    shared: Shared,
    mic_tx: mpsc::Sender<EncodedSample>,
    system_tx: mpsc::Sender<EncodedSample>,
    screen_tx: mpsc::Sender<EncodedSample>,
    thumbnail_tx: mpsc::Sender<EncodedSample>,
}

#[derive(Clone)]
struct Shared {
    /// Mutable so the server's TURN credentials can be folded in after the
    /// connection is established.
    cfg: Arc<RwLock<CallConfig>>,
    sender: SignalingSender,
    peers: Arc<RwLock<HashMap<String, Arc<WebRtcPeer>>>>,
    sfu_peer: Arc<RwLock<Option<Arc<WebRtcPeer>>>>,
    sfu_subscriptions: Arc<RwLock<Option<HashSet<String>>>>,
    sfu_negotiated: Arc<AtomicBool>,
    sfu_negotiating: Arc<AtomicBool>,
    sfu_subscription_dirty: Arc<AtomicBool>,
    sfu_published: Arc<AtomicBool>,
    sfu_connection_epoch: Arc<std::sync::atomic::AtomicU64>,
    sfu_reconnect: mpsc::UnboundedSender<u64>,
    /// Every remote participant in the room, with their last known media and
    /// connection state.
    roster: Arc<RwLock<HashMap<String, Participant>>>,
    /// What each remote participant is publishing, keyed by peer id. Kept
    /// apart from the roster because the state can arrive before the roster
    /// names the peer.
    remote_media: Arc<RwLock<HashMap<String, RemoteMediaState>>>,
    /// ICE candidates that arrived before the peer existed, or before its
    /// remote description was set. Flushed once the connection can accept them.
    pending_ice: Arc<RwLock<HashMap<String, Vec<IceCandidate>>>>,
    /// What this peer is publishing, mirrored to the other participants.
    local: Arc<RwLock<LocalMedia>>,
    mic: Arc<MediaHub>,
    system: Arc<MediaHub>,
    screen: Arc<MediaHub>,
    thumbnail: Arc<MediaHub>,
    events: mpsc::UnboundedSender<CallEvent>,
    room_id: Arc<RwLock<Option<String>>>,
    /// Decoder pipelines for the remote participants' tracks.
    remote: Arc<RemoteMedia>,
    local_preview: Arc<LocalPreview>,
    closing: Arc<AtomicBool>,
}

/// What this peer is publishing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct LocalMedia {
    mic_on: bool,
    screen_on: bool,
}

impl CallSession {
    /// Connect to the signaling server and start the dispatch loop.
    pub async fn start(
        cfg: CallConfig,
        events: mpsc::UnboundedSender<CallEvent>,
    ) -> Result<Self, CallError> {
        let client = SignalingClient::connect(&cfg.signaling_url).await?;
        let sender = client.sender();
        let incoming = client.incoming;

        let (mic_tx, mic_rx) = mpsc::channel(64);
        let (system_tx, system_rx) = mpsc::channel(64);
        let (screen_tx, screen_rx) = mpsc::channel(64);
        let (thumbnail_tx, thumbnail_rx) = mpsc::channel(16);
        let (sfu_reconnect_tx, sfu_reconnect_rx) = mpsc::unbounded_channel();

        let mic = Arc::new(MediaHub::new());
        let system = Arc::new(MediaHub::new());
        let screen = Arc::new(MediaHub::new());
        let thumbnail = Arc::new(MediaHub::new());
        let local_preview = Arc::new(LocalPreview::new());

        let shared = Shared {
            cfg: Arc::new(RwLock::new(cfg.clone())),
            sender,
            peers: Arc::new(RwLock::new(HashMap::new())),
            sfu_peer: Arc::new(RwLock::new(None)),
            sfu_subscriptions: Arc::new(RwLock::new(None)),
            sfu_negotiated: Arc::new(AtomicBool::new(false)),
            sfu_negotiating: Arc::new(AtomicBool::new(false)),
            sfu_subscription_dirty: Arc::new(AtomicBool::new(false)),
            sfu_published: Arc::new(AtomicBool::new(false)),
            sfu_connection_epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            sfu_reconnect: sfu_reconnect_tx,
            roster: Arc::new(RwLock::new(HashMap::new())),
            remote_media: Arc::new(RwLock::new(HashMap::new())),
            pending_ice: Arc::new(RwLock::new(HashMap::new())),
            local: Arc::new(RwLock::new(LocalMedia::default())),
            mic: mic.clone(),
            system: system.clone(),
            screen: screen.clone(),
            thumbnail: thumbnail.clone(),
            events,
            room_id: Arc::new(RwLock::new(None)),
            remote: Arc::new(RemoteMedia::new()),
            local_preview: local_preview.clone(),
            closing: Arc::new(AtomicBool::new(false)),
        };

        // One fan-out task per local media stream. The screen stream also feeds
        // the self-view loopback, so it is tapped before it reaches the peers.
        tokio::spawn(hub_loop(mic, mic_rx, None));
        tokio::spawn(hub_loop(system, system_rx, None));
        tokio::spawn(hub_loop(screen, screen_rx, Some(local_preview)));
        tokio::spawn(hub_loop(thumbnail, thumbnail_rx, None));

        // Signaling dispatch.
        tokio::spawn(dispatch(shared.clone(), incoming, sfu_reconnect_rx));

        // Ask the server for time-limited TURN credentials; they are folded
        // into the ICE configuration before any peer connection is made.
        let _ = shared.sender.send(SignalMessage::GetTurnCredentials);

        Ok(Self {
            shared,
            mic_tx,
            system_tx,
            screen_tx,
            thumbnail_tx,
        })
    }

    /// Ask the server to create a room.
    pub fn create_room(&self) -> Result<(), CallError> {
        self.shared.sender.send(SignalMessage::CreateRoom)?;
        Ok(())
    }

    /// Ask the server to join an existing room.
    pub fn join_room(&self, room_id: String) -> Result<(), CallError> {
        self.shared
            .sender
            .send(SignalMessage::JoinRoom { room_id })?;
        Ok(())
    }

    /// Leave the current room, tearing down the peer connections with it.
    pub async fn leave_room(&self) -> Result<(), CallError> {
        self.shared.sender.send(SignalMessage::LeaveRoom)?;
        close_all_peers(&self.shared).await;
        close_sfu_peer(&self.shared).await;
        Ok(())
    }

    /// Close the current room for everyone in it. Any member may do this.
    pub async fn delete_room(&self) -> Result<(), CallError> {
        self.shared.sender.send(SignalMessage::DeleteRoom)?;
        close_all_peers(&self.shared).await;
        close_sfu_peer(&self.shared).await;
        Ok(())
    }

    /// Tell the other participants that this peer started or stopped sharing
    /// its screen.
    pub fn set_screen_sharing(&self, sharing: bool) {
        {
            let mut local = self.shared.local.write().unwrap();
            if local.screen_on == sharing {
                return;
            }
            local.screen_on = sharing;
        }
        if self.shared.cfg.read().unwrap().connection_mode == ConnectionMode::Sfu {
            // The SFU publication is registered with the initial SDP offer.
            // Keep it stable across share start/stop; the media hub has no
            // samples while capture is stopped, and duplicate publication IDs
            // are rejected by the signaling server.
            broadcast_local_state(&self.shared);
            return;
        }
        broadcast_local_state(&self.shared);
    }

    /// Tell the other participants whether this peer's microphone is live.
    pub fn set_mic_on(&self, on: bool) {
        {
            let mut local = self.shared.local.write().unwrap();
            if local.mic_on == on {
                return;
            }
            local.mic_on = on;
        }
        broadcast_local_state(&self.shared);
    }

    /// Choose the full-quality screen publisher in SFU mode. `None` clears the
    /// focus and leaves only secondary subscriptions selected by the server.
    pub fn set_focus(&self, publisher: Option<String>) -> Result<(), CallError> {
        if self.shared.cfg.read().unwrap().connection_mode == ConnectionMode::Sfu {
            self.shared
                .sender
                .send(SignalMessage::SetFocus { publisher })?;
        }
        Ok(())
    }

    /// Channel to feed microphone samples into.
    pub fn mic_sender(&self) -> mpsc::Sender<EncodedSample> {
        self.mic_tx.clone()
    }

    /// Channel to feed system-audio samples into.
    pub fn system_sender(&self) -> mpsc::Sender<EncodedSample> {
        self.system_tx.clone()
    }

    /// Channel to feed screen-share samples into.
    pub fn screen_sender(&self) -> mpsc::Sender<EncodedSample> {
        self.screen_tx.clone()
    }

    /// Channel for the best-effort low-resolution screen-share layer.
    pub fn thumbnail_sender(&self) -> mpsc::Sender<EncodedSample> {
        self.thumbnail_tx.clone()
    }

    pub fn room_id(&self) -> Option<String> {
        self.shared.room_id.read().unwrap().clone()
    }

    /// The video codec the screen track was negotiated with. Capture must
    /// encode with this codec: the track's payload type is fixed at
    /// negotiation, so a different codec would be undecodable.
    pub fn video_codec(&self) -> Codec {
        self.shared.cfg.read().unwrap().video_codec
    }

    pub fn peer_count(&self) -> usize {
        self.shared.peers.read().unwrap().len()
            + usize::from(self.shared.sfu_peer.read().unwrap().is_some())
    }

    /// Recent selected ICE-pair RTT by peer; absent measurements are omitted.
    pub fn peer_round_trip_times(
        &self,
    ) -> impl std::future::Future<Output = Vec<(String, f64)>> + Send + 'static {
        let peers: Vec<_> = self
            .shared
            .peers
            .read()
            .unwrap()
            .iter()
            .map(|(id, peer)| (id.clone(), peer.clone()))
            .collect();
        async move {
            let mut result = Vec::new();
            for (id, peer) in peers {
                if let Some(ms) = peer.round_trip_time_ms().await {
                    result.push((id, ms));
                }
            }
            result
        }
    }

    /// The latest decoded video frame (JPEG) from a peer, if any.
    pub fn remote_frame(&self, peer_id: &str) -> Option<Vec<u8>> {
        self.shared
            .remote
            .latest_frame(&format!("{peer_id}-screen"))
            .or_else(|| self.shared.remote.latest_frame(peer_id))
    }

    /// The latest low-resolution SFU thumbnail for a participant.
    pub fn remote_thumbnail_frame(&self, peer_id: &str) -> Option<Vec<u8>> {
        self.shared
            .remote
            .latest_frame(&format!("{peer_id}-screen-thumb"))
    }

    /// The latest frame of our own screen share as a peer decodes it, if any.
    ///
    /// This is the outgoing encoded stream played back, not a second capture of
    /// the screen, so the self-view matches what the other participants see.
    /// Calling it also marks the self-view as watched; the loopback stops
    /// decoding when nobody polls.
    pub fn local_preview(&self) -> Option<Vec<u8>> {
        self.shared.local_preview.latest_frame()
    }

    /// The latest decoded frame of whichever participant is sharing, if any.
    pub fn shared_screen_frame(&self) -> Option<Vec<u8>> {
        let sharer = self
            .shared
            .roster
            .read()
            .unwrap()
            .values()
            .find(|p| p.screen_on)
            .map(|p| p.id.clone())?;
        self.shared
            .remote
            .latest_frame(&format!("{sharer}-screen"))
            .or_else(|| self.shared.remote.latest_frame(&sharer))
    }

    /// Set the playback volume for a peer's audio.
    pub fn set_peer_volume(&self, peer_id: &str, volume: f64) {
        self.shared.remote.set_peer_volume(peer_id, volume);
    }

    /// Close every peer connection and stop the decoders. Called when the
    /// user disconnects from the signaling server.
    pub async fn disconnect(&self) {
        self.shared.closing.store(true, Ordering::Release);
        if self.room_id().is_some() {
            let _ = self.shared.sender.send(SignalMessage::LeaveRoom);
        }
        self.shared.sender.close();
        close_all_peers(&self.shared).await;
        close_sfu_peer(&self.shared).await;
    }
}

async fn hub_loop(
    hub: Arc<MediaHub>,
    mut rx: mpsc::Receiver<EncodedSample>,
    preview: Option<Arc<LocalPreview>>,
) {
    while let Some(sample) = rx.recv().await {
        if let Some(preview) = &preview {
            preview.push(&sample);
        }
        hub.broadcast(&sample).await;
    }
}

async fn dispatch(
    shared: Shared,
    mut incoming: mpsc::UnboundedReceiver<Incoming>,
    mut sfu_reconnects: mpsc::UnboundedReceiver<u64>,
) {
    loop {
        tokio::select! {
            item = incoming.recv() => {
                let Some(item) = item else { break };
                if shared.closing.load(Ordering::Acquire) {
                    break;
                }
                match item {
                    Incoming::Message(msg) => handle_message(&shared, msg).await,
                    Incoming::Disconnected { reason } => {
                        tracing::warn!(%reason, "signaling disconnected; stopping call dispatch");
                        let _ = shared.events.send(CallEvent::Error {
                            message: format!("signaling connection lost: {reason}"),
                        });
                        break;
                    }
                }
            }
            Some(epoch) = sfu_reconnects.recv() => {
                if shared.sfu_connection_epoch.load(Ordering::Acquire) != epoch
                    || shared.closing.load(Ordering::Acquire)
                    || !shared.sfu_published.load(Ordering::Acquire)
                {
                    continue;
                }
                if let Err(error) = shared.sender.send(SignalMessage::SfuRestart) {
                    let _ = shared.events.send(CallEvent::Error {
                        message: format!("Could not restart SFU signaling: {error}"),
                    });
                    continue;
                }
                close_sfu_transport(&shared).await;
                if let Err(error) = offer_sfu(&shared).await {
                    let _ = shared.events.send(CallEvent::Error {
                        message: format!("Could not reconnect to SFU: {error}"),
                    });
                }
            }
        }
    }
}

async fn handle_message(shared: &Shared, msg: SignalMessage) {
    match msg {
        SignalMessage::RoomCreated { room_id } => {
            *shared.room_id.write().unwrap() = Some(room_id.clone());
            if shared.cfg.read().unwrap().connection_mode == ConnectionMode::Sfu {
                if let Err(error) = offer_sfu(shared).await {
                    let _ = shared.events.send(CallEvent::Error {
                        message: error.to_string(),
                    });
                }
            }
            let _ = shared.events.send(CallEvent::RoomCreated { room_id });
        }
        SignalMessage::Joined { room_id, peers } => {
            *shared.room_id.write().unwrap() = Some(room_id.clone());
            if shared.cfg.read().unwrap().connection_mode == ConnectionMode::Sfu {
                if let Err(error) = offer_sfu(shared).await {
                    let _ = shared.events.send(CallEvent::Error {
                        message: error.to_string(),
                    });
                }
            }
            for p in &peers {
                let participant = upsert_participant(shared, p);
                let _ = shared
                    .events
                    .send(CallEvent::ParticipantJoined { participant });
                send_local_state(shared, &p.id);
            }
            promote_mesh_to_sfu(shared, peers.len() + 1).await;
            let _ = shared.events.send(CallEvent::RoomJoined { room_id });
        }
        SignalMessage::RoomDeleted { room_id } => {
            *shared.room_id.write().unwrap() = None;
            shared.roster.write().unwrap().clear();
            shared.remote_media.write().unwrap().clear();
            close_all_peers(shared).await;
            close_sfu_peer(shared).await;
            let _ = shared.events.send(CallEvent::RoomDeleted { room_id });
        }
        SignalMessage::PeerJoined { peer } => {
            let participant = upsert_participant(shared, &peer);
            let _ = shared
                .events
                .send(CallEvent::ParticipantJoined { participant });
            send_local_state(shared, &peer.id);
            let participant_count = shared.roster.read().unwrap().len() + 1;
            if promote_mesh_to_sfu(shared, participant_count).await {
                return;
            }
            if shared.cfg.read().unwrap().connection_mode == ConnectionMode::Mesh {
                if let Err(e) = offer_to(shared, &peer.id).await {
                    tracing::warn!("failed to offer to {}: {e}", peer.id);
                    let _ = shared.events.send(CallEvent::Error {
                        message: format!("Could not start media with {}: {e}", peer.name),
                    });
                }
            }
        }
        SignalMessage::PeerLeft { id } => {
            shared.roster.write().unwrap().remove(&id);
            shared.remote_media.write().unwrap().remove(&id);
            close_peer(shared, &id).await;
            let _ = shared.events.send(CallEvent::ParticipantLeft { id });
        }
        SignalMessage::FromPeer { from, payload } => match payload {
            PeerPayload::Offer { sdp } => match serde_json::from_str::<SessionDescription>(&sdp) {
                Ok(desc) => {
                    if let Err(e) = answer_peer(shared, &from, desc).await {
                        tracing::warn!("failed to answer {}: {e}", from);
                        let _ = shared.events.send(CallEvent::Error {
                            message: format!("Could not answer media connection from {from}: {e}"),
                        });
                    }
                }
                Err(e) => tracing::warn!("bad offer sdp from {from}: {e}"),
            },
            PeerPayload::Answer { sdp } => match serde_json::from_str::<SessionDescription>(&sdp) {
                Ok(desc) => {
                    let peer = shared.peers.read().unwrap().get(&from).cloned();
                    if let Some(peer) = peer {
                        if let Err(e) = peer.set_remote_description(&desc).await {
                            tracing::warn!("failed to set answer on {from}: {e}");
                        } else {
                            flush_pending_candidates(shared, &from).await;
                        }
                    }
                }
                Err(e) => tracing::warn!("bad answer sdp from {from}: {e}"),
            },
            PeerPayload::IceCandidate { candidate } => {
                match serde_json::from_str::<IceCandidate>(&candidate) {
                    Ok(cand) => add_or_buffer_candidate(shared, &from, cand).await,
                    Err(e) => tracing::warn!("bad ice candidate from {from}: {e}"),
                }
            }
            PeerPayload::MediaState { mic_on, screen_on } => {
                shared
                    .remote_media
                    .write()
                    .unwrap()
                    .insert(from.clone(), RemoteMediaState { mic_on, screen_on });
                if let Some(participant) = update_participant(shared, &from, |p| {
                    p.mic_on = mic_on;
                    p.screen_on = screen_on;
                }) {
                    let _ = shared
                        .events
                        .send(CallEvent::ParticipantUpdated { participant });
                }
            }
        },
        SignalMessage::TurnCredentials {
            urls,
            username,
            credential,
        } => {
            let mut cfg = shared.cfg.write().unwrap();
            cfg.peer_config
                .ice_servers
                .push(sharescreen_webrtc::IceServer {
                    urls,
                    username,
                    credential,
                });
            tracing::info!(
                "received TURN credentials, {} ICE server(s)",
                cfg.peer_config.ice_servers.len()
            );
        }
        SignalMessage::SfuAnswer { sdp } => {
            let parsed = serde_json::from_str::<SessionDescription>(&sdp);
            let sfu_peer = shared.sfu_peer.read().unwrap().clone();
            match (parsed, sfu_peer) {
                (Ok(answer), Some(peer)) => {
                    if let Err(error) = peer.set_remote_description(&answer).await {
                        let _ = shared.events.send(CallEvent::Error {
                            message: error.to_string(),
                        });
                    } else {
                        shared.sfu_negotiated.store(true, Ordering::Release);
                        shared.sfu_negotiating.store(false, Ordering::Release);
                        flush_sfu_candidates(shared).await;
                        if shared.sfu_subscription_dirty.swap(false, Ordering::AcqRel) {
                            if let Err(error) = renegotiate_sfu(shared).await {
                                let _ = shared.events.send(CallEvent::Error {
                                    message: format!("SFU renegotiation failed: {error}"),
                                });
                            }
                        }
                    }
                }
                (Err(error), _) => tracing::warn!("bad SFU answer: {error}"),
                (_, None) => tracing::warn!("received SFU answer without an active peer"),
            }
        }
        SignalMessage::SfuIceCandidate { candidate } => {
            match serde_json::from_str::<IceCandidate>(&candidate) {
                Ok(candidate) => add_or_buffer_sfu_candidate(shared, candidate).await,
                Err(error) => tracing::warn!("bad SFU ICE candidate: {error}"),
            }
        }
        SignalMessage::SfuSubscriptions { tracks, .. } => {
            let tracks: HashSet<String> = tracks.into_iter().collect();
            let removed = {
                let mut last = shared.sfu_subscriptions.write().unwrap();
                if last.as_ref() == Some(&tracks) {
                    None
                } else {
                    let removed = last
                        .as_ref()
                        .map(|previous| previous.difference(&tracks).cloned().collect::<Vec<_>>());
                    *last = Some(tracks);
                    Some(removed.unwrap_or_default())
                }
            };
            if let Some(removed) = removed {
                for track_id in removed {
                    shared.remote.detach_frame_key(&track_id);
                }
                shared.sfu_subscription_dirty.store(true, Ordering::Release);
                if shared.sfu_negotiated.load(Ordering::Acquire) {
                    if let Err(error) = renegotiate_sfu(shared).await {
                        let _ = shared.events.send(CallEvent::Error {
                            message: format!("SFU renegotiation failed: {error}"),
                        });
                    }
                }
            }
        }
        SignalMessage::SfuKeyframeRequest { track_id } => {
            let _ = shared
                .events
                .send(CallEvent::KeyframeRequested { track_id });
        }
        SignalMessage::Error { message } => {
            let _ = shared.events.send(CallEvent::Error { message });
        }
        _ => {}
    }
}

async fn promote_mesh_to_sfu(shared: &Shared, participant_count: usize) -> bool {
    if !mesh_needs_sfu(participant_count) {
        return false;
    }
    {
        let mut cfg = shared.cfg.write().unwrap();
        if cfg.connection_mode != ConnectionMode::Mesh {
            return false;
        }
        cfg.connection_mode = ConnectionMode::Sfu;
    }

    let peers: Vec<String> = shared.peers.read().unwrap().keys().cloned().collect();
    for peer_id in peers {
        close_peer(shared, &peer_id).await;
    }
    let _ = shared.events.send(CallEvent::Error {
        message: format!(
            "This room has more than {MAX_MESH_PARTICIPANTS} participants; switching this call to the SFU."
        ),
    });
    if let Err(error) = offer_sfu(shared).await {
        let _ = shared.events.send(CallEvent::Error {
            message: format!("Could not switch this call to the SFU: {error}"),
        });
    }
    true
}

fn mesh_needs_sfu(participant_count: usize) -> bool {
    participant_count > MAX_MESH_PARTICIPANTS
}

/// What a remote participant is publishing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct RemoteMediaState {
    mic_on: bool,
    screen_on: bool,
}

/// Insert or refresh a participant from the signaling roster, preserving any
/// media state already learned over the peer-to-peer channel.
fn upsert_participant(shared: &Shared, peer: &PeerInfo) -> Participant {
    let media = shared.remote_media.read().unwrap().get(&peer.id).copied();
    let mut roster = shared.roster.write().unwrap();
    let entry = roster
        .entry(peer.id.clone())
        .or_insert_with(|| Participant {
            id: peer.id.clone(),
            name: peer.name.clone(),
            mic_on: false,
            screen_on: false,
            connection: ConnectionState::New,
        });
    entry.name = peer.name.clone();
    if let Some(media) = media {
        entry.mic_on = media.mic_on;
        entry.screen_on = media.screen_on;
    }
    entry.clone()
}

/// Apply a change to one participant and return the updated record.
///
/// A peer that is not in the roster yet is skipped: its media state is kept
/// separately and applied once the roster names it.
fn update_participant(
    shared: &Shared,
    peer_id: &str,
    change: impl FnOnce(&mut Participant),
) -> Option<Participant> {
    let mut roster = shared.roster.write().unwrap();
    let participant = roster.get_mut(peer_id)?;
    change(participant);
    Some(participant.clone())
}

/// Send this peer's media state to one participant.
fn send_local_state(shared: &Shared, peer_id: &str) {
    let local = *shared.local.read().unwrap();
    let _ = shared.sender.send(SignalMessage::ToPeer {
        to: peer_id.to_string(),
        payload: PeerPayload::MediaState {
            mic_on: local.mic_on,
            screen_on: local.screen_on,
        },
    });
}

/// Send this peer's media state to every participant in the room.
fn broadcast_local_state(shared: &Shared) {
    let peers: Vec<String> = shared.roster.read().unwrap().keys().cloned().collect();
    for peer_id in peers {
        send_local_state(shared, &peer_id);
    }
}

/// Create a peer connection with the standard local tracks and offer to it.
async fn offer_to(shared: &Shared, peer_id: &str) -> Result<(), CallError> {
    if shared.cfg.read().unwrap().connection_mode == ConnectionMode::Sfu {
        tracing::info!("not offering to {peer_id}: this call uses the SFU");
        return Ok(());
    }
    tracing::info!("offering to {peer_id}");
    let peer = create_peer(shared, peer_id).await?;
    let offer = peer.create_offer().await?;
    // Applying the local description starts ICE gathering and sends our
    // candidates; without it the remote never gets a usable candidate pair
    // and the connection stalls in `Connecting`.
    peer.set_local_description(&offer).await?;
    shared.sender.send(SignalMessage::ToPeer {
        to: peer_id.to_string(),
        payload: PeerPayload::Offer {
            sdp: serde_json::to_string(&offer)?,
        },
    })?;
    Ok(())
}

async fn offer_sfu(shared: &Shared) -> Result<(), CallError> {
    if shared.sfu_peer.read().unwrap().is_some() {
        return Ok(());
    }
    let cfg = shared.cfg.read().unwrap().clone();
    let shared_for_events = shared.clone();
    let events: Arc<dyn Fn(PeerEvent) + Send + Sync> = Arc::new(move |event| match event {
        PeerEvent::IceCandidate(candidate) => {
            if let Ok(candidate) = serde_json::to_string(&candidate) {
                let _ = shared_for_events
                    .sender
                    .send(SignalMessage::SfuIceCandidate { candidate });
            }
        }
        PeerEvent::TrackReceived { track } => {
            let remote = shared_for_events.remote.clone();
            let subscriptions = shared_for_events.sfu_subscriptions.clone();
            let events = shared_for_events.events.clone();
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    let publisher = track.stream_id().await;
                    let track_id = track.track_id().await;
                    if subscriptions
                        .read()
                        .unwrap()
                        .as_ref()
                        .is_some_and(|subscriptions| !subscriptions.contains(&track_id))
                    {
                        return;
                    }
                    if let Err(error) = remote.attach_named(&publisher, &track_id, track).await {
                        tracing::warn!(%publisher, %error, "failed to attach SFU track");
                        let _ = events.send(CallEvent::Error {
                            message: format!("Could not decode remote track {track_id}: {error}"),
                        });
                    }
                });
            }
        }
        PeerEvent::ConnectionState(state) => {
            tracing::info!("SFU peer state: {state:?}");
            let epoch = shared_for_events
                .sfu_connection_epoch
                .fetch_add(1, Ordering::AcqRel)
                + 1;
            if matches!(
                state,
                ConnectionState::Disconnected | ConnectionState::Failed
            ) {
                let reconnect = shared_for_events.clone();
                if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                    runtime.spawn(async move {
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                        if reconnect.sfu_connection_epoch.load(Ordering::Acquire) != epoch
                            || reconnect.closing.load(Ordering::Acquire)
                            || !reconnect.sfu_published.load(Ordering::Acquire)
                        {
                            return;
                        }
                        if let Err(error) = reconnect.sfu_reconnect.send(epoch) {
                            let _ = reconnect.events.send(CallEvent::Error {
                                message: format!("Could not queue SFU reconnect: {error}"),
                            });
                        }
                    });
                }
            }
        }
        _ => {}
    });
    let peer = Arc::new(WebRtcPeer::new(cfg.peer_config.clone(), cfg.video_codec, events).await?);
    let mic_id = format!("{}-mic", cfg.self_id);
    let system_id = format!("{}-system", cfg.self_id);
    let screen_id = format!("{}-screen", cfg.self_id);
    #[cfg(target_os = "linux")]
    let thumbnail_id = format!("{}-screen-thumb", cfg.self_id);
    let mic = peer
        .add_relay_track(&cfg.self_id, &mic_id, Codec::Opus)
        .await?;
    let system = peer
        .add_relay_track(&cfg.self_id, &system_id, Codec::Opus)
        .await?;
    let screen = peer
        .add_relay_track(&cfg.self_id, &screen_id, cfg.video_codec)
        .await?;
    #[cfg(target_os = "linux")]
    let thumbnail = peer
        .add_relay_track(&cfg.self_id, &thumbnail_id, cfg.video_codec)
        .await?;
    shared.mic.register("__sfu__", Arc::new(mic));
    shared.system.register("__sfu__", Arc::new(system));
    shared.screen.register("__sfu__", Arc::new(screen));
    #[cfg(target_os = "linux")]
    shared.thumbnail.register("__sfu__", Arc::new(thumbnail));
    for _ in 0..(SFU_MAX_REMOTE_PARTICIPANTS * SFU_AUDIO_TRACKS_PER_PARTICIPANT) {
        peer.add_recvonly_transceiver(sharescreen_webrtc::RemoteTrackKind::Audio)
            .await?;
    }
    let video_slots = SFU_MAX_REMOTE_PARTICIPANTS * if cfg!(target_os = "linux") { 2 } else { 1 };
    for _ in 0..video_slots {
        peer.add_recvonly_transceiver(sharescreen_webrtc::RemoteTrackKind::Video)
            .await?;
    }

    if !shared.sfu_published.load(Ordering::Acquire) {
        let mut publications = vec![
            (mic_id, PublishedTrackKind::Audio, None),
            (system_id, PublishedTrackKind::Audio, None),
            (
                screen_id,
                PublishedTrackKind::Screen,
                Some(sharescreen_core::PublishedVideoLayer::Full),
            ),
        ];
        #[cfg(target_os = "linux")]
        publications.push((
            thumbnail_id,
            PublishedTrackKind::Screen,
            Some(sharescreen_core::PublishedVideoLayer::Thumbnail),
        ));
        for (track_id, kind, layer) in publications {
            shared.sender.send(SignalMessage::PublishTrack {
                track_id,
                kind,
                layer,
            })?;
        }
        shared.sfu_published.store(true, Ordering::Release);
    }
    let offer = peer.create_offer().await?;
    peer.set_local_description(&offer).await?;
    shared.sfu_negotiating.store(true, Ordering::Release);
    shared.sender.send(SignalMessage::SfuOffer {
        sdp: serde_json::to_string(&offer)?,
        video_codec: format!("{:?}", cfg.video_codec).to_ascii_lowercase(),
    })?;
    *shared.sfu_peer.write().unwrap() = Some(peer);
    Ok(())
}

async fn renegotiate_sfu(shared: &Shared) -> Result<(), CallError> {
    if shared
        .sfu_negotiating
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Ok(());
    }
    let peer = shared.sfu_peer.read().unwrap().clone();
    let Some(peer) = peer else {
        shared.sfu_negotiating.store(false, Ordering::Release);
        return Ok(());
    };
    let offer = match peer.create_offer().await {
        Ok(offer) => offer,
        Err(error) => {
            shared.sfu_negotiating.store(false, Ordering::Release);
            return Err(error.into());
        }
    };
    if let Err(error) = peer.set_local_description(&offer).await {
        shared.sfu_negotiating.store(false, Ordering::Release);
        return Err(error.into());
    }
    let result = shared.sender.send(SignalMessage::SfuOffer {
        sdp: serde_json::to_string(&offer)?,
        video_codec: format!("{:?}", shared.cfg.read().unwrap().video_codec).to_ascii_lowercase(),
    });
    if result.is_err() {
        shared.sfu_negotiating.store(false, Ordering::Release);
    }
    result?;
    Ok(())
}

async fn close_sfu_peer(shared: &Shared) {
    close_sfu_transport(shared).await;
    shared.sfu_published.store(false, Ordering::Release);
}

async fn close_sfu_transport(shared: &Shared) {
    shared.sfu_negotiated.store(false, Ordering::Release);
    shared.sfu_negotiating.store(false, Ordering::Release);
    shared
        .sfu_subscription_dirty
        .store(false, Ordering::Release);
    let subscriptions = shared.sfu_subscriptions.write().unwrap().take();
    if let Some(subscriptions) = subscriptions {
        for track_id in subscriptions {
            shared.remote.detach_frame_key(&track_id);
        }
    }
    shared.mic.unregister("__sfu__");
    shared.system.unregister("__sfu__");
    shared.screen.unregister("__sfu__");
    shared.thumbnail.unregister("__sfu__");
    shared.pending_ice.write().unwrap().remove("__sfu__");
    let peer = shared.sfu_peer.write().unwrap().take();
    if let Some(peer) = peer {
        let _ = peer.close().await;
    }
}

async fn add_or_buffer_sfu_candidate(shared: &Shared, candidate: IceCandidate) {
    let peer = shared.sfu_peer.read().unwrap().clone();
    if let Some(peer) = peer {
        if peer.add_ice_candidate(candidate.clone()).await.is_ok() {
            return;
        }
    }
    let mut pending = shared.pending_ice.write().unwrap();
    let queue = pending.entry("__sfu__".into()).or_default();
    if queue.len() < 128 {
        queue.push(candidate);
    }
}

async fn flush_sfu_candidates(shared: &Shared) {
    let pending = shared
        .pending_ice
        .write()
        .unwrap()
        .remove("__sfu__")
        .unwrap_or_default();
    let peer = shared.sfu_peer.read().unwrap().clone();
    if let Some(peer) = peer {
        for candidate in pending {
            let _ = peer.add_ice_candidate(candidate).await;
        }
    }
}

/// Answer an incoming offer.
async fn answer_peer(
    shared: &Shared,
    peer_id: &str,
    offer: SessionDescription,
) -> Result<(), CallError> {
    // In SFU mode all media flows through the server. Answering a mesh offer
    // would open a second media path for the same participant and double the
    // encode/decode and bandwidth.
    if shared.cfg.read().unwrap().connection_mode == ConnectionMode::Sfu {
        tracing::info!("ignoring mesh offer from {peer_id}: this call uses the SFU");
        return Ok(());
    }
    tracing::info!("answering {peer_id}");
    // Read the guard in its own statement: holding it across the `await` in
    // `create_peer` would make the future non-`Send`.
    let existing = shared.peers.read().unwrap().get(peer_id).cloned();
    let peer = match existing {
        Some(p) => p,
        None => create_peer(shared, peer_id).await?,
    };
    peer.set_remote_description(&offer).await?;
    flush_pending_candidates(shared, peer_id).await;
    let answer = peer.create_answer().await?;
    peer.set_local_description(&answer).await?;
    shared.sender.send(SignalMessage::ToPeer {
        to: peer_id.to_string(),
        payload: PeerPayload::Answer {
            sdp: serde_json::to_string(&answer)?,
        },
    })?;
    Ok(())
}

/// Build a peer connection with the mic, system-audio and screen tracks.
async fn create_peer(shared: &Shared, peer_id: &str) -> Result<Arc<WebRtcPeer>, CallError> {
    let cfg = shared.cfg.read().unwrap().clone();
    tracing::info!(
        "creating peer {peer_id} with mic/system/screen tracks (video {:?})",
        cfg.video_codec
    );
    let callback: Arc<dyn Fn(PeerEvent) + Send + Sync> =
        peer_event_handler(shared.clone(), peer_id.to_string());
    let peer = Arc::new(WebRtcPeer::new(cfg.peer_config.clone(), cfg.video_codec, callback).await?);

    let mic = peer.add_track(TrackId::Mic, Codec::Opus).await?;
    let system = peer.add_track(TrackId::System, Codec::Opus).await?;
    let screen = peer.add_track(TrackId::Screen, cfg.video_codec).await?;

    shared.mic.register(peer_id, Arc::new(mic));
    shared.system.register(peer_id, Arc::new(system));
    shared.screen.register(peer_id, Arc::new(screen));
    shared
        .peers
        .write()
        .unwrap()
        .insert(peer_id.to_string(), peer.clone());

    Ok(peer)
}

/// Drop a peer and its local track registrations.
async fn close_peer(shared: &Shared, peer_id: &str) {
    shared.mic.unregister(peer_id);
    shared.system.unregister(peer_id);
    shared.screen.unregister(peer_id);
    shared.pending_ice.write().unwrap().remove(peer_id);
    // Stop the decoder pipelines for this peer's tracks; otherwise they keep
    // running (and, for audio, keep playing) after the peer is gone.
    shared.remote.detach(peer_id);
    let peer = shared.peers.write().unwrap().remove(peer_id);
    if let Some(peer) = peer {
        let _ = peer.close().await;
    }
}

/// Close every peer connection and clear the roster.
async fn close_all_peers(shared: &Shared) {
    let peers: Vec<String> = shared.peers.read().unwrap().keys().cloned().collect();
    for peer_id in peers {
        close_peer(shared, &peer_id).await;
    }
    shared.roster.write().unwrap().clear();
    shared.remote_media.write().unwrap().clear();
    shared.pending_ice.write().unwrap().clear();
}

/// Try to add an ICE candidate for a peer; if the peer is not ready yet,
/// buffer the candidate until its remote description is set.
async fn add_or_buffer_candidate(shared: &Shared, from: &str, cand: IceCandidate) {
    let peer = shared.peers.read().unwrap().get(from).cloned();
    if let Some(peer) = peer {
        if peer.add_ice_candidate(cand.clone()).await.is_ok() {
            return;
        }
    }
    shared
        .pending_ice
        .write()
        .unwrap()
        .entry(from.to_string())
        .or_default()
        .push(cand);
    tracing::debug!("buffered ICE candidate for {from} (peer/description not ready)");
}

/// Flush ICE candidates buffered before the peer's remote description existed.
async fn flush_pending_candidates(shared: &Shared, peer_id: &str) {
    let pending = shared
        .pending_ice
        .write()
        .unwrap()
        .remove(peer_id)
        .unwrap_or_default();
    if pending.is_empty() {
        return;
    }
    let peer = shared.peers.read().unwrap().get(peer_id).cloned();
    if let Some(p) = peer {
        let n = pending.len();
        for cand in pending {
            let _ = p.add_ice_candidate(cand).await;
        }
        tracing::debug!("flushed {n} buffered ICE candidate(s) for {peer_id}");
    }
}

/// Build the per-peer event callback: relays ICE candidates and reports
/// connection state changes.
fn peer_event_handler(shared: Shared, peer_id: String) -> Arc<dyn Fn(PeerEvent) + Send + Sync> {
    Arc::new(move |ev: PeerEvent| match ev {
        PeerEvent::IceCandidate(candidate) => {
            if let Ok(json) = serde_json::to_string(&candidate) {
                let _ = shared.sender.send(SignalMessage::ToPeer {
                    to: peer_id.clone(),
                    payload: PeerPayload::IceCandidate { candidate: json },
                });
            }
        }
        PeerEvent::ConnectionState(state) => {
            tracing::info!("peer {peer_id} connection state: {state:?}");
            if let Some(participant) = update_participant(&shared, &peer_id, |p| {
                p.connection = state;
            }) {
                let _ = shared
                    .events
                    .send(CallEvent::ParticipantUpdated { participant });
            }
        }
        PeerEvent::TrackReceived { track } => {
            // The handler runs on the WebRTC runtime; hop to a task so the
            // decoder pipeline can be attached.
            let media = shared.remote.clone();
            let events = shared.events.clone();
            let peer = peer_id.clone();
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move {
                    if let Err(e) = media.attach(&peer, track).await {
                        tracing::warn!("failed to attach remote track from {peer}: {e}");
                        let _ = events.send(CallEvent::Error {
                            message: format!("Could not decode remote track from {peer}: {e}"),
                        });
                    }
                });
            }
        }
        PeerEvent::GatheringComplete | PeerEvent::NegotiationNeeded => {}
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mesh_limit_counts_the_local_participant() {
        assert!(!mesh_needs_sfu(3));
        assert!(mesh_needs_sfu(4));
    }
}

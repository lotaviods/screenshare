//! Call session: signaling + WebRTC mesh + local media routing.

mod hub;

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use sharescreen_core::{CallEvent, Codec, ConnectionState, EncodedSample, Participant};
use sharescreen_media::RemoteMedia;
use sharescreen_signaling::{
    Incoming, PeerInfo, PeerPayload, SignalMessage, SignalingClient, SignalingSender,
};
use sharescreen_webrtc::{
    IceCandidate, PeerConfig, PeerEvent, SessionDescription, TrackId, WebRtcPeer,
};
use tokio::sync::mpsc;

use crate::hub::MediaHub;

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
}

/// A live call: one peer connection per remote participant, plus the local
/// media channels that feed them.
pub struct CallSession {
    shared: Shared,
    mic_tx: mpsc::Sender<EncodedSample>,
    system_tx: mpsc::Sender<EncodedSample>,
    screen_tx: mpsc::Sender<EncodedSample>,
}

#[derive(Clone)]
struct Shared {
    /// Mutable so the server's TURN credentials can be folded in after the
    /// connection is established.
    cfg: Arc<RwLock<CallConfig>>,
    sender: SignalingSender,
    peers: Arc<RwLock<HashMap<String, Arc<WebRtcPeer>>>>,
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
    events: mpsc::UnboundedSender<CallEvent>,
    room_id: Arc<RwLock<Option<String>>>,
    /// Decoder pipelines for the remote participants' tracks.
    remote: Arc<RemoteMedia>,
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

        let mic = Arc::new(MediaHub::new());
        let system = Arc::new(MediaHub::new());
        let screen = Arc::new(MediaHub::new());

        let shared = Shared {
            cfg: Arc::new(RwLock::new(cfg.clone())),
            sender,
            peers: Arc::new(RwLock::new(HashMap::new())),
            roster: Arc::new(RwLock::new(HashMap::new())),
            remote_media: Arc::new(RwLock::new(HashMap::new())),
            pending_ice: Arc::new(RwLock::new(HashMap::new())),
            local: Arc::new(RwLock::new(LocalMedia::default())),
            mic: mic.clone(),
            system: system.clone(),
            screen: screen.clone(),
            events,
            room_id: Arc::new(RwLock::new(None)),
            remote: Arc::new(RemoteMedia::new()),
        };

        // One fan-out task per local media stream.
        tokio::spawn(hub_loop(mic, mic_rx));
        tokio::spawn(hub_loop(system, system_rx));
        tokio::spawn(hub_loop(screen, screen_rx));

        // Signaling dispatch.
        tokio::spawn(dispatch(shared.clone(), incoming));

        // Ask the server for time-limited TURN credentials; they are folded
        // into the ICE configuration before any peer connection is made.
        let _ = shared.sender.send(SignalMessage::GetTurnCredentials);

        Ok(Self {
            shared,
            mic_tx,
            system_tx,
            screen_tx,
        })
    }

    /// Ask the server to create a room.
    pub fn create_room(&self) -> Result<(), CallError> {
        self.shared.sender.send(SignalMessage::CreateRoom)?;
        Ok(())
    }

    /// Ask the server to join an existing room.
    pub fn join_room(&self, room_id: String) -> Result<(), CallError> {
        self.shared.sender.send(SignalMessage::JoinRoom { room_id })?;
        Ok(())
    }

    /// Leave the current room, tearing down the peer connections with it.
    pub async fn leave_room(&self) -> Result<(), CallError> {
        self.shared.sender.send(SignalMessage::LeaveRoom)?;
        close_all_peers(&self.shared).await;
        Ok(())
    }

    /// Close the current room for everyone in it. Any member may do this.
    pub async fn delete_room(&self) -> Result<(), CallError> {
        self.shared.sender.send(SignalMessage::DeleteRoom)?;
        close_all_peers(&self.shared).await;
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
    }

    /// The latest decoded video frame (JPEG) from a peer, if any.
    pub fn remote_frame(&self, peer_id: &str) -> Option<Vec<u8>> {
        self.shared.remote.latest_frame(peer_id)
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
        self.shared.remote.latest_frame(&sharer)
    }

    /// Set the playback volume for a peer's audio.
    pub fn set_peer_volume(&self, peer_id: &str, volume: f64) {
        self.shared.remote.set_peer_volume(peer_id, volume);
    }

    /// Close every peer connection and stop the decoders. Called when the
    /// user disconnects from the signaling server.
    pub async fn disconnect(&self) {
        close_all_peers(&self.shared).await;
    }
}

async fn hub_loop(hub: Arc<MediaHub>, mut rx: mpsc::Receiver<EncodedSample>) {
    while let Some(sample) = rx.recv().await {
        hub.broadcast(&sample).await;
    }
}

async fn dispatch(shared: Shared, mut incoming: mpsc::UnboundedReceiver<Incoming>) {
    while let Some(item) = incoming.recv().await {
        match item {
            Incoming::Message(msg) => handle_message(&shared, msg).await,
            Incoming::Disconnected => {
                let _ = shared.events.send(CallEvent::Error {
                    message: "signaling connection lost".into(),
                });
                break;
            }
        }
    }
}

async fn handle_message(shared: &Shared, msg: SignalMessage) {
    match msg {
        SignalMessage::RoomCreated { room_id } => {
            *shared.room_id.write().unwrap() = Some(room_id.clone());
            let _ = shared.events.send(CallEvent::RoomCreated { room_id });
        }
        SignalMessage::Joined { room_id, peers } => {
            *shared.room_id.write().unwrap() = Some(room_id.clone());
            for p in &peers {
                let participant = upsert_participant(shared, p);
                let _ = shared.events.send(CallEvent::ParticipantJoined { participant });
                // Tell the peers already in the room what we are publishing.
                send_local_state(shared, &p.id);
            }
            let _ = shared.events.send(CallEvent::RoomJoined { room_id });
        }
        SignalMessage::RoomDeleted { room_id } => {
            *shared.room_id.write().unwrap() = None;
            shared.roster.write().unwrap().clear();
            shared.remote_media.write().unwrap().clear();
            close_all_peers(shared).await;
            let _ = shared.events.send(CallEvent::RoomDeleted { room_id });
        }
        SignalMessage::PeerJoined { peer } => {
            let participant = upsert_participant(shared, &peer);
            let _ = shared.events.send(CallEvent::ParticipantJoined { participant });
            send_local_state(shared, &peer.id);
            // Peers already in the room offer to the newcomer.
            if let Err(e) = offer_to(shared, &peer.id).await {
                tracing::warn!("failed to offer to {}: {e}", peer.id);
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
                    let _ = shared.events.send(CallEvent::ParticipantUpdated { participant });
                }
            }
        },
        SignalMessage::TurnCredentials {
            urls,
            username,
            credential,
        } => {
            let mut cfg = shared.cfg.write().unwrap();
            cfg.peer_config.ice_servers.push(sharescreen_webrtc::IceServer {
                urls,
                username,
                credential,
            });
            tracing::info!("received TURN credentials, {} ICE server(s)", cfg.peer_config.ice_servers.len());
        }
        SignalMessage::Error { message } => {
            let _ = shared.events.send(CallEvent::Error { message });
        }
        _ => {}
    }
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

/// Answer an incoming offer.
async fn answer_peer(
    shared: &Shared,
    peer_id: &str,
    offer: SessionDescription,
) -> Result<(), CallError> {
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
    let callback: Arc<dyn Fn(PeerEvent) + Send + Sync> =
        peer_event_handler(shared.clone(), peer_id.to_string());
    let peer = Arc::new(
        WebRtcPeer::new(cfg.peer_config.clone(), cfg.video_codec, callback).await?,
    );

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
        for cand in pending {
            let _ = p.add_ice_candidate(cand).await;
        }
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
            if let Some(participant) = update_participant(&shared, &peer_id, |p| {
                p.connection = state;
            }) {
                let _ = shared.events.send(CallEvent::ParticipantUpdated { participant });
            }
        }
        PeerEvent::TrackReceived { track } => {
            // The handler runs on the WebRTC runtime; hop to a task so the
            // decoder pipeline can be attached.
            let media = shared.remote.clone();
            let peer = peer_id.clone();
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move {
                    if let Err(e) = media.attach(&peer, track).await {
                        tracing::warn!("failed to attach remote track from {peer}: {e}");
                    }
                });
            }
        }
        PeerEvent::GatheringComplete | PeerEvent::NegotiationNeeded => {}
    })
}

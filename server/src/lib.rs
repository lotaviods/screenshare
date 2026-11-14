//! `sharescreen-server`: the signaling server.
//!
//! An axum + WebSocket server that manages rooms and relays SDP/ICE between
//! peers. It never proxies media; WebRTC connects peers directly (or via
//! TURN). See `ARCHITECTURE.md`.

pub mod rooms;
pub mod sfu;
pub mod turn;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::response::Response;
use axum::routing::get;
use axum::{Json, Router};
use sharescreen_core::{Codec, PeerInfo, PublishedTrackKind, PublishedVideoLayer, SignalMessage};
use sharescreen_webrtc::{
    IceCandidate, IceServer, PeerConfig, PeerEvent, RemoteTrack, RtpPacket, SessionDescription,
    WebRtcPeer,
};
use tokio::sync::mpsc;

use rooms::RoomManager;
use turn::{TurnConfig, TurnCredential};

/// Build the signaling router, reading the TURN configuration from the
/// environment.
pub fn app() -> Router {
    app_with(turn::from_env())
}

/// Build the signaling router with an explicit TURN configuration.
pub fn app_with(turn_config: TurnConfig) -> Router {
    let state = AppState {
        rooms: Arc::new(RoomManager::new()),
        sfu_rooms: Arc::new(Mutex::new(HashMap::new())),
        sfu_peers: Arc::new(Mutex::new(HashMap::new())),
        sfu_pending_ice: Arc::new(Mutex::new(HashMap::new())),
        sfu_forwarder: Arc::new(sfu::RtpForwarder::new()),
        sfu_sources: Arc::new(Mutex::new(HashMap::new())),
        turn: turn_config,
    };
    Router::new()
        .route("/ws", get(ws_handler))
        .route("/turn-credentials", get(turn_credentials))
        .with_state(state)
}

#[derive(Clone)]
struct AppState {
    rooms: Arc<RoomManager>,
    sfu_rooms: Arc<Mutex<HashMap<String, sfu::SfuRoom>>>,
    sfu_peers: Arc<Mutex<HashMap<String, Arc<WebRtcPeer>>>>,
    sfu_pending_ice: Arc<Mutex<HashMap<String, Vec<IceCandidate>>>>,
    sfu_forwarder: Arc<sfu::RtpForwarder<RtpPacket>>,
    sfu_sources: Arc<Mutex<HashMap<String, tokio::task::JoinHandle<()>>>>,
    turn: TurnConfig,
}

/// Current unix time in seconds.
fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// HTTP endpoint mirroring the WebSocket flow, for clients that prefer REST.
async fn turn_credentials(State(state): State<AppState>) -> Json<TurnCredential> {
    Json(state.turn.issue(now_unix()))
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let id = params.get("id").cloned().unwrap_or_default();
    let name = params.get("name").cloned().unwrap_or_default();
    ws.on_upgrade(move |socket| handle_connection(socket, state, id, name))
}

async fn handle_connection(mut socket: WebSocket, state: AppState, id: String, name: String) {
    let info = PeerInfo {
        id: id.clone(),
        name,
    };
    let (tx, mut rx) = mpsc::unbounded_channel::<SignalMessage>();
    let mut room_id: Option<String> = None;

    loop {
        tokio::select! {
            // Outbound messages (from room relay) → WebSocket.
            msg = rx.recv() => {
                match msg {
                    Some(m) => {
                        if let Ok(text) = serde_json::to_string(&m) {
                            if socket.send(Message::Text(text.into())).await.is_err() {
                                break;
                            }
                        }
                    }
                    None => break,
                }
            }
            // Inbound WebSocket messages → process.
            incoming = socket.recv() => {
                let Some(Ok(msg)) = incoming else { break };
                match msg {
                    Message::Text(text) => {
                        let Ok(m) = serde_json::from_str::<SignalMessage>(text.as_str()) else {
                            let _ = tx.send(SignalMessage::Error {
                                message: "malformed message".into(),
                            });
                            continue;
                        };
                        match m {
                            SignalMessage::CreateRoom => {
                                if room_id.is_some() {
                                    let _ = tx.send(SignalMessage::Error {
                                        message: "already in a room".into(),
                                    });
                                    continue;
                                }
                                let rid = state.rooms.create_room();
                                room_id = Some(rid.clone());
                                let _ = state.rooms.join(&rid, info.clone(), tx.clone());
                                state.sfu_rooms.lock().unwrap().entry(rid.clone()).or_default().join(id.clone());
                                let _ = tx.send(SignalMessage::RoomCreated { room_id: rid });
                            }
                            SignalMessage::JoinRoom { room_id: rid } => {
                                if room_id.is_some() {
                                    let _ = tx.send(SignalMessage::Error {
                                        message: "already in a room".into(),
                                    });
                                    continue;
                                }
                                match state.rooms.join(&rid, info.clone(), tx.clone()) {
                                    Ok(existing) => {
                                        room_id = Some(rid.clone());
                                        state.sfu_rooms.lock().unwrap().entry(rid.clone()).or_default().join(id.clone());
                                        for p in &existing {
                                            let _ = state.rooms.send_to(
                                                &rid,
                                                &p.id,
                                                SignalMessage::PeerJoined { peer: info.clone() },
                                            );
                                        }
                                        let _ = tx.send(SignalMessage::Joined {
                                            room_id: rid.clone(),
                                            peers: existing,
                                        });
                                        if let Some(sfu_room) = state.sfu_rooms.lock().unwrap().get(&rid) {
                                            notify_subscriptions(&state, &rid, sfu_room);
                                        }
                                    }
                                    Err(e) => {
                                        let _ = tx.send(SignalMessage::Error {
                                            message: e.to_string(),
                                        });
                                    }
                                }
                            }
                            SignalMessage::LeaveRoom => {
                                if let Some(rid) = room_id.take() {
                                    close_sfu_peer(&state, &rid, &id).await;
                                    if let Some(sfu_room) = state.sfu_rooms.lock().unwrap().get_mut(&rid) {
                                        sfu_room.leave(&id);
                                    }
                                    if let Some(remaining) = state.rooms.leave(&rid, &id) {
                                        for p in remaining {
                                            let _ = state.rooms.send_to(
                                                &rid,
                                                &p.id,
                                                SignalMessage::PeerLeft { id: id.clone() },
                                            );
                                        }
                                    }
                                    if let Some(sfu_room) = state.sfu_rooms.lock().unwrap().get(&rid) {
                                        notify_subscriptions(&state, &rid, sfu_room);
                                    }
                                }
                            }
                            SignalMessage::DeleteRoom => {
                                let Some(rid) = room_id.clone() else {
                                    let _ = tx.send(SignalMessage::Error {
                                        message: "not in a room".into(),
                                    });
                                    continue;
                                };
                                match state.rooms.delete(&rid) {
                                    Some(peers) => {
                                        room_id = None;
                                        state.sfu_rooms.lock().unwrap().remove(&rid);
                                        close_room_sfu_peers(&state, &rid).await;
                                        for (_, peer_tx) in peers {
                                            let _ = peer_tx.send(SignalMessage::RoomDeleted {
                                                room_id: rid.clone(),
                                            });
                                        }
                                    }
                                    None => {
                                        room_id = None;
                                        let _ = tx.send(SignalMessage::Error {
                                            message: "room not found".into(),
                                        });
                                    }
                                }
                            }
                            SignalMessage::GetTurnCredentials => {
                                let cred = state.turn.issue(now_unix());
                                let _ = tx.send(SignalMessage::TurnCredentials {
                                    urls: cred.urls,
                                    username: cred.username,
                                    credential: cred.credential,
                                });
                            }
                            SignalMessage::ToPeer { to, payload } => {
                                if let Some(rid) = &room_id {
                                    if let Err(e) = state.rooms.relay(rid, &id, &to, payload) {
                                        let _ = tx.send(SignalMessage::Error {
                                            message: e.to_string(),
                                        });
                                    }
                                }
                            }
                            SignalMessage::PublishTrack { track_id, kind, layer } => {
                                let Some(rid) = &room_id else {
                                    let _ = tx.send(SignalMessage::Error { message: "not in a room".into() });
                                    continue;
                                };
                                let kind = match kind {
                                    PublishedTrackKind::Audio => sfu::TrackKind::Audio,
                                    PublishedTrackKind::Screen => sfu::TrackKind::Screen,
                                };
                                let layer = layer.map(|layer| match layer {
                                    PublishedVideoLayer::Full => sfu::VideoLayer::Full,
                                    PublishedVideoLayer::Thumbnail => sfu::VideoLayer::Thumbnail,
                                });
                                let mut rooms = state.sfu_rooms.lock().unwrap();
                                if let Some(sfu_room) = rooms.get_mut(rid) {
                                    if let Err(error) = sfu_room.publish(sfu::PublishedTrack {
                                        id: track_id,
                                        publisher: id.clone(),
                                        kind,
                                        layer,
                                    }) {
                                        let _ = tx.send(SignalMessage::Error { message: error.to_string() });
                                    } else {
                                        notify_subscriptions(&state, rid, sfu_room);
                                    }
                                }
                            }
                            SignalMessage::UnpublishTrack { track_id } => {
                                if let Some(rid) = &room_id {
                                    let mut rooms = state.sfu_rooms.lock().unwrap();
                                    if let Some(sfu_room) = rooms.get_mut(rid) {
                                        if sfu_room.track_publisher(&track_id) == Some(id.as_str()) {
                                            if sfu_room.unpublish(&track_id) {
                                                remove_sfu_source(&state, rid, &id, &track_id);
                                            }
                                            notify_subscriptions(&state, rid, sfu_room);
                                        }
                                    }
                                }
                            }
                            SignalMessage::SetFocus { publisher } => {
                                let Some(rid) = &room_id else {
                                    let _ = tx.send(SignalMessage::Error { message: "not in a room".into() });
                                    continue;
                                };
                                let mut rooms = state.sfu_rooms.lock().unwrap();
                                if let Some(sfu_room) = rooms.get_mut(rid) {
                                    if let Err(error) = sfu_room.set_focus(&id, publisher.as_deref()) {
                                        let _ = tx.send(SignalMessage::Error { message: error.to_string() });
                                    } else {
                                        notify_subscriptions(&state, rid, sfu_room);
                                    }
                                }
                            }
                            SignalMessage::SfuOffer { sdp, video_codec } => {
                                let Some(rid) = &room_id else {
                                    let _ = tx.send(SignalMessage::Error { message: "not in a room".into() });
                                    continue;
                                };
                                if let Err(error) = handle_sfu_offer(
                                    &state, rid, &id, &tx, &sdp, &video_codec,
                                ).await {
                                    let _ = tx.send(SignalMessage::Error { message: error });
                                }
                            }
                            SignalMessage::SfuAnswer { sdp } => {
                                let Some(rid) = &room_id else { continue };
                                let key = sfu_peer_key(rid, &id);
                                let peer = state.sfu_peers.lock().unwrap().get(&key).cloned();
                                if let Some(peer) = peer {
                                    match serde_json::from_str::<SessionDescription>(&sdp) {
                                        Ok(answer) => {
                                            if let Err(error) = peer.set_remote_description(&answer).await {
                                                let _ = tx.send(SignalMessage::Error { message: error.to_string() });
                                            }
                                        }
                                        Err(error) => {
                                            let _ = tx.send(SignalMessage::Error { message: format!("invalid SFU answer: {error}") });
                                        }
                                    }
                                }
                            }
                            SignalMessage::SfuIceCandidate { candidate } => {
                                if let Some(rid) = &room_id {
                                    handle_sfu_candidate(&state, rid, &id, &candidate).await;
                                }
                            }
                            _ => {}
                        }
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
        }
    }

    // Cleanup on disconnect: leave the room and notify the remaining peers.
    if let Some(rid) = room_id {
        close_sfu_peer(&state, &rid, &id).await;
        if let Some(sfu_room) = state.sfu_rooms.lock().unwrap().get_mut(&rid) {
            sfu_room.leave(&id);
        }
        if let Some(remaining) = state.rooms.leave(&rid, &id) {
            for p in remaining {
                let _ =
                    state
                        .rooms
                        .send_to(&rid, &p.id, SignalMessage::PeerLeft { id: id.clone() });
            }
        }
        if let Some(sfu_room) = state.sfu_rooms.lock().unwrap().get(&rid) {
            notify_subscriptions(&state, &rid, sfu_room);
        }
    }
}

fn notify_subscriptions(state: &AppState, room_id: &str, room: &sfu::SfuRoom) {
    for viewer in state.rooms.peer_ids(room_id) {
        let has_sfu_peer = state
            .sfu_peers
            .lock()
            .unwrap()
            .contains_key(&sfu_peer_key(room_id, &viewer));
        if !has_sfu_peer && !room.has_publications() {
            continue;
        }
        let subscriptions = room.subscriptions_for(&viewer);
        let _ = state.rooms.send_to(
            room_id,
            &viewer,
            SignalMessage::SfuSubscriptions {
                focus: subscriptions.focus,
                tracks: subscriptions.tracks.into_iter().collect(),
            },
        );
    }
}

fn sfu_peer_key(room_id: &str, peer_id: &str) -> String {
    format!("{room_id}:{peer_id}")
}

fn sfu_track_key(room_id: &str, track_id: &str) -> String {
    format!("{}:{room_id}{}:{track_id}", room_id.len(), track_id.len())
}

fn sfu_source_prefix(room_id: &str, publisher: &str) -> String {
    format!("{}:{room_id}{}:{publisher}", room_id.len(), publisher.len())
}

fn attach_relay_source(
    forwarder: Arc<sfu::RtpForwarder<RtpPacket>>,
    sources: Arc<Mutex<HashMap<String, tokio::task::JoinHandle<()>>>>,
    room_id: String,
    publisher: String,
    track: RemoteTrack,
) {
    tokio::spawn(async move {
        let publication_id = track.track_id().await;
        let relay_id = sfu_track_key(&room_id, &publication_id);
        let source_id = format!(
            "{}{}:{}",
            sfu_source_prefix(&room_id, &publisher),
            publication_id.len(),
            publication_id
        );
        let task = forwarder.attach_source(relay_id, track);
        if let Some(previous) = sources.lock().unwrap().insert(source_id, task) {
            previous.abort();
        }
    });
}

fn remove_sfu_source(state: &AppState, room_id: &str, publisher: &str, track_id: &str) {
    let source_id = format!(
        "{}{}:{track_id}",
        sfu_source_prefix(room_id, publisher),
        track_id.len()
    );
    if let Some(task) = state.sfu_sources.lock().unwrap().remove(&source_id) {
        task.abort();
    }
    state
        .sfu_forwarder
        .remove_track(&sfu_track_key(room_id, track_id));
}

async fn handle_sfu_offer(
    state: &AppState,
    room_id: &str,
    peer_id: &str,
    tx: &mpsc::UnboundedSender<SignalMessage>,
    sdp: &str,
    video_codec: &str,
) -> Result<(), String> {
    let codec = match video_codec.to_ascii_lowercase().as_str() {
        "vp8" => Codec::Vp8,
        "vp9" => Codec::Vp9,
        "h264" => Codec::H264,
        "h265" | "hevc" => Codec::H265,
        "av1" => Codec::Av1,
        _ => return Err(format!("unsupported SFU video codec: {video_codec}")),
    };
    let key = sfu_peer_key(room_id, peer_id);

    // Check whether a live WebRTC peer already exists for this client. If it
    // does, the client is renegotiating (e.g. a new publisher joined and the
    // subscription set changed). We MUST NOT close the existing peer: doing so
    // would destroy the ICE transport that the client is currently using.
    let existing_peer = state.sfu_peers.lock().unwrap().get(&key).cloned();

    if let Some(peer) = existing_peer {
        // ── Renegotiation path ────────────────────────────────────────────────
        // The peer connection is alive. Only add egress tracks for publications
        // that are not yet wired into the forwarder for this viewer; tracks
        // from the first negotiation are already forwarding and must not be
        // re-added (that would spawn a duplicate writer task and replace the
        // channel in the forwarder).
        tracing::info!(
            peer_id,
            room_id,
            "SFU renegotiation: reusing existing peer, adding new tracks only"
        );

        let publications: Vec<sfu::PublishedTrack> = {
            let rooms = state.sfu_rooms.lock().unwrap();
            let Some(room) = rooms.get(room_id) else {
                return Err("SFU room not found".into());
            };
            let subscribed = room.subscriptions_for(peer_id).tracks;
            room.publications()
                .filter(|publication| subscribed.contains(&publication.id))
                .cloned()
                .collect()
        };

        for publication in publications {
            let track_key = sfu_track_key(room_id, &publication.id);
            // Skip tracks the viewer already receives; they are already in the
            // forwarder and their m-lines exist in the peer's local description.
            if state.sfu_forwarder.has_subscriber(&track_key, peer_id) {
                continue;
            }
            tracing::info!(
                peer_id,
                publication_id = %publication.id,
                "SFU renegotiation: adding new egress track"
            );
            let output_codec = match publication.kind {
                sfu::TrackKind::Audio => Codec::Opus,
                sfu::TrackKind::Screen => codec,
            };
            let writer = peer
                .add_relay_track(&publication.publisher, &publication.id, output_codec)
                .await
                .map_err(|e| e.to_string())?;
            state.sfu_forwarder.subscribe_writer(
                track_key,
                publication.kind,
                peer_id.to_owned(),
                Arc::new(writer),
                64,
            );
        }

        let offer: SessionDescription = serde_json::from_str(sdp).map_err(|e| e.to_string())?;
        peer.set_remote_description(&offer)
            .await
            .map_err(|e| e.to_string())?;
        let answer = peer.create_answer().await.map_err(|e| e.to_string())?;
        peer.set_local_description(&answer)
            .await
            .map_err(|e| e.to_string())?;

        tx.send(SignalMessage::SfuAnswer {
            sdp: serde_json::to_string(&answer).map_err(|e| e.to_string())?,
        })
        .map_err(|e| e.to_string())?;
        return Ok(());
    }

    // ── Initial negotiation path ──────────────────────────────────────────────
    // No live peer exists yet. Tear down any stale state (pending ICE, forwarder
    // subscriptions) and create a fresh WebRTC peer connection.
    tracing::info!(
        peer_id,
        room_id,
        "SFU initial negotiation: creating new peer"
    );
    close_sfu_peer(state, room_id, peer_id).await;

    let outbound = tx.clone();
    let forwarder = state.sfu_forwarder.clone();
    let sources = state.sfu_sources.clone();
    let source_room = room_id.to_owned();
    let source_publisher = peer_id.to_owned();
    let events = Arc::new(move |event: PeerEvent| match event {
        PeerEvent::IceCandidate(candidate) => {
            let _ = outbound.send(SignalMessage::SfuIceCandidate {
                candidate: serde_json::to_string(&candidate).unwrap_or_default(),
            });
        }
        PeerEvent::TrackReceived { track } => attach_relay_source(
            forwarder.clone(),
            sources.clone(),
            source_room.clone(),
            source_publisher.clone(),
            track,
        ),
        _ => {}
    });
    let turn = state.turn.issue(now_unix());
    let peer_config = PeerConfig {
        ice_servers: if turn.urls.is_empty() {
            Vec::new()
        } else {
            vec![IceServer {
                urls: turn.urls,
                username: turn.username,
                credential: turn.credential,
            }]
        },
    };
    let peer = Arc::new(
        WebRtcPeer::new(peer_config, codec, events)
            .await
            .map_err(|e| e.to_string())?,
    );
    let publications: Vec<sfu::PublishedTrack> = {
        let rooms = state.sfu_rooms.lock().unwrap();
        let Some(room) = rooms.get(room_id) else {
            return Err("SFU room not found".into());
        };
        let subscribed = room.subscriptions_for(peer_id).tracks;
        room.publications()
            .filter(|publication| subscribed.contains(&publication.id))
            .cloned()
            .collect()
    };
    for publication in publications {
        let output_codec = match publication.kind {
            sfu::TrackKind::Audio => Codec::Opus,
            sfu::TrackKind::Screen => codec,
        };
        let writer = peer
            .add_relay_track(&publication.publisher, &publication.id, output_codec)
            .await
            .map_err(|e| e.to_string())?;
        state.sfu_forwarder.subscribe_writer(
            sfu_track_key(room_id, &publication.id),
            publication.kind,
            peer_id.to_owned(),
            Arc::new(writer),
            64,
        );
    }
    let offer: SessionDescription = serde_json::from_str(sdp).map_err(|e| e.to_string())?;
    peer.set_remote_description(&offer)
        .await
        .map_err(|e| e.to_string())?;
    let answer = peer.create_answer().await.map_err(|e| e.to_string())?;
    peer.set_local_description(&answer)
        .await
        .map_err(|e| e.to_string())?;
    state.sfu_peers.lock().unwrap().insert(key, peer);
    let pending = state
        .sfu_pending_ice
        .lock()
        .unwrap()
        .remove(&sfu_peer_key(room_id, peer_id))
        .unwrap_or_default();
    if !pending.is_empty() {
        let peer = state
            .sfu_peers
            .lock()
            .unwrap()
            .get(&sfu_peer_key(room_id, peer_id))
            .cloned();
        if let Some(peer) = peer {
            for candidate in pending {
                if let Err(error) = peer.add_ice_candidate(candidate).await {
                    tracing::debug!("buffered SFU ICE candidate rejected: {error}");
                }
            }
        }
    }
    if let Some(room) = state.sfu_rooms.lock().unwrap().get(room_id) {
        notify_subscriptions(state, room_id, room);
    }
    tx.send(SignalMessage::SfuAnswer {
        sdp: serde_json::to_string(&answer).map_err(|e| e.to_string())?,
    })
    .map_err(|e| e.to_string())?;
    Ok(())
}

async fn handle_sfu_candidate(state: &AppState, room_id: &str, peer_id: &str, json: &str) {
    let key = sfu_peer_key(room_id, peer_id);
    let Ok(candidate) = serde_json::from_str::<IceCandidate>(json) else {
        tracing::warn!("invalid SFU ICE candidate from {peer_id}");
        return;
    };
    let peer = state.sfu_peers.lock().unwrap().get(&key).cloned();
    if let Some(peer) = peer {
        if peer.add_ice_candidate(candidate.clone()).await.is_ok() {
            return;
        }
    }
    let mut pending = state.sfu_pending_ice.lock().unwrap();
    let queue = pending.entry(key).or_default();
    if queue.len() < 128 {
        queue.push(candidate);
    } else {
        tracing::warn!("dropping excess pending SFU ICE candidate from {peer_id}");
    }
}

async fn close_sfu_peer(state: &AppState, room_id: &str, peer_id: &str) {
    let key = sfu_peer_key(room_id, peer_id);
    state.sfu_pending_ice.lock().unwrap().remove(&key);
    state.sfu_forwarder.remove_viewer(peer_id);
    let source_prefix = sfu_source_prefix(room_id, peer_id);
    let source_keys: Vec<String> = state
        .sfu_sources
        .lock()
        .unwrap()
        .keys()
        .filter(|source_key| source_key.starts_with(&source_prefix))
        .cloned()
        .collect();
    for source_key in source_keys {
        if let Some(task) = state.sfu_sources.lock().unwrap().remove(&source_key) {
            task.abort();
        }
    }
    let peer = state.sfu_peers.lock().unwrap().remove(&key);
    if let Some(peer) = peer {
        let _ = peer.close().await;
    }
}

async fn close_room_sfu_peers(state: &AppState, room_id: &str) {
    let prefix = format!("{room_id}:");
    let keys: Vec<String> = state
        .sfu_peers
        .lock()
        .unwrap()
        .keys()
        .filter(|key| key.starts_with(&prefix))
        .cloned()
        .collect();
    for key in keys {
        let peer_id = key.trim_start_matches(&prefix).to_owned();
        close_sfu_peer(state, room_id, &peer_id).await;
    }
}

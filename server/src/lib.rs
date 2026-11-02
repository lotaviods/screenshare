//! `sharescreen-server`: the signaling server.
//!
//! An axum + WebSocket server that manages rooms and relays SDP/ICE between
//! peers. It never proxies media; WebRTC connects peers directly (or via
//! TURN). See `ARCHITECTURE.md`.

pub mod rooms;
pub mod turn;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::response::Response;
use axum::routing::get;
use axum::{Json, Router};
use sharescreen_core::{PeerInfo, SignalMessage};
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
    let info = PeerInfo { id: id.clone(), name };
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
                                        for p in &existing {
                                            let _ = state.rooms.send_to(
                                                &rid,
                                                &p.id,
                                                SignalMessage::PeerJoined { peer: info.clone() },
                                            );
                                        }
                                        let _ = tx.send(SignalMessage::Joined {
                                            room_id: rid,
                                            peers: existing,
                                        });
                                    }
                                    Err(e) => {
                                        let _ = tx.send(SignalMessage::Error {
                                            message: e.to_string(),
                                        });
                                    }
                                }
                            }
                            SignalMessage::LeaveRoom => {                                if let Some(rid) = room_id.take() {
                                    if let Some(remaining) = state.rooms.leave(&rid, &id) {
                                        for p in remaining {
                                            let _ = state.rooms.send_to(
                                                &rid,
                                                &p.id,
                                                SignalMessage::PeerLeft { id: id.clone() },
                                            );
                                        }
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
        if let Some(remaining) = state.rooms.leave(&rid, &id) {
            for p in remaining {
                let _ = state.rooms.send_to(
                    &rid,
                    &p.id,
                    SignalMessage::PeerLeft { id: id.clone() },
                );
            }
        }
    }
}

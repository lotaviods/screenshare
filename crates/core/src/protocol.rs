//! Signaling protocol messages shared by the desktop client and the signaling
//! server. These are JSON messages over a WebSocket; the server relays SDP and
//! ICE blobs verbatim and never touches media.

use serde::{Deserialize, Serialize};

/// Identifying info for a peer in a room.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerInfo {
    pub id: String,
    pub name: String,
}

/// Media/peer payload relayed verbatim between two peers. `sdp` and `candidate`
/// carry JSON-serialized SDP descriptions and ICE candidates; the WebRTC layer
/// owns (de)serialization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PeerPayload {
    Offer { sdp: String },
    Answer { sdp: String },
    IceCandidate { candidate: String },
    /// Whether the sender is publishing microphone audio and/or a screen.
    /// The media itself rides the peer connection; this tells the UI whose
    /// screen to render.
    MediaState { mic_on: bool, screen_on: bool },
}

/// A signaling message exchanged over the WebSocket, both directions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SignalMessage {
    // --- client → server ---
    CreateRoom,
    JoinRoom { room_id: String },
    LeaveRoom,
    /// Close the current room for everyone in it (creator only).
    DeleteRoom,
    /// Ask the server for time-limited TURN credentials.
    GetTurnCredentials,
    ToPeer { to: String, payload: PeerPayload },

    // --- server → client ---
    RoomCreated { room_id: String },
    Joined { room_id: String, peers: Vec<PeerInfo> },
    /// The room was closed by its creator; everyone was removed from it.
    RoomDeleted { room_id: String },
    PeerJoined { peer: PeerInfo },
    PeerLeft { id: String },
    FromPeer { from: String, payload: PeerPayload },
    /// Time-limited TURN credentials (coturn REST scheme).
    TurnCredentials {
        urls: Vec<String>,
        username: String,
        credential: String,
    },
    Error { message: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_json() {
        let msgs = [
            SignalMessage::CreateRoom,
            SignalMessage::DeleteRoom,
            SignalMessage::RoomDeleted {
                room_id: "k3f9xz".into(),
            },
            SignalMessage::JoinRoom {
                room_id: "abc123".into(),
            },
            SignalMessage::ToPeer {
                to: "peer-1".into(),
                payload: PeerPayload::Offer {
                    sdp: r#"{"type":"offer","sdp":"v=0..."}"#.into(),
                },
            },
            SignalMessage::RoomCreated {
                room_id: "abc123".into(),
            },
            SignalMessage::Joined {
                room_id: "abc123".into(),
                peers: vec![PeerInfo {
                    id: "peer-2".into(),
                    name: "Alice".into(),
                }],
            },
            SignalMessage::PeerLeft { id: "peer-2".into() },
            SignalMessage::FromPeer {
                from: "peer-2".into(),
                payload: PeerPayload::IceCandidate {
                    candidate: r#"{"candidate":"candidate:1"}"#.into(),
                },
            },
            SignalMessage::ToPeer {
                to: "peer-2".into(),
                payload: PeerPayload::MediaState {
                    mic_on: true,
                    screen_on: false,
                },
            },
        ];

        for msg in msgs {
            let json = serde_json::to_string(&msg).unwrap();
            let back: SignalMessage = serde_json::from_str(&json).unwrap();
            assert_eq!(msg, back);
        }
    }

    #[test]
    fn tags_are_snake_case() {
        let json = serde_json::to_string(&SignalMessage::PeerJoined {
            peer: PeerInfo {
                id: "p".into(),
                name: "n".into(),
            },
        })
        .unwrap();
        assert!(json.contains("\"type\":\"peer_joined\""));
        assert!(json.contains("\"kind\":\"ice_candidate\"") == false);
    }
}

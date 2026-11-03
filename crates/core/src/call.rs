use serde::{Deserialize, Serialize};

/// Connection state of a peer connection or an individual participant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionState {
    New,
    Connecting,
    Connected,
    Disconnected,
    Failed,
    Closed,
}

/// A participant in the current call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Participant {
    pub id: String,
    pub name: String,
    pub mic_on: bool,
    pub screen_on: bool,
    pub connection: ConnectionState,
}

/// Events emitted by the Rust core and forwarded to the frontend over Tauri.
///
/// The `type` tag lets the frontend discriminate without knowing the payload
/// shape ahead of time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CallEvent {
    RoomCreated {
        room_id: String,
    },
    RoomJoined {
        room_id: String,
    },
    /// The room was closed by its creator and this peer was removed from it.
    RoomDeleted {
        room_id: String,
    },
    ParticipantJoined {
        participant: Participant,
    },
    ParticipantLeft {
        id: String,
    },
    /// The full state of a participant changed (mic, screen, connection).
    ParticipantUpdated {
        participant: Participant,
    },
    ConnectionStateChanged {
        participant_id: String,
        state: ConnectionState,
    },
    Error {
        message: String,
    },
}

//! In-memory room management for the signaling server.
//!
//! Pure state + mpsc senders; the axum handler drives this. Kept independent of
//! axum so it can be unit-tested directly.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use sharescreen_core::{PeerInfo, PeerPayload, SignalMessage};
use tokio::sync::mpsc;

/// Sends messages to a connected peer.
pub type PeerSender = mpsc::UnboundedSender<SignalMessage>;

#[derive(Debug, thiserror::Error)]
pub enum RoomError {
    #[error("room not found")]
    NotFound,
    #[error("peer not in room")]
    PeerNotFound,
}

#[derive(Default)]
pub struct RoomManager {
    next_id: AtomicU64,
    rooms: Mutex<HashMap<String, Room>>,
}

#[derive(Default)]
struct Room {
    peers: HashMap<String, Peer>,
}

struct Peer {
    info: PeerInfo,
    tx: PeerSender,
}

impl RoomManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a new empty room and return its id.
    pub fn create_room(&self) -> String {
        let mut rooms = self.rooms.lock().unwrap();
        loop {
            let id = short_code(self.next_id.fetch_add(1, Ordering::Relaxed), nanos());
            if rooms.contains_key(&id) {
                continue;
            }
            rooms.insert(id.clone(), Room::default());
            return id;
        }
    }

    /// Remove a room and return its peers so the caller can notify them.
    /// Any member may delete the room.
    pub fn delete(&self, room_id: &str) -> Option<Vec<(PeerInfo, PeerSender)>> {
        let room = self.rooms.lock().unwrap().remove(room_id)?;
        Some(
            room.peers
                .into_values()
                .map(|p| (p.info, p.tx))
                .collect(),
        )
    }

    /// Add a peer to a room, returning the peers already present.
    pub fn join(
        &self,
        room_id: &str,
        info: PeerInfo,
        tx: PeerSender,
    ) -> Result<Vec<PeerInfo>, RoomError> {
        let mut rooms = self.rooms.lock().unwrap();
        let room = rooms.get_mut(room_id).ok_or(RoomError::NotFound)?;
        let existing = room.peers.values().map(|p| p.info.clone()).collect();
        room.peers.insert(info.id.clone(), Peer { info, tx });
        Ok(existing)
    }

    /// Remove a peer from a room, returning the remaining peers' info.
    pub fn leave(&self, room_id: &str, peer_id: &str) -> Option<Vec<PeerInfo>> {
        let mut rooms = self.rooms.lock().unwrap();
        let room = rooms.get_mut(room_id)?;
        room.peers.remove(peer_id);
        Some(room.peers.values().map(|p| p.info.clone()).collect())
    }

    /// Relay a payload from `from` to `to` within a room.
    pub fn relay(
        &self,
        room_id: &str,
        from: &str,
        to: &str,
        payload: PeerPayload,
    ) -> Result<(), RoomError> {
        let rooms = self.rooms.lock().unwrap();
        let room = rooms.get(room_id).ok_or(RoomError::NotFound)?;
        if !room.peers.contains_key(from) {
            return Err(RoomError::PeerNotFound);
        }
        let peer = room.peers.get(to).ok_or(RoomError::PeerNotFound)?;
        let _ = peer.tx.send(SignalMessage::FromPeer {
            from: from.to_string(),
            payload,
        });
        Ok(())
    }

    /// Send a message to a specific peer in a room. Returns false if absent.
    pub fn send_to(&self, room_id: &str, peer_id: &str, msg: SignalMessage) -> bool {
        let rooms = self.rooms.lock().unwrap();
        match rooms.get(room_id).and_then(|r| r.peers.get(peer_id)) {
            Some(peer) => peer.tx.send(msg).is_ok(),
            None => false,
        }
    }
}

/// Length of a room code. Six base-36 characters ≈ 2.1 billion codes.
const CODE_LEN: usize = 6;

const ALPHABET: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";

/// A short room code: a hash of the creation counter and the wall clock,
/// encoded base-36. Collisions are resolved by the caller's retry loop.
fn short_code(counter: u64, nanos: u64) -> String {
    // FNV-1a over both inputs: cheap, no dependency, and spreads the
    // sequential counter across the whole code space.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in counter
        .to_le_bytes()
        .iter()
        .chain(nanos.to_le_bytes().iter())
    {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }

    let mut out = Vec::with_capacity(CODE_LEN);
    let mut n = hash;
    for _ in 0..CODE_LEN {
        out.push(ALPHABET[(n % 36) as usize]);
        n /= 36;
    }
    String::from_utf8(out).unwrap()
}

/// Nanoseconds since the unix epoch; the clock half of the code hash.
fn nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(id: &str) -> PeerInfo {
        PeerInfo {
            id: id.to_string(),
            name: id.to_uppercase(),
        }
    }

    #[test]
    fn create_room_returns_unique_ids() {
        let rm = RoomManager::new();
        let a = rm.create_room();
        let b = rm.create_room();
        assert_ne!(a, b);
    }

    #[test]
    fn room_ids_are_short_base36_codes() {
        let rm = RoomManager::new();
        let id = rm.create_room();
        assert_eq!(id.len(), CODE_LEN);
        assert!(id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()));
    }

    #[test]
    fn delete_removes_room_and_returns_peers() {
        let rm = RoomManager::new();
        let rid = rm.create_room();
        let (tx1, _rx1) = mpsc::unbounded_channel();
        let (tx2, _rx2) = mpsc::unbounded_channel();
        rm.join(&rid, peer("a"), tx1).unwrap();
        rm.join(&rid, peer("b"), tx2).unwrap();

        let peers = rm.delete(&rid).unwrap();
        let mut ids: Vec<_> = peers.iter().map(|(p, _)| p.id.clone()).collect();
        ids.sort();
        assert_eq!(ids, vec!["a", "b"]);
        assert!(matches!(
            rm.join(&rid, peer("c"), mpsc::unbounded_channel().0),
            Err(RoomError::NotFound)
        ));
    }

    #[test]
    fn any_member_can_delete() {
        let rm = RoomManager::new();
        let rid = rm.create_room();
        let (tx2, _rx2) = mpsc::unbounded_channel();
        rm.join(&rid, peer("b"), tx2).unwrap();
        assert!(rm.delete(&rid).is_some());
    }

    #[test]
    fn deleting_an_unknown_room_returns_none() {
        assert!(RoomManager::new().delete("nope").is_none());
    }

    #[test]
    fn join_returns_existing_peers() {
        let rm = RoomManager::new();
        let rid = rm.create_room();
        let (tx1, _rx1) = mpsc::unbounded_channel();
        let (tx2, _rx2) = mpsc::unbounded_channel();

        assert!(rm.join(&rid, peer("a"), tx1).unwrap().is_empty());
        let existing = rm.join(&rid, peer("b"), tx2).unwrap();
        assert_eq!(existing, vec![peer("a")]);
    }

    #[test]
    fn relay_delivers_from_peer_to_target() {
        let rm = RoomManager::new();
        let rid = rm.create_room();
        let (_tx1, mut rx1) = mpsc::unbounded_channel::<SignalMessage>();
        let (tx2, mut rx2) = mpsc::unbounded_channel::<SignalMessage>();
        rm.join(&rid, peer("a"), _tx1).unwrap();
        rm.join(&rid, peer("b"), tx2).unwrap();

        rm.relay(&rid, "a", "b", PeerPayload::Offer { sdp: "{}".into() })
            .unwrap();

        match rx2.try_recv().unwrap() {
            SignalMessage::FromPeer { from, payload } => {
                assert_eq!(from, "a");
                assert_eq!(payload, PeerPayload::Offer { sdp: "{}".into() });
            }
            other => panic!("unexpected message: {other:?}"),
        }
        assert!(rx1.try_recv().is_err());
    }

    #[test]
    fn relay_to_unknown_peer_errors() {
        let rm = RoomManager::new();
        let rid = rm.create_room();
        let (tx1, _rx1) = mpsc::unbounded_channel();
        rm.join(&rid, peer("a"), tx1).unwrap();
        assert!(matches!(
            rm.relay(&rid, "a", "nobody", PeerPayload::Answer { sdp: "{}".into() }),
            Err(RoomError::PeerNotFound)
        ));
    }

    #[test]
    fn leave_removes_peer_and_returns_remaining() {
        let rm = RoomManager::new();
        let rid = rm.create_room();
        let (tx1, _rx1) = mpsc::unbounded_channel();
        let (tx2, _rx2) = mpsc::unbounded_channel();
        rm.join(&rid, peer("a"), tx1).unwrap();
        rm.join(&rid, peer("b"), tx2).unwrap();

        let remaining = rm.leave(&rid, "a").unwrap();
        assert_eq!(remaining, vec![peer("b")]);
    }
}

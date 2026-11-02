//! `sharescreen-network`: turns user configuration into network endpoints.
//!
//! Everything here is pure: the same `NetworkConfig` always produces the same
//! signaling URL and ICE server list, so the mapping is unit-testable without
//! a network.

use sharescreen_config::NetworkConfig;
use sharescreen_webrtc::{IceServer, PeerConfig};

/// WebSocket URL of the signaling endpoint (`ws://host:port/ws`).
pub fn signaling_url(config: &NetworkConfig) -> String {
    let scheme = if config.signaling_secure { "wss" } else { "ws" };
    format!(
        "{scheme}://{}:{}/ws",
        config.signaling_host, config.signaling_port
    )
}

/// Signaling URL including the peer identity query parameters.
pub fn signaling_url_with_identity(
    config: &NetworkConfig,
    id: &str,
    name: &str,
) -> String {
    format!(
        "{}?id={}&name={}",
        signaling_url(config),
        percent_encode(id),
        percent_encode(name)
    )
}

/// ICE servers (STUN first, then TURN) from the user configuration.
pub fn ice_servers(config: &NetworkConfig) -> Vec<IceServer> {
    let mut servers = Vec::new();
    for stun in &config.stun_servers {
        servers.push(IceServer {
            urls: vec![stun.url.clone()],
            username: String::new(),
            credential: String::new(),
        });
    }
    for turn in &config.turn_servers {
        servers.push(IceServer {
            urls: vec![turn.url.clone()],
            username: turn.username.clone().unwrap_or_default(),
            credential: turn.credential.clone().unwrap_or_default(),
        });
    }
    servers
}

/// Peer connection configuration used for every mesh peer.
pub fn peer_config(config: &NetworkConfig) -> PeerConfig {
    PeerConfig {
        ice_servers: ice_servers(config),
    }
}

/// A short unique id for this client instance.
pub fn generate_peer_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    format!("{:x}{:x}{:x}", nanos, std::process::id(), seq)
}

/// Percent-encode a query-parameter value (unreserved characters only).
fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use sharescreen_config::{StunServer, TurnServer};

    fn config() -> NetworkConfig {
        NetworkConfig {
            signaling_host: "example.com".into(),
            signaling_port: 8443,
            signaling_secure: true,
            stun_servers: vec![StunServer {
                url: "stun:stun.example.com:3478".into(),
            }],
            turn_servers: vec![TurnServer {
                url: "turn:turn.example.com:3478".into(),
                username: Some("user".into()),
                credential: Some("pass".into()),
            }],
            connection_mode: Default::default(),
        }
    }

    #[test]
    fn signaling_url_uses_scheme_and_port() {
        let mut c = config();
        assert_eq!(signaling_url(&c), "wss://example.com:8443/ws");
        c.signaling_secure = false;
        assert_eq!(signaling_url(&c), "ws://example.com:8443/ws");
    }

    #[test]
    fn signaling_url_encodes_identity() {
        let url = signaling_url_with_identity(&config(), "abc", "A B");
        assert_eq!(url, "wss://example.com:8443/ws?id=abc&name=A%20B");
    }

    #[test]
    fn ice_servers_map_stun_and_turn() {
        let servers = ice_servers(&config());
        assert_eq!(servers.len(), 2);
        assert_eq!(servers[0].urls, vec!["stun:stun.example.com:3478"]);
        assert!(servers[0].username.is_empty());
        assert_eq!(servers[1].urls, vec!["turn:turn.example.com:3478"]);
        assert_eq!(servers[1].username, "user");
        assert_eq!(servers[1].credential, "pass");
    }

    #[test]
    fn peer_ids_are_unique() {
        assert_ne!(generate_peer_id(), generate_peer_id());
    }
}

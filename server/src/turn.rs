//! Ephemeral TURN credentials (coturn's REST/shared-secret scheme).
//!
//! The signaling server never hands out a long-lived TURN secret. It issues
//! time-limited credentials on request: the username is `<expiry>:<user>` and
//! the credential is `base64(HMAC-SHA1(secret, username))`. coturn verifies
//! them with the same shared secret (`use-auth-secret`).

use base64::Engine;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha1::Sha1;

/// TURN server configuration and shared secret.
#[derive(Debug, Clone, Default)]
pub struct TurnConfig {
    /// TURN URLs (e.g. `turn:turn.example.com:3478?transport=udp`).
    pub urls: Vec<String>,
    /// Shared secret configured in coturn (`static-auth-secret`).
    pub secret: String,
    /// Lifetime of an issued credential, in seconds.
    pub ttl_secs: u64,
    /// Identifier embedded in the credential username.
    pub user: String,
}

/// A time-limited TURN credential.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnCredential {
    pub urls: Vec<String>,
    pub username: String,
    pub credential: String,
}

impl TurnConfig {
    /// Whether TURN credentials can be issued at all.
    pub fn is_enabled(&self) -> bool {
        !self.urls.is_empty() && !self.secret.is_empty()
    }

    /// Issue a credential valid for `ttl_secs` from `now` (unix seconds).
    pub fn issue(&self, now: u64) -> TurnCredential {
        let expiry = now + self.ttl_secs;
        let username = format!("{expiry}:{}", self.user);
        let credential = self.sign(&username);
        TurnCredential {
            urls: self.urls.clone(),
            username,
            credential,
        }
    }

    /// `base64(HMAC-SHA1(secret, username))`.
    fn sign(&self, username: &str) -> String {
        type HmacSha1 = Hmac<Sha1>;
        let mut mac =
            HmacSha1::new_from_slice(self.secret.as_bytes()).expect("HMAC accepts any key length");
        mac.update(username.as_bytes());
        base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes())
    }
}

/// Read the TURN configuration from the environment.
pub fn from_env() -> TurnConfig {
    let urls = std::env::var("TURN_URLS")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let secret = std::env::var("TURN_SECRET").unwrap_or_default();
    let ttl_secs = std::env::var("TURN_TTL")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3600);
    let user = std::env::var("TURN_USER").unwrap_or_else(|_| "sharescreen".to_string());
    TurnConfig {
        urls,
        secret,
        ttl_secs,
        user,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> TurnConfig {
        TurnConfig {
            urls: vec!["turn:turn.example.com:3478".to_string()],
            secret: "secret".to_string(),
            ttl_secs: 3600,
            user: "user".to_string(),
        }
    }

    #[test]
    fn credential_matches_the_coturn_rest_scheme() {
        let cred = config().issue(1000);
        assert_eq!(cred.username, "4600:user");
        // base64(HMAC-SHA1("secret", "4600:user")) — the signature covers the
        // whole username, expiry included.
        assert_eq!(cred.credential, "jnxw99dHpQLGP+4t6zslYayQ7+I=");
        assert_eq!(cred.urls, vec!["turn:turn.example.com:3478"]);
    }

    #[test]
    fn expiry_follows_the_ttl() {
        let cred = config().issue(1_000_000);
        assert!(cred.username.starts_with("1003600:"));
    }

    #[test]
    fn disabled_without_a_secret() {
        let mut c = config();
        c.secret = String::new();
        assert!(!c.is_enabled());
        c.secret = "secret".to_string();
        c.urls.clear();
        assert!(!c.is_enabled());
        assert!(c.is_enabled() == false || c.urls.is_empty());
    }
}

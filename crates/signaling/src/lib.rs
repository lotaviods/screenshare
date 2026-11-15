//! `sharescreen-signaling`: client-side WebSocket signaling.
//!
//! [`SignalingClient::connect`] opens a WebSocket to the signaling server,
//! spawns reader/writer tasks, and exposes an mpsc of incoming
//! [`Incoming`] events plus a [`SignalingClient::send`] for outbound messages.
//!
//! The client does not know about SDP or ICE: it relays [`SignalMessage`]
//! values; the WebRTC layer serializes/deserializes the media blobs.

use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;

pub use sharescreen_core::{PeerInfo, PeerPayload, SignalMessage};

/// How long to wait for the WebSocket handshake before reporting an error.
/// Without this a blocked port hangs the UI with no feedback.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// An event delivered to the caller from the connection.
#[derive(Debug)]
pub enum Incoming {
    Message(SignalMessage),
    Disconnected { reason: String },
}

/// A cloneable handle for sending signaling messages.
#[derive(Clone)]
pub struct SignalingSender {
    out: mpsc::UnboundedSender<Outbound>,
}

enum Outbound {
    Message(SignalMessage),
    Close,
}

impl SignalingSender {
    pub fn send(&self, msg: SignalMessage) -> Result<(), SignalingError> {
        self.out
            .send(Outbound::Message(msg))
            .map_err(|_| SignalingError::Disconnected)
    }

    /// Close the WebSocket even if other sender clones are still alive.
    pub fn close(&self) {
        let _ = self.out.send(Outbound::Close);
    }
}

/// An open signaling connection.
pub struct SignalingClient {
    sender: SignalingSender,
    pub incoming: mpsc::UnboundedReceiver<Incoming>,
}

#[derive(Debug, thiserror::Error)]
pub enum SignalingError {
    #[error("connect failed: {0}")]
    Connect(String),
    #[error("connection closed")]
    Disconnected,
}

impl SignalingClient {
    /// Connect to `url` (a `ws://` or `wss://` URL, optionally with query
    /// parameters such as `?id=..&name=..`).
    pub async fn connect(url: &str) -> Result<Self, SignalingError> {
        tracing::info!(%url, "connecting to signaling server");
        let (ws, _) = tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(url))
            .await
            .map_err(|_| {
                SignalingError::Connect(format!("timed out after {}s", CONNECT_TIMEOUT.as_secs()))
            })?
            .map_err(|e| SignalingError::Connect(e.to_string()))?;
        tracing::info!(%url, "signaling WebSocket connected");
        let (mut ws_sink, mut ws_stream) = ws.split();

        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Outbound>();
        let (incoming_tx, incoming_rx) = mpsc::unbounded_channel::<Incoming>();

        // Writer task: serialize outbound messages and write them to the socket.
        tokio::spawn(async move {
            while let Some(msg) = out_rx.recv().await {
                let Outbound::Message(msg) = msg else {
                    tracing::debug!("signaling writer: explicit close requested");
                    break;
                };
                let Ok(text) = serde_json::to_string(&msg) else {
                    tracing::warn!("signaling writer: failed to serialize outbound message");
                    continue;
                };
                if let Err(e) = ws_sink.send(WsMessage::Text(text.into())).await {
                    tracing::warn!("signaling writer: websocket send failed: {e}");
                    break;
                }
            }
            if let Err(e) = ws_sink.close().await {
                tracing::debug!("signaling writer: websocket close failed: {e}");
            }
        });

        // Reader task: parse inbound messages and forward them; signal
        // disconnection when the socket closes or errors.
        tokio::spawn(async move {
            let reason = loop {
                let Some(msg) = ws_stream.next().await else {
                    break "websocket stream ended without close frame".to_string();
                };
                match msg {
                    Ok(WsMessage::Text(text)) => {
                        match serde_json::from_str::<SignalMessage>(text.as_str()) {
                            Ok(m) => {
                                tracing::trace!("signaling reader: received {:?}", std::mem::discriminant(&m));
                                if incoming_tx.send(Incoming::Message(m)).is_err() {
                                    return;
                                }
                            }
                            Err(e) => tracing::warn!("signaling reader: ignoring malformed signaling message: {e}"),
                        }
                    }
                    Ok(WsMessage::Close(frame)) => {
                        break match frame {
                            Some(f) => format!(
                                "server closed websocket: code={} reason={:?}",
                                f.code, f.reason
                            ),
                            None => "server closed websocket without close frame details".to_string(),
                        };
                    }
                    Ok(WsMessage::Ping(_)) => tracing::trace!("signaling reader: ping"),
                    Ok(WsMessage::Pong(_)) => tracing::trace!("signaling reader: pong"),
                    Ok(WsMessage::Binary(_)) => tracing::warn!("signaling reader: unexpected binary frame"),
                    Ok(WsMessage::Frame(_)) => {}
                    Err(e) => {
                        tracing::warn!("signaling socket error: {e}");
                        break format!("websocket read error: {e}");
                    }
                }
            };
            tracing::warn!("signaling disconnected: {reason}");
            let _ = incoming_tx.send(Incoming::Disconnected { reason });
        });

        Ok(Self {
            sender: SignalingSender { out: out_tx },
            incoming: incoming_rx,
        })
    }

    /// A cloneable sender for this connection, usable from other tasks.
    pub fn sender(&self) -> SignalingSender {
        self.sender.clone()
    }

    /// Queue a message to be sent. Returns `Disconnected` if the writer task
    /// has already shut down.
    pub fn send(&self, msg: SignalMessage) -> Result<(), SignalingError> {
        self.sender.send(msg)
    }
}

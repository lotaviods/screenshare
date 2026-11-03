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

/// An event delivered to the caller from the connection.
#[derive(Debug)]
pub enum Incoming {
    Message(SignalMessage),
    Disconnected,
}

/// A cloneable handle for sending signaling messages.
#[derive(Clone)]
pub struct SignalingSender {
    out: mpsc::UnboundedSender<SignalMessage>,
}

impl SignalingSender {
    pub fn send(&self, msg: SignalMessage) -> Result<(), SignalingError> {
        self.out.send(msg).map_err(|_| SignalingError::Disconnected)
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
        let (ws, _) = tokio_tungstenite::connect_async(url)
            .await
            .map_err(|e| SignalingError::Connect(e.to_string()))?;
        let (mut ws_sink, mut ws_stream) = ws.split();

        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<SignalMessage>();
        let (incoming_tx, incoming_rx) = mpsc::unbounded_channel::<Incoming>();

        // Writer task: serialize outbound messages and write them to the socket.
        tokio::spawn(async move {
            while let Some(msg) = out_rx.recv().await {
                let Ok(text) = serde_json::to_string(&msg) else {
                    continue;
                };
                if ws_sink.send(WsMessage::Text(text.into())).await.is_err() {
                    break;
                }
            }
            let _ = ws_sink.close().await;
        });

        // Reader task: parse inbound messages and forward them; signal
        // disconnection when the socket closes or errors.
        tokio::spawn(async move {
            while let Some(msg) = ws_stream.next().await {
                match msg {
                    Ok(WsMessage::Text(text)) => {
                        match serde_json::from_str::<SignalMessage>(text.as_str()) {
                            Ok(m) => {
                                if incoming_tx.send(Incoming::Message(m)).is_err() {
                                    return;
                                }
                            }
                            Err(e) => tracing::warn!("ignoring malformed signaling message: {e}"),
                        }
                    }
                    Ok(WsMessage::Close(_)) => break,
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!("signaling socket error: {e}");
                        break;
                    }
                }
            }
            let _ = incoming_tx.send(Incoming::Disconnected);
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

//! Fans one local media stream out to every peer's track writer.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use futures_util::future::join_all;
use sharescreen_core::EncodedSample;
use sharescreen_webrtc::TrackWriter;

/// Holds the local track writer of every connected peer for one media kind.
#[derive(Default)]
pub struct MediaHub {
    writers: RwLock<HashMap<String, Arc<TrackWriter>>>,
}

impl MediaHub {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&self, peer_id: &str, writer: Arc<TrackWriter>) {
        self.writers
            .write()
            .unwrap()
            .insert(peer_id.to_string(), writer);
    }

    pub fn unregister(&self, peer_id: &str) {
        self.writers.write().unwrap().remove(peer_id);
    }

    /// Write one sample to every registered peer.
    ///
    /// Every peer write is polled concurrently, so one slow connection no
    /// longer serializes the others. No per-peer queue is added here: the
    /// call remains backpressured by the slowest current write instead of
    /// letting an unbounded backlog build up in the hub.
    pub async fn broadcast(&self, sample: &EncodedSample) {
        let writers: Vec<Arc<TrackWriter>> =
            self.writers.read().unwrap().values().cloned().collect();
        tracing::debug!("broadcasting sample to {} peer(s)", writers.len());
        let writes = writers.into_iter().map(|writer| {
            let sample = sample.clone();
            async move {
                if let Err(e) = writer.write(sample).await {
                    tracing::warn!("failed to write sample to peer: {e}");
                }
            }
        });
        join_all(writes).await;
    }
}

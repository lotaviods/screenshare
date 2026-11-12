//! Windows capture via GDI + FFmpeg H.264/VP8/VP9.
//!
//! `BitBlt` captures monitors and the whole desktop; `PrintWindow` captures a
//! single window. Frames are copied to a BGRA DIB and encoded by the native
//! FFmpeg encoder.

use async_trait::async_trait;
use tokio::sync::mpsc;

use sharescreen_core::{CaptureError, CaptureOptions, CaptureSource, EncodedSample, ScreenCapture};

use crate::native_windows;

/// Windows capture backend.
#[derive(Default)]
pub struct WindowsCapture {
    stream: Option<native_windows::Stream>,
}

impl WindowsCapture {
    /// Latest JPEG self-view frame of the running capture.
    pub fn latest_preview(&self) -> Option<Vec<u8>> {
        self.stream.as_ref().and_then(|s| s.latest_preview())
    }
}

#[async_trait]
impl ScreenCapture for WindowsCapture {
    fn list_sources(&self) -> Vec<CaptureSource> {
        native_windows::sources()
    }

    async fn start(
        &mut self,
        source: CaptureSource,
        options: CaptureOptions,
        sink: mpsc::Sender<EncodedSample>,
    ) -> Result<(), CaptureError> {
        self.stop().await.ok();

        if !native_windows::available(options.codec) {
            return Err(CaptureError::Unavailable(format!(
                "native Windows capture has no FFmpeg encoder for {:?}",
                options.codec
            )));
        }

        let stream = native_windows::spawn(source, &options, sink)?;
        self.stream = Some(stream);
        Ok(())
    }

    async fn stop(&mut self) -> Result<(), CaptureError> {
        if let Some(stream) = self.stream.take() {
            stream.stop();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sources_are_non_empty() {
        assert!(!native_windows::sources().is_empty());
    }
}

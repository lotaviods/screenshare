//! Native audio playback for remote call audio.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
pub(crate) use crate::pulse::PlaybackSink as Playback;
#[cfg(windows)]
pub(crate) use crate::wasapi::Playback;

/// Playback format, matching the Opus decode path.
pub(crate) const PLAYBACK_RATE: u32 = 48_000;
pub(crate) const PLAYBACK_CHANNELS: usize = 2;
/// One mix chunk: 20 ms of stereo S16LE at 48 kHz.
pub(crate) const PLAYBACK_CHUNK_MS: u64 = 20;
pub(crate) const PLAYBACK_CHUNK_FRAMES: usize = 960;
/// Start playback once this much audio is queued (~60 ms).
pub(crate) const PLAYBACK_TARGET_MS: u64 = 60;
pub(crate) const PLAYBACK_TARGET_FRAMES: usize = 2_880;
/// Upper bound on queued audio. Beyond this the oldest frames are dropped, so
/// a stalled consumer cannot grow latency without limit (~200 ms).
pub(crate) const PLAYBACK_MAX_MS: u64 = 200;
pub(crate) const PLAYBACK_MAX_FRAMES: usize = 9_600;
static PLAYBACK_WRITE_ERRORS: AtomicU64 = AtomicU64::new(0);
static PLAYBACK_UNDERRUNS: AtomicU64 = AtomicU64::new(0);
static PLAYBACK_BUFFER_DROPS: AtomicU64 = AtomicU64::new(0);

pub(crate) fn write_errors() -> u64 {
    PLAYBACK_WRITE_ERRORS.load(Ordering::Relaxed)
}

pub(crate) fn underruns() -> u64 {
    PLAYBACK_UNDERRUNS.load(Ordering::Relaxed)
}

pub(crate) fn buffer_drops() -> u64 {
    PLAYBACK_BUFFER_DROPS.load(Ordering::Relaxed)
}

/// Bytes of stereo S16LE for `ms` milliseconds.
pub(crate) fn playback_bytes_for_ms(ms: u64) -> u32 {
    let bytes =
        PLAYBACK_RATE as u64 * PLAYBACK_CHANNELS as u64 * std::mem::size_of::<i16>() as u64 * ms
            / 1_000;
    bytes.min(u32::MAX as u64) as u32
}

/// Bounded jitter buffer feeding the device in fixed-size chunks.
///
/// PCM is queued until [`target_frames`](Self::target_frames) are available,
/// then chunks are released in order. If the queue runs dry mid-chunk the
/// remainder is padded with silence and playback waits for a fresh prebuffer.
/// When a burst would exceed [`max_frames`](Self::max_frames), the oldest
/// frames are dropped instead of accumulating latency.
pub(crate) struct JitterBuffer {
    pending: VecDeque<i16>,
    channels: usize,
    chunk_frames: usize,
    target_frames: usize,
    max_frames: usize,
    playing: bool,
    underruns: u64,
}

impl JitterBuffer {
    pub(crate) fn new(
        channels: usize,
        chunk_frames: usize,
        target_frames: usize,
        max_frames: usize,
    ) -> Self {
        assert!(channels > 0, "jitter buffer needs at least one channel");
        Self {
            pending: VecDeque::new(),
            channels,
            chunk_frames,
            target_frames,
            max_frames: max_frames.max(target_frames).max(chunk_frames),
            playing: false,
            underruns: 0,
        }
    }

    /// Number of complete interleaved frames queued.
    pub(crate) fn buffered_frames(&self) -> usize {
        self.pending.len() / self.channels
    }

    fn set_target_frames(&mut self, frames: usize) {
        self.target_frames = frames.max(self.chunk_frames).min(self.max_frames);
    }

    /// Queue interleaved PCM, discarding the oldest frames on overflow.
    pub(crate) fn push(&mut self, samples: &[i16]) -> usize {
        self.pending.extend(samples.iter().copied());
        let max_samples = self.max_frames.saturating_mul(self.channels);
        if self.pending.len() > max_samples {
            // Drop whole frames so the interleaved channel order stays intact.
            let excess_frames = (self.pending.len() - max_samples).div_ceil(self.channels);
            let excess = (excess_frames * self.channels).min(self.pending.len());
            self.pending.drain(..excess);
            return excess_frames;
        }
        0
    }

    /// Release the next full chunk, or `None` while waiting for the prebuffer.
    ///
    /// A chunk that cannot be filled from the queue is padded with silence and
    /// resets the prebuffer requirement.
    pub(crate) fn next_chunk(&mut self) -> Option<Vec<i16>> {
        if self.chunk_frames == 0 {
            return None;
        }
        if !self.playing {
            if self.buffered_frames() < self.target_frames {
                return None;
            }
            self.playing = true;
        }

        let wanted = self.chunk_frames * self.channels;
        let take = self.pending.len().min(wanted);
        let mut chunk: Vec<i16> = self.pending.drain(..take).collect();
        if chunk.len() < wanted {
            chunk.resize(wanted, 0);
            self.playing = false;
            self.underruns += 1;
            // Recover with one packet instead of imposing the full initial
            // prebuffer again after a short network gap.
            self.target_frames = self.chunk_frames;
        }
        Some(chunk)
    }
}

/// A playback sink on the default output device with network-jitter buffering.
pub struct Output {
    sender: SyncSender<PlaybackCommand>,
    thread: Option<JoinHandle<()>>,
}

enum PlaybackCommand {
    Samples(Vec<i16>),
    Stop,
}

impl Output {
    /// Open the default output. `device` is reserved for the settings picker.
    pub fn open(_device: Option<String>) -> Result<Self, String> {
        const QUEUE: usize = 16;
        let (sender, receiver) = mpsc::sync_channel(QUEUE);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let thread = thread::Builder::new()
            .name("sharescreen-audio-playback".into())
            .spawn(move || {
                let mut inner = match Playback::open(None) {
                    Ok(inner) => {
                        let _ = ready_tx.send(Ok(()));
                        inner
                    }
                    Err(error) => {
                        let _ = ready_tx.send(Err(error));
                        return;
                    }
                };
                let mut buffer = JitterBuffer::new(
                    PLAYBACK_CHANNELS,
                    PLAYBACK_CHUNK_FRAMES,
                    PLAYBACK_TARGET_FRAMES,
                    PLAYBACK_MAX_FRAMES,
                );
                let cadence = Duration::from_millis(PLAYBACK_CHUNK_MS);
                let mut next_deadline = Instant::now();
                let mut last_packet = None;
                let mut jitter_ms = 0.0f64;
                let mut stopping = false;
                while !stopping {
                    let wait = next_deadline
                        .saturating_duration_since(Instant::now())
                        .min(Duration::from_millis(5));
                    match receiver.recv_timeout(wait) {
                        Ok(PlaybackCommand::Samples(samples)) => {
                            let now = Instant::now();
                            if let Some(previous) = last_packet {
                                let actual = now.duration_since(previous).as_secs_f64() * 1000.0;
                                let deviation = (actual - PLAYBACK_CHUNK_MS as f64).abs();
                                jitter_ms += (deviation - jitter_ms) / 16.0;
                                let target_ms = (40.0 + 4.0 * jitter_ms).clamp(40.0, 120.0);
                                buffer.set_target_frames(
                                    (target_ms * PLAYBACK_RATE as f64 / 1000.0) as usize,
                                );
                            }
                            last_packet = Some(now);
                            let dropped = buffer.push(&samples);
                            if dropped > 0 {
                                PLAYBACK_BUFFER_DROPS.fetch_add(dropped as u64, Ordering::Relaxed);
                            }
                        }
                        Ok(PlaybackCommand::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                            stopping = true
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }
                    let now = Instant::now();
                    if now >= next_deadline {
                        let underruns_before = buffer.underruns;
                        if let Some(chunk) = buffer.next_chunk() {
                            let underruns = buffer.underruns - underruns_before;
                            if underruns > 0 {
                                PLAYBACK_UNDERRUNS.fetch_add(underruns, Ordering::Relaxed);
                            }
                            if let Err(error) = inner.play(&chunk) {
                                let errors =
                                    PLAYBACK_WRITE_ERRORS.fetch_add(1, Ordering::Relaxed) + 1;
                                if errors == 1 || errors.is_multiple_of(100) {
                                    tracing::warn!(errors, %error, "audio playback write failed");
                                }
                            }
                            next_deadline += cadence;
                            if next_deadline + cadence < now {
                                next_deadline = now + cadence;
                            }
                        } else {
                            next_deadline = now + Duration::from_millis(5);
                        }
                    }
                }
                inner.stop();
            })
            .map_err(|e| format!("failed to spawn playback thread: {e}"))?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                sender,
                thread: Some(thread),
            }),
            Ok(Err(error)) => {
                let _ = thread.join();
                Err(error)
            }
            Err(_) => {
                let _ = thread.join();
                Err("playback thread stopped during initialization".into())
            }
        }
    }

    /// Queue interleaved 16-bit stereo PCM and write a due chunk.
    pub fn push(&mut self, samples: &[i16]) {
        if !samples.is_empty()
            && self
                .sender
                .send(PlaybackCommand::Samples(samples.to_vec()))
                .is_err()
        {
            let errors = PLAYBACK_WRITE_ERRORS.fetch_add(1, Ordering::Relaxed) + 1;
            if errors == 1 || errors.is_multiple_of(100) {
                tracing::error!(
                    errors,
                    "audio playback worker is unavailable; PCM was not queued"
                );
            }
        }
    }

    /// Write the next chunk if the 20 ms cadence is due.
    ///
    /// Call this from the decode loop even when no packet arrived, so an
    /// underrun becomes silence instead of a device underrun/click.
    pub fn tick(&mut self) {
        // The worker owns device pacing and services it during packet gaps.
    }

    /// Play interleaved 16-bit stereo PCM. Kept as a queueing alias for callers
    /// that used the old API; playback is paced by [`tick`](Self::tick).
    pub fn play(&mut self, samples: &[i16]) {
        self.push(samples);
    }

    pub fn stop(&mut self) {
        let _ = self.sender.send(PlaybackCommand::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::JitterBuffer;

    fn chunk(values: &[i16]) -> Vec<i16> {
        values.to_vec()
    }

    #[test]
    fn prebuffers_then_releases_chunks_in_order() {
        let mut buffer = JitterBuffer::new(1, 4, 8, 32);
        buffer.push(&[1, 2, 3, 4]);
        assert!(buffer.next_chunk().is_none(), "played before prebuffer");
        buffer.push(&[5, 6, 7, 8]);

        assert_eq!(buffer.next_chunk(), Some(chunk(&[1, 2, 3, 4])));
        assert_eq!(buffer.next_chunk(), Some(chunk(&[5, 6, 7, 8])));
        // The queue is now empty but playback had started: the next chunk is
        // silence, not an error.
        assert_eq!(buffer.next_chunk(), Some(chunk(&[0, 0, 0, 0])));
        assert!(buffer.next_chunk().is_none(), "silence looped forever");
    }

    #[test]
    fn excessive_backlog_drops_the_oldest_frames() {
        let mut buffer = JitterBuffer::new(1, 2, 2, 6);
        buffer.push(&[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(buffer.buffered_frames(), 6);
        assert_eq!(buffer.next_chunk(), Some(chunk(&[3, 4])));
        assert_eq!(buffer.next_chunk(), Some(chunk(&[5, 6])));
        assert_eq!(buffer.next_chunk(), Some(chunk(&[7, 8])));
    }

    #[test]
    fn underrun_pads_with_silence_and_reprebuffers() {
        let mut buffer = JitterBuffer::new(1, 4, 4, 16);
        buffer.push(&[1, 2, 3, 4]);
        assert_eq!(buffer.next_chunk(), Some(chunk(&[1, 2, 3, 4])));

        buffer.push(&[5, 6]);
        assert_eq!(
            buffer.next_chunk(),
            Some(chunk(&[5, 6, 0, 0])),
            "partial chunk must be padded with silence"
        );
        assert!(
            buffer.next_chunk().is_none(),
            "must wait for a fresh prebuffer after an underrun"
        );

        buffer.push(&[7, 8, 9, 10]);
        assert_eq!(buffer.next_chunk(), Some(chunk(&[7, 8, 9, 10])));
    }

    #[test]
    fn interleaved_channels_stay_aligned_when_dropping() {
        let mut buffer = JitterBuffer::new(2, 2, 2, 3);
        // Four stereo frames; the oldest one is dropped to keep three.
        buffer.push(&[1, -1, 2, -2, 3, -3, 4, -4]);
        assert_eq!(buffer.buffered_frames(), 3);
        assert_eq!(buffer.next_chunk(), Some(chunk(&[2, -2, 3, -3])));
        assert_eq!(buffer.next_chunk(), Some(chunk(&[4, -4, 0, 0])));
    }
}

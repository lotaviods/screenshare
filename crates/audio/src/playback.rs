//! Native audio playback for remote call audio.

use std::collections::VecDeque;
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
        }
    }

    /// Number of complete interleaved frames queued.
    pub(crate) fn buffered_frames(&self) -> usize {
        self.pending.len() / self.channels
    }

    /// Queue interleaved PCM, discarding the oldest frames on overflow.
    pub(crate) fn push(&mut self, samples: &[i16]) {
        self.pending.extend(samples.iter().copied());
        let max_samples = self.max_frames.saturating_mul(self.channels);
        if self.pending.len() > max_samples {
            // Drop whole frames so the interleaved channel order stays intact.
            let excess_frames = (self.pending.len() - max_samples).div_ceil(self.channels);
            let excess = (excess_frames * self.channels).min(self.pending.len());
            self.pending.drain(..excess);
        }
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
        }
        Some(chunk)
    }

    pub(crate) fn clear(&mut self) {
        self.pending.clear();
        self.playing = false;
    }
}

/// A playback sink on the default output device with network-jitter buffering.
pub struct Output {
    inner: Playback,
    buffer: JitterBuffer,
    next_chunk: Instant,
}

impl Output {
    /// Open the default output. `device` is reserved for the settings picker.
    pub fn open(_device: Option<String>) -> Result<Self, String> {
        Ok(Self {
            inner: Playback::open(None)?,
            buffer: JitterBuffer::new(
                PLAYBACK_CHANNELS,
                PLAYBACK_CHUNK_FRAMES,
                PLAYBACK_TARGET_FRAMES,
                PLAYBACK_MAX_FRAMES,
            ),
            next_chunk: Instant::now(),
        })
    }

    /// Queue interleaved 16-bit stereo PCM and write a due chunk.
    pub fn push(&mut self, samples: &[i16]) {
        self.buffer.push(samples);
        self.tick();
    }

    /// Write the next chunk if the 20 ms cadence is due.
    ///
    /// Call this from the decode loop even when no packet arrived, so an
    /// underrun becomes silence instead of a device underrun/click.
    pub fn tick(&mut self) {
        let now = Instant::now();
        if now < self.next_chunk {
            return;
        }
        match self.buffer.next_chunk() {
            Some(chunk) => {
                self.inner.play(&chunk);
                self.next_chunk = now + Duration::from_millis(PLAYBACK_CHUNK_MS);
            }
            None => {
                // Still prebuffering. Re-check soon without spinning.
                self.next_chunk = now + Duration::from_millis(5);
            }
        }
    }

    /// Play interleaved 16-bit stereo PCM. Kept as a queueing alias for callers
    /// that used the old API; playback is paced by [`tick`](Self::tick).
    pub fn play(&mut self, samples: &[i16]) {
        self.push(samples);
    }

    pub fn stop(&mut self) {
        self.buffer.clear();
        self.inner.stop();
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

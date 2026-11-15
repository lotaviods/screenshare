//! RTP depacketization for the codecs Sharescreen negotiates.
//!
//! The RTP header is 12 bytes. H.264 is reassembled into Annex-B access units
//! (single NAL, STAP-A, FU-A). VP8/VP9 and Opus payloads are passed through
//! as-is; the decoder consumes the payload.
//!
//! An access unit is one RTP timestamp, terminated by the RTP marker bit. The
//! unit is returned as soon as the marking packet arrives; a timestamp change
//! is kept as a fallback for senders that never set the marker. An access unit
//! that is only partially received stays buffered (its remaining packets may
//! still be in flight) and is abandoned on the next sequence gap, or dropped
//! by [`RtpDepacketizer::flush`] when the stream is torn down.
//!
//! [`RtpReorderBuffer`] sits in front of the depacketizer: the network (and
//! NACK retransmissions) may deliver packets out of order, and handing a
//! reordered packet straight to [`RtpDepacketizer`] either drops it or closes
//! an access unit early. Both cases corrupt the frame.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RtpKind {
    H264,
    Vp8,
    Vp9,
    Opus,
}

/// How long a sequence gap stays open waiting for a retransmission before the
/// missing packet is given up on. This is the added latency after packet loss;
/// it must stay above the retransmission round trip to actually repair a gap.
pub const REORDER_HOLD: Duration = Duration::from_millis(80);

/// Bound on buffered packets while a gap is open, so one lost packet cannot
/// make memory grow if the sender keeps producing.
const REORDER_MAX_PACKETS: usize = 128;

/// Reorders RTP packets by sequence number and tolerates NACK retransmissions.
///
/// Contiguous packets are returned immediately. When a packet is missing, the
/// following packets are held for [`REORDER_HOLD`] so a retransmission can fill
/// the hole; if it does not arrive, the gap is skipped and the depacketizer
/// sees the discontinuity (and discards the incomplete access unit). Packets
/// older than the next expected sequence number are dropped as duplicates.
pub struct RtpReorderBuffer {
    next: Option<u16>,
    pending: BTreeMap<u16, Vec<u8>>,
    hold_until: Option<Instant>,
}

impl Default for RtpReorderBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl RtpReorderBuffer {
    pub fn new() -> Self {
        Self {
            next: None,
            pending: BTreeMap::new(),
            hold_until: None,
        }
    }

    /// Forget buffered packets and resynchronize on the next input. Used when
    /// the decoder is restarted so stale packets cannot cross the boundary.
    pub fn reset(&mut self) {
        self.next = None;
        self.pending.clear();
        self.hold_until = None;
    }

    /// Whether a packet is still being held for a gap.
    pub fn is_waiting(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Insert one packet and return every packet that is ready, in sequence
    /// order.
    pub fn push(&mut self, packet: Vec<u8>, now: Instant) -> Vec<Vec<u8>> {
        let Some(sequence) = rtp_sequence(&packet) else {
            return Vec::new();
        };
        let mut ready = Vec::new();
        match self.next {
            None => {
                self.next = Some(sequence.wrapping_add(1));
                ready.push(packet);
                self.drain(&mut ready);
            }
            Some(next) => {
                let distance = sequence.wrapping_sub(next);
                if distance == 0 {
                    self.next = Some(next.wrapping_add(1));
                    ready.push(packet);
                    self.drain(&mut ready);
                } else if distance < 0x4000 {
                    // A gap: hold this and everything after it briefly.
                    self.pending.entry(sequence).or_insert(packet);
                    self.hold_until
                        .get_or_insert(now.checked_add(REORDER_HOLD).unwrap_or(now));
                    if self.pending.len() > REORDER_MAX_PACKETS {
                        self.skip_gap();
                        self.drain(&mut ready);
                    }
                } else if distance < 0x8000 {
                    // Too far ahead for a lost packet: treat it as a stream
                    // discontinuity and resynchronize instead of stalling.
                    self.pending.clear();
                    self.hold_until = None;
                    self.next = Some(sequence.wrapping_add(1));
                    ready.push(packet);
                    self.drain(&mut ready);
                }
                // Otherwise the packet is older than what was already
                // delivered: a duplicate or a retransmission that arrived too
                // late. Drop it.
            }
        }
        self.release(now, &mut ready);
        ready
    }

    /// Emit packets whose gap has waited long enough. Call periodically while
    /// no new packets arrive so a lost packet cannot stall the stream forever.
    pub fn release(&mut self, now: Instant, ready: &mut Vec<Vec<u8>>) {
        if self.pending.is_empty() {
            self.hold_until = None;
            return;
        }
        if self.hold_until.is_some_and(|deadline| now >= deadline) {
            self.skip_gap();
            self.drain(ready);
            if self.pending.is_empty() {
                self.hold_until = None;
            } else {
                self.hold_until = now.checked_add(REORDER_HOLD);
            }
        }
    }

    fn skip_gap(&mut self) {
        let Some((&sequence, _)) = self.pending.iter().next() else {
            return;
        };
        tracing::debug!(
            missing = sequence.wrapping_sub(self.next.unwrap_or(sequence)),
            "giving up on a missing RTP packet"
        );
        self.next = Some(sequence);
    }

    fn drain(&mut self, ready: &mut Vec<Vec<u8>>) {
        while let Some(next) = self.next {
            let Some(packet) = self.pending.remove(&next) else {
                break;
            };
            self.next = Some(next.wrapping_add(1));
            ready.push(packet);
        }
    }
}

fn rtp_sequence(packet: &[u8]) -> Option<u16> {
    if packet.len() < 4 {
        return None;
    }
    Some(u16::from_be_bytes([packet[2], packet[3]]))
}

/// A depacketizer that turns RTP packets into decoder-ready frames.
pub struct RtpDepacketizer {
    kind: RtpKind,
    h264: Vec<u8>,
    fu: Vec<u8>,
    in_fu: bool,
    last_ts: Option<u32>,
    last_seq: Option<u16>,
    discard_ts: Option<u32>,
}

const MAX_H264_ACCESS_UNIT_BYTES: usize = 32 * 1024 * 1024;

/// Strip VP8's payload descriptor, leaving the frame/partition bytes.
pub fn vp8_payload(payload: &[u8]) -> Option<&[u8]> {
    let descriptor = *payload.first()?;
    let mut offset = 1;
    if descriptor & 0x80 != 0 {
        let extension = *payload.get(offset)?;
        offset += 1;
        if extension & 0x80 != 0 {
            let picture_id = *payload.get(offset)?;
            offset += if picture_id & 0x80 != 0 { 2 } else { 1 };
        }
        if extension & 0x40 != 0 {
            offset += 1;
        }
        if extension & 0x30 != 0 {
            offset += 1;
        }
    }
    payload.get(offset..)
}

/// Strip VP9's payload descriptor, including layers and scalability metadata.
pub fn vp9_payload(payload: &[u8]) -> Option<&[u8]> {
    let descriptor = *payload.first()?;
    let mut offset = 1;
    if descriptor & 0x80 != 0 {
        let picture_id = *payload.get(offset)?;
        offset += if picture_id & 0x80 != 0 { 2 } else { 1 };
    }
    if descriptor & 0x20 != 0 {
        offset += 1;
        if descriptor & 0x10 == 0 {
            offset += 1;
        }
    }
    if descriptor & 0x50 == 0x50 {
        // Flexible-mode inter frame: up to three P_DIFF octets, N in the LSB.
        for _ in 0..3 {
            let difference = *payload.get(offset)?;
            offset += 1;
            if difference & 0x01 == 0 {
                break;
            }
        }
    }
    if descriptor & 0x02 != 0 {
        let scaling = *payload.get(offset)?;
        offset += 1;
        if scaling & 0x80 != 0 {
            let layers = *payload.get(offset)?;
            offset += 1;
            if layers & 0x10 != 0 {
                offset = offset.checked_add((usize::from((layers >> 5) & 0x07) + 1) * 4)?;
            }
            if layers & 0x08 != 0 {
                let frames = usize::from(*payload.get(offset)?);
                offset += 1;
                for _ in 0..frames {
                    let frame = *payload.get(offset)?;
                    offset += 1 + usize::from((frame >> 2) & 0x07);
                }
            }
        }
    }
    payload.get(offset..)
}

/// Return the payload after the fixed header, CSRCs, extensions and padding.
pub fn rtp_payload(packet: &[u8]) -> Option<&[u8]> {
    if packet.len() < 12 || packet[0] >> 6 != 2 {
        return None;
    }
    let mut offset = 12 + usize::from(packet[0] & 0x0f) * 4;
    if offset > packet.len() {
        return None;
    }
    if packet[0] & 0x10 != 0 {
        let extension = packet.get(offset..offset + 4)?;
        let words = usize::from(u16::from_be_bytes([extension[2], extension[3]]));
        offset = offset.checked_add(4 + words.checked_mul(4)?)?;
    }
    let end = if packet[0] & 0x20 != 0 {
        let padding = usize::from(*packet.last()?);
        if padding == 0 || padding > packet.len().saturating_sub(offset) {
            return None;
        }
        packet.len() - padding
    } else {
        packet.len()
    };
    packet.get(offset..end)
}

impl RtpDepacketizer {
    pub fn new(kind: RtpKind) -> Self {
        Self {
            kind,
            h264: Vec::new(),
            fu: Vec::new(),
            in_fu: false,
            last_ts: None,
            last_seq: None,
            discard_ts: None,
        }
    }

    /// Push one full RTP packet. Returns zero or more complete frames.
    pub fn push(&mut self, packet: &[u8]) -> Vec<Vec<u8>> {
        let Some(payload) = rtp_payload(packet) else {
            return Vec::new();
        };
        match self.kind {
            RtpKind::H264 => {
                let ts = u32::from_be_bytes([packet[4], packet[5], packet[6], packet[7]]);
                let seq = u16::from_be_bytes([packet[2], packet[3]]);
                let mut out = Vec::new();
                if self.discard_ts == Some(ts) {
                    self.last_seq = Some(seq);
                    return out;
                }
                if self.last_seq == Some(seq) {
                    return out;
                }
                if self
                    .last_seq
                    .is_some_and(|last| seq.wrapping_sub(last) > 0x8000)
                {
                    // Do not feed a late fragment into the newer access unit.
                    return out;
                }
                let sequence_gap = self
                    .last_seq
                    .is_some_and(|last| seq != last.wrapping_add(1));
                if sequence_gap {
                    self.h264.clear();
                    self.fu.clear();
                    self.in_fu = false;
                    // A gap inside an access unit invalidates that unit. If
                    // the timestamp changed, keep the new packet as the start
                    // of a fresh unit instead of dropping an extra frame.
                    self.discard_ts = (self.last_ts == Some(ts)).then_some(ts);
                    self.last_ts = Some(ts);
                    self.last_seq = Some(seq);
                    if self.discard_ts == Some(ts) {
                        return out;
                    }
                    self.push_h264(payload);
                    self.finish_h264(packet, &mut out);
                    return out;
                }
                self.last_seq = Some(seq);
                if let Some(last) = self.last_ts {
                    if last != ts {
                        self.discard_ts = None;
                        if self.in_fu {
                            self.h264.clear();
                            self.fu.clear();
                            self.in_fu = false;
                        } else if !self.h264.is_empty() {
                            out.push(std::mem::take(&mut self.h264));
                        }
                    }
                }
                self.last_ts = Some(ts);
                self.push_h264(payload);
                self.finish_h264(packet, &mut out);
                out
            }
            RtpKind::Vp8 => {
                vp8_payload(payload).map_or_else(Vec::new, |frame| vec![frame.to_vec()])
            }
            RtpKind::Vp9 => {
                vp9_payload(payload).map_or_else(Vec::new, |frame| vec![frame.to_vec()])
            }
            RtpKind::Opus => vec![payload.to_vec()],
        }
    }

    /// Drop the partially assembled access unit, if any. Call this when the
    /// stream is torn down, never between packets of a live stream.
    ///
    /// A unit without a marker bit is incomplete: its trailing packet is
    /// either still in flight or was lost. Emitting it would decode a
    /// truncated picture, and the rest of the same unit would then be emitted
    /// on its own.
    pub fn flush(&mut self) -> Vec<Vec<u8>> {
        if self.h264.is_empty() && !self.in_fu {
            return Vec::new();
        }
        tracing::debug!("dropping an H264 access unit that was never marked complete");
        self.h264.clear();
        self.fu.clear();
        self.in_fu = false;
        self.discard_ts = None;
        self.last_ts = None;
        Vec::new()
    }

    /// Emit the buffered access unit when this packet carries the RTP marker
    /// bit. A packet that ends mid-FU cannot complete a unit, so it is ignored.
    fn finish_h264(&mut self, packet: &[u8], out: &mut Vec<Vec<u8>>) {
        if packet.len() > 1 && packet[1] & 0x80 != 0 && !self.in_fu && !self.h264.is_empty() {
            self.discard_ts = None;
            out.push(std::mem::take(&mut self.h264));
        }
    }

    fn push_h264(&mut self, payload: &[u8]) {
        if payload.is_empty() {
            return;
        }
        let nal_type = payload[0] & 0x1f;
        if nal_type == 28 {
            // FU-A: start/middle/end fragments of one NAL unit.
            if payload.len() < 2 {
                return;
            }
            let fu_header = payload[1];
            let start = fu_header & 0x80 != 0;
            let end = fu_header & 0x40 != 0;
            let nal = fu_header & 0x1f;
            if start {
                self.fu.clear();
                self.fu.extend_from_slice(&[0, 0, 0, 1]);
                self.fu.push((payload[0] & 0xe0) | nal);
                self.in_fu = true;
            }
            if self.in_fu {
                if self.fu.len().saturating_add(payload.len()) > MAX_H264_ACCESS_UNIT_BYTES {
                    self.fu.clear();
                    self.h264.clear();
                    self.in_fu = false;
                    self.discard_ts = self.last_ts;
                    return;
                }
                self.fu.extend_from_slice(&payload[2..]);
            }
            if end {
                self.in_fu = false;
                if self.h264.len().saturating_add(self.fu.len()) <= MAX_H264_ACCESS_UNIT_BYTES {
                    self.h264.extend_from_slice(&self.fu);
                } else {
                    self.h264.clear();
                    self.discard_ts = self.last_ts;
                }
                self.fu.clear();
            }
            return;
        }
        if nal_type == 24 {
            // STAP-A: one or more NAL units, each with a 2-byte length.
            // Validate the whole aggregation before committing any of its NALs.
            let mut valid = 1;
            while valid < payload.len() {
                if valid + 2 > payload.len() {
                    return;
                }
                let size = u16::from_be_bytes([payload[valid], payload[valid + 1]]) as usize;
                valid += 2;
                if size == 0 || valid.saturating_add(size) > payload.len() {
                    return;
                }
                valid += size;
            }
            let mut i = 1;
            while i + 2 <= payload.len() {
                let size = u16::from_be_bytes([payload[i], payload[i + 1]]) as usize;
                i += 2;
                if i + size > payload.len() {
                    break;
                }
                if self.h264.len().saturating_add(size).saturating_add(4)
                    > MAX_H264_ACCESS_UNIT_BYTES
                {
                    self.h264.clear();
                    self.discard_ts = self.last_ts;
                    break;
                }
                self.h264.extend_from_slice(&[0, 0, 0, 1]);
                self.h264.extend_from_slice(&payload[i..i + size]);
                i += size;
            }
            return;
        }
        if (1..=23).contains(&nal_type) {
            if self
                .h264
                .len()
                .saturating_add(payload.len())
                .saturating_add(4)
                > MAX_H264_ACCESS_UNIT_BYTES
            {
                self.h264.clear();
                self.discard_ts = self.last_ts;
                return;
            }
            self.h264.extend_from_slice(&[0, 0, 0, 1]);
            self.h264.extend_from_slice(payload);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rtp(payload: &[u8]) -> Vec<u8> {
        let mut packet = vec![0u8; 12];
        packet[0] = 0x80;
        packet.extend_from_slice(payload);
        packet
    }

    /// A packet whose RTP marker bit says it ends an access unit.
    fn rtp_marked(payload: &[u8]) -> Vec<u8> {
        let mut packet = rtp(payload);
        packet[1] |= 0x80;
        packet
    }

    #[test]
    fn marker_bit_closes_the_access_unit() {
        let mut d = RtpDepacketizer::new(RtpKind::H264);
        assert_eq!(
            d.push(&rtp_marked(&[0x65, 0x01, 0x02])),
            vec![vec![0, 0, 0, 1, 0x65, 0x01, 0x02]]
        );
        assert!(d.flush().is_empty());
    }

    #[test]
    fn stap_a_splits_into_annexb_nals() {
        let mut d = RtpDepacketizer::new(RtpKind::H264);
        let mut payload = vec![0x78];
        payload.extend_from_slice(&[0, 2, 0x67, 0x01]);
        payload.extend_from_slice(&[0, 2, 0x68, 0x02]);
        assert_eq!(
            d.push(&rtp_marked(&payload)),
            vec![vec![0, 0, 0, 1, 0x67, 0x01, 0, 0, 0, 1, 0x68, 0x02]]
        );
    }

    #[test]
    fn fu_a_reassembles_into_one_nal() {
        let mut d = RtpDepacketizer::new(RtpKind::H264);
        let mut start = rtp(&[0x7c, 0x85, 0xaa]);
        start[2..4].copy_from_slice(&10u16.to_be_bytes());
        let mut middle = rtp(&[0x7c, 0x05, 0xbb]);
        middle[2..4].copy_from_slice(&11u16.to_be_bytes());
        let mut end = rtp_marked(&[0x7c, 0x45, 0xcc]);
        end[2..4].copy_from_slice(&12u16.to_be_bytes());
        assert!(d.push(&start).is_empty());
        assert!(d.push(&middle).is_empty());
        assert_eq!(d.push(&end), vec![vec![0, 0, 0, 1, 0x65, 0xaa, 0xbb, 0xcc]]);
        assert!(d.flush().is_empty());
    }

    #[test]
    fn partial_access_unit_is_dropped_instead_of_emitted_on_flush() {
        let mut d = RtpDepacketizer::new(RtpKind::H264);
        // The trailing packet of this unit never arrived, so flush must not
        // hand a truncated picture to the decoder.
        assert!(d.push(&rtp(&[0x65, 0x01])).is_empty());
        assert!(d.flush().is_empty());
        // The late trailing packet and the start of the next unit form a
        // second, incomplete unit: it is dropped as well.
        assert!(d.push(&rtp(&[0x65, 0x02])).is_empty());
        assert!(d.flush().is_empty());
    }

    #[test]
    fn marker_inside_an_unfinished_fu_does_not_emit_a_truncated_nal() {
        let mut d = RtpDepacketizer::new(RtpKind::H264);
        let mut start = rtp(&[0x7c, 0x85, 0xaa]);
        start[2..4].copy_from_slice(&10u16.to_be_bytes());
        let mut tail = rtp_marked(&[0x7c, 0x05, 0xbb]);
        tail[2..4].copy_from_slice(&11u16.to_be_bytes());
        assert!(d.push(&start).is_empty());
        assert!(d.push(&tail).is_empty());
        assert!(d.flush().is_empty());
    }

    #[test]
    fn timestamp_change_closes_the_unit() {
        let mut d = RtpDepacketizer::new(RtpKind::H264);
        let mut first = rtp(&[0x65, 0x01]);
        first[2..4].copy_from_slice(&20u16.to_be_bytes());
        first[4..8].copy_from_slice(&1u32.to_be_bytes());
        assert!(d.push(&first).is_empty());
        let mut second = rtp(&[0x65, 0x02]);
        second[2..4].copy_from_slice(&21u16.to_be_bytes());
        second[4..8].copy_from_slice(&2u32.to_be_bytes());
        assert_eq!(d.push(&second), vec![vec![0, 0, 0, 1, 0x65, 0x01]]);
        // An unmarked unit is incomplete, so it must not be flushed.
        assert!(d.flush().is_empty());
    }

    #[test]
    fn csrcs_extensions_and_padding_are_not_codec_payload() {
        let mut packet = rtp(&[0x65, 0x01, 0x02, 0, 0, 0]);
        packet[0] = 0x80 | 0x20 | 0x10 | 0x01;
        packet[1] = 0x80;
        packet.splice(12..12, [0, 0, 0, 1]);
        packet.splice(16..16, [0xbe, 0xde, 0, 1, 9, 8, 7, 6]);
        *packet.last_mut().unwrap() = 3;
        let mut d = RtpDepacketizer::new(RtpKind::H264);
        assert_eq!(d.push(&packet), vec![vec![0, 0, 0, 1, 0x65, 1, 2]]);
    }

    #[test]
    fn duplicate_and_late_packets_do_not_corrupt_the_next_unit() {
        let mut d = RtpDepacketizer::new(RtpKind::H264);
        let mut first = rtp_marked(&[0x65, 0x01]);
        first[2..4].copy_from_slice(&10u16.to_be_bytes());
        assert_eq!(d.push(&first), vec![vec![0, 0, 0, 1, 0x65, 1]]);
        assert!(d.push(&first).is_empty());
        let mut late = rtp(&[0x41, 0xff]);
        late[2..4].copy_from_slice(&9u16.to_be_bytes());
        assert!(d.push(&late).is_empty());
        assert!(d.flush().is_empty());
    }

    fn sequenced(sequence: u16, payload: u8) -> Vec<u8> {
        let mut packet = rtp(&[0x65, payload]);
        packet[2..4].copy_from_slice(&sequence.to_be_bytes());
        packet
    }

    #[test]
    fn reorder_buffer_delivers_contiguous_packets_immediately() {
        let now = Instant::now();
        let mut reorder = RtpReorderBuffer::new();
        assert_eq!(reorder.push(sequenced(10, 1), now), vec![sequenced(10, 1)]);
        assert_eq!(reorder.push(sequenced(11, 2), now), vec![sequenced(11, 2)]);
        assert!(!reorder.is_waiting());
    }

    #[test]
    fn reorder_buffer_holds_a_gap_and_releases_the_retransmission() {
        let now = Instant::now();
        let mut reorder = RtpReorderBuffer::new();
        reorder.push(sequenced(10, 1), now);
        // 11 is missing; 12 and 13 must wait for it.
        assert!(reorder.push(sequenced(12, 3), now).is_empty());
        assert!(reorder.push(sequenced(13, 4), now).is_empty());
        assert!(reorder.push(sequenced(10, 1), now).is_empty(), "duplicate");
        // The retransmission fills the hole and unblocks the held packets.
        assert_eq!(
            reorder.push(sequenced(11, 2), now),
            vec![sequenced(11, 2), sequenced(12, 3), sequenced(13, 4)]
        );
        assert!(!reorder.is_waiting());
    }

    #[test]
    fn reorder_buffer_skips_a_gap_after_the_hold_expires() {
        let now = Instant::now();
        let mut reorder = RtpReorderBuffer::new();
        reorder.push(sequenced(10, 1), now);
        reorder.push(sequenced(12, 3), now);
        let mut ready = Vec::new();
        reorder.release(now + REORDER_HOLD / 2, &mut ready);
        assert!(
            ready.is_empty(),
            "the gap must stay open before the deadline"
        );
        reorder.release(now + REORDER_HOLD, &mut ready);
        assert_eq!(ready, vec![sequenced(12, 3)]);
        // A retransmission arriving after the skip is too late.
        assert!(reorder.push(sequenced(11, 2), now).is_empty());
        assert_eq!(reorder.push(sequenced(13, 4), now), vec![sequenced(13, 4)]);
    }

    #[test]
    fn reorder_buffer_resynchronizes_after_a_large_jump() {
        let now = Instant::now();
        let mut reorder = RtpReorderBuffer::new();
        reorder.push(sequenced(10, 1), now);
        // More than 0x4000 packets ahead: treat as a new stream position
        // rather than dropping everything forever.
        assert_eq!(
            reorder.push(sequenced(20_000, 2), now),
            vec![sequenced(20_000, 2)]
        );
        assert_eq!(
            reorder.push(sequenced(20_001, 3), now),
            vec![sequenced(20_001, 3)]
        );
    }

    #[test]
    fn reorder_buffer_bounds_memory_when_the_sender_races_ahead() {
        let now = Instant::now();
        let mut reorder = RtpReorderBuffer::new();
        reorder.push(sequenced(10, 0), now);
        let mut released = Vec::new();
        for offset in 2..=(REORDER_MAX_PACKETS as u16 + 8) {
            released.extend(reorder.push(sequenced(10 + offset, 1), now));
        }
        assert!(!released.is_empty(), "overflow must release held packets");
        assert!(reorder.pending.len() <= REORDER_MAX_PACKETS);
    }

    #[test]
    fn malformed_stap_does_not_leave_a_partial_access_unit() {
        let mut d = RtpDepacketizer::new(RtpKind::H264);
        d.push(&rtp(&[0x78, 0, 2, 0x67, 0x01, 0, 9, 0x68]));
        assert!(d.flush().is_empty());
    }

    #[test]
    fn vp8_and_vp9_descriptors_are_not_sent_to_the_decoder() {
        let mut vp8 = RtpDepacketizer::new(RtpKind::Vp8);
        assert_eq!(vp8.push(&rtp(&[0x10, 0x01])), vec![vec![0x01]]);
        // X, I, L and T/K: 15-bit picture id, TL0PICIDX and T/K octet.
        assert_eq!(
            vp8.push(&rtp(&[0x90, 0xf0, 0x81, 0x02, 0x03, 0x04, 0x00, 0x9d])),
            vec![vec![0x00, 0x9d]]
        );
        let mut vp9 = RtpDepacketizer::new(RtpKind::Vp9);
        // I, L, B and V with one layer size and one group entry.
        assert_eq!(
            vp9.push(&rtp(&[
                0xaa, 0x01, 0x02, 0x03, 0x80, 0x10, 0, 1, 0, 2, 0x02, 0x9d,
            ])),
            vec![vec![0x02, 0x9d]]
        );
        // Flexible-mode inter frame: I, P and F with two P_DIFF octets.
        let mut vp9 = RtpDepacketizer::new(RtpKind::Vp9);
        assert_eq!(
            vp9.push(&rtp(&[0xd0, 0x01, 0x85, 0x86, 0x9d])),
            vec![vec![0x9d]]
        );
        let mut opus = RtpDepacketizer::new(RtpKind::Opus);
        assert_eq!(opus.push(&rtp(&[0x01, 0x02])), vec![vec![0x01, 0x02]]);
    }
}

//! RTP depacketization for the codecs Sharescreen negotiates.
//!
//! The RTP header is 12 bytes. H.264 is reassembled into Annex-B access units
//! (single NAL, STAP-A, FU-A). VP8/VP9 and Opus payloads are passed through
//! as-is; the decoder consumes the payload.
//!
//! An access unit is one RTP timestamp. Packets sharing a timestamp are
//! appended; the unit is returned when the timestamp changes or on
//! [`RtpDepacketizer::flush`].

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RtpKind {
    H264,
    Vp8,
    Vp9,
    Opus,
}

/// A depacketizer that turns RTP packets into decoder-ready frames.
pub struct RtpDepacketizer {
    kind: RtpKind,
    h264: Vec<u8>,
    fu: Vec<u8>,
    in_fu: bool,
    last_ts: Option<u32>,
}

impl RtpDepacketizer {
    pub fn new(kind: RtpKind) -> Self {
        Self {
            kind,
            h264: Vec::new(),
            fu: Vec::new(),
            in_fu: false,
            last_ts: None,
        }
    }

    /// Push one full RTP packet. Returns zero or more complete frames.
    pub fn push(&mut self, packet: &[u8]) -> Vec<Vec<u8>> {
        if packet.len() < 12 {
            return Vec::new();
        }
        let payload = &packet[12..];
        match self.kind {
            RtpKind::H264 => {
                let ts = u32::from_be_bytes([packet[4], packet[5], packet[6], packet[7]]);
                let mut out = Vec::new();
                if let Some(last) = self.last_ts {
                    if last != ts && !self.h264.is_empty() {
                        out.push(std::mem::take(&mut self.h264));
                    }
                }
                self.last_ts = Some(ts);
                self.push_h264(payload);
                out
            }
            RtpKind::Vp8 | RtpKind::Vp9 | RtpKind::Opus => vec![payload.to_vec()],
        }
    }

    /// Return the access unit still buffered, if any.
    pub fn flush(&mut self) -> Vec<Vec<u8>> {
        if self.h264.is_empty() {
            return Vec::new();
        }
        self.last_ts = None;
        vec![std::mem::take(&mut self.h264)]
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
                self.fu.extend_from_slice(&payload[2..]);
            }
            if end {
                self.in_fu = false;
                self.h264.extend_from_slice(&self.fu);
                self.fu.clear();
            }
            return;
        }
        if nal_type == 24 {
            // STAP-A: one or more NAL units, each with a 2-byte length.
            let mut i = 1;
            while i + 2 <= payload.len() {
                let size = u16::from_be_bytes([payload[i], payload[i + 1]]) as usize;
                i += 2;
                if i + size > payload.len() {
                    break;
                }
                self.h264.extend_from_slice(&[0, 0, 0, 1]);
                self.h264.extend_from_slice(&payload[i..i + size]);
                i += size;
            }
            return;
        }
        if (1..=23).contains(&nal_type) {
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
        packet.extend_from_slice(payload);
        packet
    }

    #[test]
    fn single_nal_is_buffered_until_flush() {
        let mut d = RtpDepacketizer::new(RtpKind::H264);
        assert!(d.push(&rtp(&[0x65, 0x01, 0x02])).is_empty());
        assert_eq!(d.flush(), vec![vec![0, 0, 0, 1, 0x65, 0x01, 0x02]]);
    }

    #[test]
    fn stap_a_splits_into_annexb_nals() {
        let mut d = RtpDepacketizer::new(RtpKind::H264);
        let mut payload = vec![0x78];
        payload.extend_from_slice(&[0, 2, 0x67, 0x01]);
        payload.extend_from_slice(&[0, 2, 0x68, 0x02]);
        assert!(d.push(&rtp(&payload)).is_empty());
        assert_eq!(
            d.flush(),
            vec![vec![0, 0, 0, 1, 0x67, 0x01, 0, 0, 0, 1, 0x68, 0x02]]
        );
    }

    #[test]
    fn fu_a_reassembles_into_one_nal() {
        let mut d = RtpDepacketizer::new(RtpKind::H264);
        assert!(d.push(&rtp(&[0x7c, 0x85, 0xaa])).is_empty());
        assert!(d.push(&rtp(&[0x7c, 0x05, 0xbb])).is_empty());
        assert!(d.push(&rtp(&[0x7c, 0x45, 0xcc])).is_empty());
        assert_eq!(d.flush(), vec![vec![0, 0, 0, 1, 0x65, 0xaa, 0xbb, 0xcc]]);
    }

    #[test]
    fn timestamp_change_closes_the_unit() {
        let mut d = RtpDepacketizer::new(RtpKind::H264);
        let mut first = rtp(&[0x65, 0x01]);
        first[4..8].copy_from_slice(&1u32.to_be_bytes());
        assert!(d.push(&first).is_empty());
        let mut second = rtp(&[0x65, 0x02]);
        second[4..8].copy_from_slice(&2u32.to_be_bytes());
        assert_eq!(d.push(&second), vec![vec![0, 0, 0, 1, 0x65, 0x01]]);
        assert_eq!(d.flush(), vec![vec![0, 0, 0, 1, 0x65, 0x02]]);
    }

    #[test]
    fn vp8_and_opus_pass_through() {
        let mut d = RtpDepacketizer::new(RtpKind::Vp8);
        assert_eq!(d.push(&rtp(&[0x10, 0x01])), vec![vec![0x10, 0x01]]);
        let mut d = RtpDepacketizer::new(RtpKind::Opus);
        assert_eq!(d.push(&rtp(&[0x01, 0x02])), vec![vec![0x01, 0x02]]);
    }
}

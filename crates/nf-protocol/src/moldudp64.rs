//! MoldUDP64 framing codec. Pure, zero-alloc, no panics on any input.
//! Grammar law: docs/02-moldudp64.md. Verify claims there before relying.

pub const HEADER_LEN: usize = 20;
pub const REQUEST_LEN: usize = 20; // same width as HEADER_LEN BY COINCIDENCE.
                                   // Different protocols. Never share the const.
pub const HEARTBEAT_COUNT: u16 = 0;
pub const EOS_COUNT: u16 = 0xFFFF;

pub type SessionId = [u8; 10];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub session: SessionId,
    pub seq: u64,
    pub count: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Data,
    Heartbeat,
    EndOfSession,
}

impl Header {
    /// Classification by count field (doc 02 §3).
    #[inline]
    pub fn kind(&self) -> Kind {
        match self.count {
            HEARTBEAT_COUNT => Kind::Heartbeat,
            EOS_COUNT => Kind::EndOfSession,
            _ => Kind::Data,
        }
    }

    /// Inclusive message span [seq, seq+count-1] for data packets.
    /// None for heartbeat/EOS (no messages) and on u64 overflow (P-4).
    #[inline]
    pub fn span(&self) -> Option<(u64, u64)> {
        if self.count == HEARTBEAT_COUNT || self.count == EOS_COUNT {
            return None;
        }
        let end = self.seq.checked_add((self.count as u64) - 1)?;
        Some((self.seq, end))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameError {
    /// Kept from G1 scaffold — frame shorter than 20 bytes.
    Truncated { need: usize, got: usize },
    /// P-5: bytes remaining after last block (or after HB/EOS header).
    TrailingBytes { extra: usize },
    /// Blocks end before `count` blocks are present.
    BlockOverrun,
    /// Block with Message Length == 0 (our policy V-5).
    ZeroLengthMessage,
    /// seq + count - 1 overflows u64 (P-4).
    SeqOverflow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessageBlock<'a> {
    /// Absolute message sequence number of this block.
    pub seq: u64,
    /// Message payload — a slice INTO the caller's frame (zero copy).
    pub data: &'a [u8],
}

/// Infallible iterator (P-1): only constructible from a validated packet.
/// Walks (pos += 2 + len, seq += 1) over bounds proven by `parse`.
#[derive(Debug, Clone)]
pub struct MessageBlocks<'a> {
    buf: &'a [u8],
    pos: usize,
    next_seq: u64,
    remaining: u16,
}

impl<'a> MessageBlocks<'a> {
    #[inline]
    pub(crate) fn new(buf: &'a [u8], start_seq: u64, count: u16) -> Self {
        Self {
            buf,
            pos: 0,
            next_seq: start_seq,
            remaining: count,
        }
    }
}

impl<'a> Iterator for MessageBlocks<'a> {
    type Item = MessageBlock<'a>;

    #[inline]
    fn next(&mut self) -> Option<MessageBlock<'a>> {
        if self.remaining == 0 {
            return None;
        }
        let len = u16::from_be_bytes([self.buf[self.pos], self.buf[self.pos + 1]]) as usize;
        let start = self.pos + 2;
        let end = start + len;
        let data = &self.buf[start..end];
        let seq = self.next_seq;

        self.pos = end;
        self.next_seq += 1;
        self.remaining -= 1;

        Some(MessageBlock { seq, data })
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        let rem = self.remaining as usize;
        (rem, Some(rem))
    }
}

impl<'a> ExactSizeIterator for MessageBlocks<'a> {
    #[inline]
    fn len(&self) -> usize {
        self.remaining as usize
    }
}

#[derive(Debug)]
pub enum Parsed<'a> {
    Data {
        header: Header,
        blocks: MessageBlocks<'a>,
    },
    Heartbeat {
        header: Header,
    },
    EndOfSession {
        header: Header,
    },
}

/// Kept from G1 scaffold; now infallible once buf.len() >= 20 checked by
/// callers — internal helper for `parse`, public for tests.
#[inline]
pub fn parse_header(buf: &[u8]) -> Result<Header, FrameError> {
    if buf.len() < HEADER_LEN {
        return Err(FrameError::Truncated {
            need: HEADER_LEN,
            got: buf.len(),
        });
    }
    let mut session = [0u8; 10];
    session.copy_from_slice(&buf[0..10]);
    let seq = u64::from_be_bytes([
        buf[10], buf[11], buf[12], buf[13], buf[14], buf[15], buf[16], buf[17],
    ]);
    let count = u16::from_be_bytes([buf[18], buf[19]]);
    Ok(Header { session, seq, count })
}

/// Full eager validation (P-1, P-2, P-5) per doc 02 §6.1 — normative
/// pseudocode there. First-match-wins error order: V-1, V-6, V-2/V-3/V-4/V-5.
/// Never panics, never allocates, never reads OOB on ANY input slice.
pub fn parse(buf: &[u8]) -> Result<Parsed<'_>, FrameError> {
    if buf.len() < HEADER_LEN {
        return Err(FrameError::Truncated {
            need: HEADER_LEN,
            got: buf.len(),
        });
    }

    let hdr = parse_header(buf)?;

    if hdr.count != HEARTBEAT_COUNT && hdr.count != EOS_COUNT {
        if hdr.seq.checked_add((hdr.count as u64) - 1).is_none() {
            return Err(FrameError::SeqOverflow);
        }
    }

    match hdr.count {
        HEARTBEAT_COUNT => {
            if buf.len() != HEADER_LEN {
                return Err(FrameError::TrailingBytes {
                    extra: buf.len() - HEADER_LEN,
                });
            }
            Ok(Parsed::Heartbeat { header: hdr })
        }
        EOS_COUNT => {
            if buf.len() != HEADER_LEN {
                return Err(FrameError::TrailingBytes {
                    extra: buf.len() - HEADER_LEN,
                });
            }
            Ok(Parsed::EndOfSession { header: hdr })
        }
        _ => {
            let mut rest = &buf[HEADER_LEN..];
            for _ in 0..hdr.count {
                if rest.len() < 2 {
                    return Err(FrameError::BlockOverrun);
                }
                let len = u16::from_be_bytes([rest[0], rest[1]]) as usize;
                if len == 0 {
                    return Err(FrameError::ZeroLengthMessage);
                }
                if rest.len() < 2 + len {
                    return Err(FrameError::BlockOverrun);
                }
                rest = &rest[2 + len..];
            }
            if !rest.is_empty() {
                return Err(FrameError::TrailingBytes {
                    extra: rest.len(),
                });
            }
            Ok(Parsed::Data {
                header: hdr,
                blocks: MessageBlocks::new(&buf[HEADER_LEN..], hdr.seq, hdr.count),
            })
        }
    }
}

/// Encode a retransmission request (doc 02 §2.3) into `out`.
/// Contract: count >= 1 (debug_assert); exactly REQUEST_LEN bytes written.
/// Zero-alloc by construction.
#[inline]
pub fn encode_request(session: &SessionId, from: u64, count: u16, out: &mut [u8; REQUEST_LEN]) {
    debug_assert!(count >= 1, "request count must be >= 1");
    out[0..10].copy_from_slice(session);
    out[10..18].copy_from_slice(&from.to_be_bytes());
    out[18..20].copy_from_slice(&count.to_be_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_packet(session: &SessionId, seq: u64, msgs: &[&[u8]]) -> Vec<u8> {
        let mut buf = Vec::with_capacity(HEADER_LEN + msgs.len() * 32);
        buf.extend_from_slice(session);
        buf.extend_from_slice(&seq.to_be_bytes());
        buf.extend_from_slice(&(msgs.len() as u16).to_be_bytes());
        for msg in msgs {
            buf.extend_from_slice(&(msg.len() as u16).to_be_bytes());
            buf.extend_from_slice(msg);
        }
        buf
    }

    const TV1_SESSION: SessionId = *b"NFTESTSESS";
    const TV1_MSG1: [u8; 12] = [0x53, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x4F];
    const TV1_MSG2: [u8; 12] = [0x53, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x43];

    const TV1_EXPECTED_BYTES: [u8; 48] = [
        0x4E, 0x46, 0x45, 0x53, 0x54, 0x53, 0x45, 0x53, 0x53, 0x00, // Session "NFTESTSESS"
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0xE8,             // Seq = 1000
        0x00, 0x02,                                                 // Count = 2
        0x00, 0x0C,                                                 // Msg 1 len = 12
        0x53, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x4F, // Msg 1
        0x00, 0x0C,                                                 // Msg 2 len = 12
        0x53, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x43, // Msg 2
    ];

    const TV2_EXPECTED_BYTES: [u8; 20] = [
        0x4E, 0x46, 0x45, 0x53, 0x54, 0x53, 0x45, 0x53, 0x53, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0xEA,
        0x00, 0x00,
    ];

    const TV3_EXPECTED_BYTES: [u8; 20] = [
        0x4E, 0x46, 0x45, 0x53, 0x54, 0x53, 0x45, 0x53, 0x53, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0xEA,
        0xFF, 0xFF,
    ];

    const TV4_EXPECTED_BYTES: [u8; 20] = [
        0x4E, 0x46, 0x45, 0x53, 0x54, 0x53, 0x45, 0x53, 0x53, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0xDE,
        0x00, 0x14,
    ];

    #[test]
    fn t1_tv1_golden() {
        let built = build_packet(&TV1_SESSION, 1000, &[&TV1_MSG1, &TV1_MSG2]);
        assert_eq!(built.as_slice(), &TV1_EXPECTED_BYTES);

        let parsed = parse(&built).expect("parse valid TV-1");
        match parsed {
            Parsed::Data { header, mut blocks } => {
                assert_eq!(header.session, TV1_SESSION);
                assert_eq!(header.seq, 1000);
                assert_eq!(header.count, 2);
                assert_eq!(header.kind(), Kind::Data);
                assert_eq!(blocks.len(), 2);

                let b1 = blocks.next().expect("block 1");
                assert_eq!(b1.seq, 1000);
                assert_eq!(b1.data, &TV1_MSG1);

                let b2 = blocks.next().expect("block 2");
                assert_eq!(b2.seq, 1001);
                assert_eq!(b2.data, &TV1_MSG2);

                assert!(blocks.next().is_none());
            }
            _ => panic!("Expected Parsed::Data"),
        }
    }

    #[test]
    fn t2_t3_heartbeat_and_eos() {
        // Heartbeat
        let parsed_hb = parse(&TV2_EXPECTED_BYTES).expect("parse TV-2");
        match parsed_hb {
            Parsed::Heartbeat { header } => {
                assert_eq!(header.seq, 1002);
                assert_eq!(header.count, 0);
                assert_eq!(header.kind(), Kind::Heartbeat);
            }
            _ => panic!("Expected Parsed::Heartbeat"),
        }

        let mut hb_extra = TV2_EXPECTED_BYTES.to_vec();
        hb_extra.push(0xAA);
        assert_eq!(parse(&hb_extra).unwrap_err(), FrameError::TrailingBytes { extra: 1 });

        // End of Session
        let parsed_eos = parse(&TV3_EXPECTED_BYTES).expect("parse TV-3");
        match parsed_eos {
            Parsed::EndOfSession { header } => {
                assert_eq!(header.seq, 1002);
                assert_eq!(header.count, 0xFFFF);
                assert_eq!(header.kind(), Kind::EndOfSession);
            }
            _ => panic!("Expected Parsed::EndOfSession"),
        }

        let mut eos_extra = TV3_EXPECTED_BYTES.to_vec();
        eos_extra.push(0xBB);
        assert_eq!(parse(&eos_extra).unwrap_err(), FrameError::TrailingBytes { extra: 1 });
    }

    #[test]
    fn t4_encode_request() {
        let mut req = [0u8; REQUEST_LEN];
        encode_request(&TV1_SESSION, 990, 20, &mut req);
        assert_eq!(&req, &TV4_EXPECTED_BYTES);
    }

    #[test]
    fn t5_truncated_frame() {
        let input = [0u8; 19];
        assert_eq!(
            parse(&input).unwrap_err(),
            FrameError::Truncated { need: 20, got: 19 }
        );
    }

    #[test]
    fn t6_block_overrun() {
        // header claims 3 blocks, but only 2 provided
        let mut buf = build_packet(&TV1_SESSION, 1000, &[&TV1_MSG1, &TV1_MSG2]);
        buf[18] = 0;
        buf[19] = 3; // set count = 3
        assert_eq!(parse(&buf).unwrap_err(), FrameError::BlockOverrun);
    }

    #[test]
    fn t7_trailing_bytes() {
        // header claims 1 block, but 2 provided
        let mut buf = build_packet(&TV1_SESSION, 1000, &[&TV1_MSG1, &TV1_MSG2]);
        buf[18] = 0;
        buf[19] = 1; // set count = 1
        assert_eq!(parse(&buf).unwrap_err(), FrameError::TrailingBytes { extra: 14 });
    }

    #[test]
    fn t8_zero_length_message() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&TV1_SESSION);
        buf.extend_from_slice(&1000u64.to_be_bytes());
        buf.extend_from_slice(&1u16.to_be_bytes()); // count = 1
        buf.extend_from_slice(&0u16.to_be_bytes()); // len = 0
        assert_eq!(parse(&buf).unwrap_err(), FrameError::ZeroLengthMessage);
    }

    #[test]
    fn t9_seq_overflow() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&TV1_SESSION);
        buf.extend_from_slice(&u64::MAX.to_be_bytes());
        buf.extend_from_slice(&2u16.to_be_bytes()); // count = 2
        buf.extend_from_slice(&4u16.to_be_bytes());
        buf.extend_from_slice(&[1, 2, 3, 4]);
        buf.extend_from_slice(&4u16.to_be_bytes());
        buf.extend_from_slice(&[5, 6, 7, 8]);
        assert_eq!(parse(&buf).unwrap_err(), FrameError::SeqOverflow);
    }

    struct SimpleRng(u64);
    impl SimpleRng {
        fn next_u64(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
    }

    #[test]
    fn t10_round_trip_property() {
        const SEED: u64 = 0xCAFE_BABE_DEAD_BEEF;
        let mut rng = SimpleRng(SEED);

        for _ in 0..10_000 {
            let count = ((rng.next_u64() % 40) + 1) as u16;
            let start_seq = rng.next_u64() % 1_000_000_000;
            let mut msgs: Vec<Vec<u8>> = Vec::with_capacity(count as usize);

            for _ in 0..count {
                let msg_len = ((rng.next_u64() % 64) + 1) as usize;
                let mut msg = vec![0u8; msg_len];
                for b in &mut msg {
                    *b = (rng.next_u64() & 0xFF) as u8;
                }
                msgs.push(msg);
            }

            let msg_slices: Vec<&[u8]> = msgs.iter().map(|m| m.as_slice()).collect();
            let raw_packet = build_packet(&TV1_SESSION, start_seq, &msg_slices);

            let parsed = parse(&raw_packet).unwrap_or_else(|e| {
                panic!("Failed to parse valid random packet with seed {:#x}: {:?}", SEED, e)
            });

            match parsed {
                Parsed::Data { header, blocks } => {
                    assert_eq!(header.seq, start_seq);
                    assert_eq!(header.count, count);
                    assert_eq!(blocks.len(), count as usize);

                    for (idx, block) in blocks.enumerate() {
                        assert_eq!(block.seq, start_seq + idx as u64);
                        assert_eq!(block.data, msgs[idx].as_slice());
                    }
                }
                _ => panic!("Expected Parsed::Data for generated packet"),
            }
        }
    }

    #[test]
    fn t11_span_calculation() {
        let data_hdr = Header {
            session: TV1_SESSION,
            seq: 1000,
            count: 2,
        };
        assert_eq!(data_hdr.span(), Some((1000, 1001)));

        let hb_hdr = Header {
            session: TV1_SESSION,
            seq: 1000,
            count: HEARTBEAT_COUNT,
        };
        assert_eq!(hb_hdr.span(), None);

        let eos_hdr = Header {
            session: TV1_SESSION,
            seq: 1000,
            count: EOS_COUNT,
        };
        assert_eq!(eos_hdr.span(), None);

        let overflow_hdr = Header {
            session: TV1_SESSION,
            seq: u64::MAX,
            count: 2,
        };
        assert_eq!(overflow_hdr.span(), None);
    }
}

//! MoldUDP64 framing + per-block ITCH validation in one pass. This is the
//! single entry the arbitrator (doc 05) and replay (doc 04) will call.

use crate::{itch5, moldudp64};

#[derive(Debug, PartialEq, Eq)]
pub enum PacketError {
    Framing(moldudp64::FrameError),
    Payload(itch5::ItchError),
}

/// R2: deterministic-replay validation memo carried per rendered frame.
///
/// `valid_count` is the EXACT leading prefix of the frame's message blocks
/// that pass `itch5::validate` (block `valid_count` is the first failure when
/// `valid_count < n`). It is computed ONCE at transport construction from the
/// same immutable bytes the in-window walk would read — the verdict of a pure
/// function over immutable inputs, memoized. Session-prefix patching never
/// touches message bodies (bytes 20+), so the memo stays valid across
/// `reset()`.
///
/// Equivalence claim (proved by construction + D10 tests):
///   for immutable frame bytes f with blocks b_0..b_{n-1}:
///     memo(f).valid_count = max k such that forall i < k: validate(b_i) = Ok
///   hence "valid_count == n" <=> "every in-window validate call returns Ok",
///   and "valid_count = k < n" <=> "the first in-window failure is b_k".
/// The sequencer uses this to gate the R3 span fast path and to skip per-
/// message validate on the classic path WITHOUT changing any observable
/// (emitted prefix, violation counters, error mapping) for either branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameMemo {
    pub valid_count: u16,
}

/// Single-pass fused framing + per-block callback walk for the ingest hot path
/// (P9c). Replaces parse-then-validate-then-emit (3 block-walks/packet) with ONE
/// pass: framing bounds are checked per block and `f` runs for blocks past
/// `skip` (dup prefix was validated on first receipt — deterministic bytes).
/// Trailing-bytes checked at end. Error mapping identical to validate_frame.
/// Edge difference (untested anywhere in the suite): errors surface in block
/// order, so an invalid frame may emit/stage its valid prefix before the
/// error return; validate_frame stays available for strict two-phase callers.
#[inline(always)]
pub fn ingest_walk(
    frame: &[u8],
    first_seq: u64,
    count: u16,
    skip: usize,
    f: &mut impl FnMut(u64, &[u8]) -> Result<(), itch5::ItchError>,
) -> Result<(), PacketError> {
    let mut pos = moldudp64::HEADER_LEN;
    let mut seq = first_seq;
    let mut to_skip = skip;
    for _ in 0..count {
        if frame.len() < pos + 2 {
            std::hint::cold_path();
            return Err(PacketError::Framing(moldudp64::FrameError::BlockOverrun));
        }
        let len = u16::from_be_bytes([frame[pos], frame[pos + 1]]) as usize;
        let start = pos + 2;
        let end = start + len;
        if end > frame.len() {
            std::hint::cold_path();
            return Err(PacketError::Framing(moldudp64::FrameError::BlockOverrun));
        }
        if to_skip > 0 {
            to_skip -= 1;
        } else if let Err(e) = f(seq, &frame[start..end]) {
            std::hint::cold_path();
            return Err(PacketError::Payload(e));
        }
        pos = end;
        seq = seq.wrapping_add(1);
    }
    if pos != frame.len() {
        std::hint::cold_path();
        return Err(PacketError::Framing(moldudp64::FrameError::TrailingBytes {
            extra: frame.len() - pos,
        }));
    }
    Ok(())
}

/// Parse the MoldUDP64 frame and validate every payload block against the
/// ITCH 5.0 LENGTH table. Returns Ok(Parsed) only if both framing and all
/// message payloads are structurally valid.
/// P2: always-inline — fuses parse + ITCH walk into ingest (saves call/packet).
#[inline(always)]
pub fn validate_frame(buf: &[u8]) -> Result<moldudp64::Parsed<'_>, PacketError> {
    let parsed = moldudp64::parse(buf).map_err(PacketError::Framing)?;
    if let moldudp64::Parsed::Data { ref blocks, .. } = parsed {
        for block in blocks.clone() {
            itch5::validate(block.data).map_err(PacketError::Payload)?;
        }
    }
    Ok(parsed)
}

#[cfg(test)]
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_frame_synthetic() {
        const SESSION: moldudp64::SessionId = *b"TESTSESS01";
        let mut frame = Vec::new();
        frame.extend_from_slice(&SESSION);
        frame.extend_from_slice(&100u64.to_be_bytes()); // seq
        frame.extend_from_slice(&2u16.to_be_bytes()); // count = 2

        // Msg 1: System Event (12B)
        let msg1 = [
            0x53, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x4F,
        ];
        frame.extend_from_slice(&(msg1.len() as u16).to_be_bytes());
        frame.extend_from_slice(&msg1);

        // Msg 2: Delete (19B)
        let msg2 = [
            0x44, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1,
        ];
        frame.extend_from_slice(&(msg2.len() as u16).to_be_bytes());
        frame.extend_from_slice(&msg2);

        let res = validate_frame(&frame).expect("validate valid frame");
        match res {
            moldudp64::Parsed::Data { header, blocks } => {
                assert_eq!(header.seq, 100);
                assert_eq!(header.count, 2);
                assert_eq!(blocks.len(), 2);
            }
            _ => panic!("Expected Data"),
        }

        // Corrupt msg2 with invalid type byte 0xFE
        let mut bad_frame = frame.clone();
        let msg2_type_pos = 20 + 2 + 12 + 2;
        bad_frame[msg2_type_pos] = 0xFE;
        assert_eq!(
            validate_frame(&bad_frame),
            Err(PacketError::Payload(itch5::ItchError::UnknownType {
                t: 0xFE
            }))
        );
    }
}

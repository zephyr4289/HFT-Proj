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

/// R12: the 8-entry vectorized watermark-ladder check — a pure function of
/// a GATHERED group: the caller collects `firsts[8]` and `ns[8]` from the
/// entries it is about to scan (AoS loads, L1-hot — zero transport-side
/// cost) and passes pointers to those arrays with the scan's current
/// watermark `w`. It returns whether the 8 entries are exactly:
///
/// * `[emit, dup, emit, dup, emit, dup, emit, dup]` — even entries emit
///   (their `first`s chain `w → w+n₀ → …`), odd entries are PURE duplicates
///   (same `first` as their even partner, `n_odd ≤ n_even` so their last
///   sequence sits below the watermark at their turn),
///
/// in which case the scan can advance `w` by `Σ n_even`, emit the four even
/// spans, and count the four odd entries as duplicates — observably
/// identical (counters, emissions, watermark) to running the scalar ladder
/// over the same eight entries, proven by the pair/chains/anchor relations
/// (see `nf_testkit::soa` for the derivation).
///
/// The implementation lives behind a runtime CPU gate (`avx512f`) in
/// `nf_testkit::soa` because CI compiles `x86-64-v3`; the type lives here as
/// the shared contract between the arbitrator (the gather + the scalar
/// fallback) and the testkit (the SIMD implementation). Raw pointers keep
/// the per-group call lean (no fat slices); passing raw pointers is safe,
/// and the implementation's dereference is bounded by the documented
/// contract.
pub type SoaLadder8 = fn(*const u64, *const u64, u64) -> bool;

/// R8: one frame ready for batched ingest — the frame bytes together with
/// the transport's precomputed per-frame index (Q1 block triples + R2
/// validation memo) in a single value, so the sequencer's batch apply loop
/// consumes frames with zero side-table indirection.
///
/// `blocks`/`memo` follow the exact contracts of
/// `Transport::batch_blocks`/`Transport::batch_memo`: empty blocks means
/// "no index" (HB/EOS frame, live transport, or defensive fallback — the
/// sequencer then takes the classic per-frame path, identical observables);
/// `None` memo means "unmemoized — validate in-window".
///
/// The batched apply path is observationally identical to feeding the same
/// frames through `ingest_auto` one by one: the same per-frame arbitration
/// ladder runs (session dispatch, kind classify, span/dup classify, apply),
/// in the same order, with the same counters and emissions. The only
/// difference is mechanical: sequencer state is hoisted across the steady
/// run and span emissions are buffered until a cold path or batch end
/// flushes them in order.
#[derive(Debug, Clone, Copy)]
pub struct FrameEntry<'a> {
    pub bytes: &'a [u8],
    /// Origin feed (arbitrator `FeedId` — plain u8, aliased at the consumer).
    pub feed: u8,
    pub blocks: &'a [(u64, u32, u32)],
    pub memo: Option<FrameMemo>,
    /// R8: the frame's first block sequence number, carried inline by the
    /// transport's slot (from the schedule at render time). With
    /// `blocks.len()` this yields last = first + n - 1 without touching the
    /// triple store; for triple-carrying rendered frames it equals
    /// `blocks[0].0` by construction (the render walk emits exactly
    /// `first_seq + i` triples or tombstones the frame).
    pub first_seq: u64,
    /// R8: the frame's session prefix as the sequencer's fused compare
    /// words (bytes 0..8 / 2..10 little-endian), carried inline by the
    /// slot — equals the corresponding `bytes` words by construction (the
    /// publisher computes them from those bytes). Lets the steady scan run
    /// without touching the frame lines (cross-core in pipelined mode).
    pub sess_lo: u64,
    pub sess_hi: u64,
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

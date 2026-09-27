//! Types, events, proofs, and sink definitions for the sequencer.

pub type FeedId = u8;

/// Zero-cost proof that a message was emitted from the contiguous sequence path.
/// Minted exclusively during in-order frame emission or in-order drain (doc 05 §9).
#[derive(Debug, PartialEq, Eq)]
pub struct LiveFeedProof {
    pub(crate) gen: u64,
}

impl LiveFeedProof {
    /// Returns the proof era generation counter.
    #[inline(always)]
    pub fn gen(&self) -> u64 {
        self.gen
    }
}

/// Sequencer lifecycle and control plane events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    GapOpened {
        from: u64,
        ahead: Option<u64>,
        gen: u64,
    },
    ReAnchored {
        gen: u64,
        at: u64,
    },
    SessionBoundary {
        prev: [u8; 10],
        next: [u8; 10],
        gen: u64,
    },
    EndOfSession {
        session: [u8; 10],
        final_wm: u64,
        announced_next: u64,
    },
    SessionDead {
        reason: DeadReason,
        last_wm: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeadReason {
    RetryExhausted,
    TcpUnreachable,
    Sealed,
}

/// Test-only mutation modes for differential oracle validation (doc 16 / D3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SequencerMutation {
    #[default]
    None,
    DisableClearOnAdvance, // Bug A: zombie class
    OffByOneClamp,         // Bug B: off-by-one clamp
    DropStagedAtEos,       // Bug C: drop last staged message at EOS
}

/// Outstanding gap recovery intent suggested by the sequencer (doc 05 §10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecoveryIntent {
    pub from: u64,
    pub to_excl: u64,
}

/// Confluence consumer sink. Single-threaded fold target.
///
/// R3 span protocol: a sink MAY opt into batched emission for contiguous
/// runs. Within one data packet on the contiguous fast path, every per-message
/// decision the sequencer makes is identical (same proof era `gen`, same
/// session, strictly consecutive sequence numbers, all-validated bodies —
/// see the R2 FrameMemo equivalence), so per-message emission
/// `on_msg(p, s_i, m_i) for i in 0..n` is observationally equivalent to a
/// single `on_span(p, s_0, n, body, blocks)` for sinks that can consume runs.
/// Sinks that need per-message call semantics simply keep
/// `wants_spans() == false` (the default) and observe bit-identical behavior.
pub trait Sink {
    /// Invoked per contiguous message with valid proof.
    fn on_msg(&mut self, proof: &LiveFeedProof, seq: u64, msg: &[u8]);
    /// Invoked per control-plane event.
    fn on_event(&mut self, ev: &Event);
    /// R3: opt into span (batched) emission for contiguous runs. Default false
    /// — sinks keep exact per-message on_msg semantics. Monomorphized per
    /// sink type, so the sequencer's gate on this folds to a compile-time
    /// constant: zero cost for classic sinks.
    #[inline(always)]
    fn wants_spans(&self) -> bool {
        false
    }
    /// R3: batched emission of a contiguous, all-validated run.
    /// Contract (only called when `wants_spans()` returned true):
    /// - messages `first_seq ..= first_seq + count - 1`, emitted in order,
    ///   all under the same proof era `proof.gen()` (identical guarantee the
    ///   per-message path provides for the same run);
    /// - `body` = the exact bytes of those messages back-to-back, including
    ///   their 2B big-endian length prefixes: message i starts at
    ///   `body[blocks[i].1 - blocks[0].1]` with length `blocks[i].2 -
    ///   blocks[i].1` (blocks are the frame's `(seq, start, end)` triples,
    ///   absolute into the frame);
    /// - `blocks[i].0 == first_seq + i` for i < count.
    ///
    /// Default impl is never invoked (gated on wants_spans()).
    #[inline(always)]
    fn on_span(
        &mut self,
        _proof: &LiveFeedProof,
        _first_seq: u64,
        _count: u16,
        _body: &[u8],
        _blocks: &[(u64, u32, u32)],
    ) {
    }
}

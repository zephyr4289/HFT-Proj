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
pub trait Sink {
    /// Invoked per contiguous message with valid proof.
    fn on_msg(&mut self, proof: &LiveFeedProof, seq: u64, msg: &[u8]);
    /// Invoked per control-plane event.
    fn on_event(&mut self, ev: &Event);
}

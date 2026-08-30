#![forbid(unsafe_code)]

pub type FeedId = u8;

pub struct LiveFeedProof {
    pub(crate) _gen: u64,
}

#[derive(Debug)]
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
    },
    SessionDead {
        reason: DeadReason,
        last_wm: u64,
    },
}

#[derive(Debug)]
pub enum DeadReason {
    RetryExhausted,
    TcpUnreachable,
}

#[derive(Debug, Clone, Copy)]
pub struct RecoveryIntent {
    pub from: u64,
    pub to_incl: u64,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Counters {
    pub msgs_in_order: u64,
    pub msgs_staged: u64,
    pub msgs_dropped_dup: u64,
    pub gaps_opened: u64,
}

pub trait Sink {
    fn on_msg(&mut self, proof: &LiveFeedProof, seq: u64, msg: &[u8]);
    fn on_event(&mut self, ev: Event);
}

pub struct Sequencer {
    /* doc 05 defines every field */
}

impl Sequencer {
    pub fn new(_anchor_session: [u8; 10]) -> Self {
        todo!("doc 05")
    }

    pub fn ingest<S: Sink>(
        &mut self,
        _frame: &[u8],
        _feed: FeedId,
        _now_ns: u64,
        _sink: &mut S,
    ) {
        todo!("doc 05")
    }

    pub fn recovery_intent(&mut self, _now_ns: u64) -> Option<RecoveryIntent> {
        todo!("doc 08")
    }

    pub fn watermark(&self) -> u64 {
        todo!()
    }

    pub fn counters(&self) -> Counters {
        todo!()
    }
}

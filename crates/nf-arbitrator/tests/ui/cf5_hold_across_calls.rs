// This is the bug class that killed production systems — "apply the book update we cached from before the outage."

use nf_arbitrator::{Event, LiveFeedProof, Sink};

struct StashingSink<'a> {
    held_proof: Option<&'a LiveFeedProof>,
}

impl<'a> Sink for StashingSink<'a> {
    fn on_msg(&mut self, proof: &LiveFeedProof, _seq: u64, _msg: &[u8]) {
        self.held_proof = Some(proof);
    }

    fn on_event(&mut self, _ev: &Event) {}
}

fn main() {}

use nf_arbitrator::LiveFeedProof;

struct FakeProof {
    _gen: u64,
}

fn consume_proof(_proof: &LiveFeedProof) {}

fn main() {
    let fake = FakeProof { _gen: 42 };
    consume_proof(&fake);
}

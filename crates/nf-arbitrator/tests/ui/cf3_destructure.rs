use nf_arbitrator::LiveFeedProof;

fn test_destructure(p: &LiveFeedProof) {
    let LiveFeedProof { .. } = p;
}

fn main() {}

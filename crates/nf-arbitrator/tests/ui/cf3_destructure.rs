use nf_arbitrator::LiveFeedProof;

fn test_destructure(p: &LiveFeedProof) {
    let LiveFeedProof { gen } = *p;
}

fn main() {}

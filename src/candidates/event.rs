use super::{CandidateContext, CandidateSource};
use crate::sampling::{Candidate, event_candidates};

pub struct VisibilityEvents {
    radius: f64,
    limit: usize,
}

impl VisibilityEvents {
    pub fn new(radius: f64, limit: usize) -> Self {
        assert!(radius > 0.0, "event radius must be positive");
        Self { radius, limit }
    }
}

impl CandidateSource for VisibilityEvents {
    fn name(&self) -> &'static str {
        "visibility events"
    }

    fn generate(&self, ctx: &CandidateContext<'_>) -> Vec<Candidate> {
        event_candidates(ctx.instance, self.radius, self.limit)
    }
}

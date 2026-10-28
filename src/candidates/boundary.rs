use rayon::prelude::*;

use super::{CandidateContext, CandidateSource};
use crate::sampling::{Candidate, boundary_candidates, boundary_candidates_convex_only};

pub struct UniformBoundary {
    spacing: f64,
    skip_reflex: bool,
}

impl UniformBoundary {
    pub fn new(spacing: f64) -> Self {
        assert!(spacing > 0.0, "candidate spacing must be positive");
        Self {
            spacing,
            skip_reflex: false,
        }
    }

    pub fn with_skip_reflex(mut self, skip_reflex: bool) -> Self {
        self.skip_reflex = skip_reflex;
        self
    }
}

impl CandidateSource for UniformBoundary {
    fn name(&self) -> &'static str {
        "uniform boundary"
    }

    fn generate(&self, ctx: &CandidateContext<'_>) -> Vec<Candidate> {
        let generate_ring = if self.skip_reflex {
            boundary_candidates_convex_only
        } else {
            boundary_candidates
        };
        ctx.instance
            .buildings
            .par_iter()
            .enumerate()
            .flat_map_iter(|(building, b)| {
                generate_ring(&b.vertices, self.spacing, building as u32)
            })
            .collect()
    }
}

use rayon::prelude::*;

use super::near_threshold::near_threshold;
use super::{CandidateContext, CandidateSource};
use crate::sampling::{Candidate, CandidateKind, boundary_candidates};

pub struct LocalBoundary {
    spacing: f64,
    progress_window: (f64, f64),
    viewers_per_building: usize,
}

impl LocalBoundary {
    pub fn new(spacing: f64) -> Self {
        assert!(spacing > 0.0, "local spacing must be positive");
        Self {
            spacing,
            progress_window: (0.35, 1.0),
            viewers_per_building: 4,
        }
    }

    pub fn with_progress_window(mut self, low: f64, high: f64) -> Self {
        assert!(low < high, "progress window must be non-empty");
        self.progress_window = (low, high);
        self
    }

    pub fn with_viewers(mut self, viewers: usize) -> Self {
        self.viewers_per_building = viewers;
        self
    }
}

impl CandidateSource for LocalBoundary {
    fn name(&self) -> &'static str {
        "local boundary"
    }

    fn generate(&self, ctx: &CandidateContext<'_>) -> Vec<Candidate> {
        let limit = (ctx.incumbent.len() * 4).max(64);
        let Some(near) =
            near_threshold(ctx, self.progress_window, limit, self.viewers_per_building)
        else {
            return Vec::new();
        };
        let mut marked = vec![false; ctx.instance.buildings.len()];
        for host in near.hosts.into_iter().flatten() {
            marked[host] = true;
        }
        for (building, viewers) in near.buildings {
            marked[building] = true;
            for viewer in viewers {
                marked[viewer] = true;
            }
        }

        marked
            .par_iter()
            .enumerate()
            .filter(|&(_, &marked)| marked)
            .flat_map_iter(|(building, _)| {
                boundary_candidates(
                    &ctx.instance.buildings[building].vertices,
                    self.spacing,
                    building as u32,
                )
                .into_iter()
                .map(|candidate| Candidate {
                    kind: CandidateKind::Local,
                    ..candidate
                })
            })
            .collect()
    }
}

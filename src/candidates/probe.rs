use framework::{Point, bbox};
use rayon::prelude::*;

use super::{CandidateContext, CandidateSource};
use crate::antennas::project_candidate_to_valid;
use crate::sampling::{Candidate, CandidateKind};

const DEFAULT_RADIUS_FRACTION: f64 = 0.02;

pub struct ProbeRings {
    rings: usize,
    directions: usize,
    radius_fraction: f64,
}

impl ProbeRings {
    pub fn new(rings: usize, directions: usize) -> Self {
        assert!(rings > 0 && directions > 0, "probe rings must be non-empty");
        Self {
            rings,
            directions,
            radius_fraction: DEFAULT_RADIUS_FRACTION,
        }
    }

    pub fn with_radius_fraction(mut self, fraction: f64) -> Self {
        assert!(fraction > 0.0, "radius fraction must be positive");
        self.radius_fraction = fraction;
        self
    }
}

impl CandidateSource for ProbeRings {
    fn name(&self) -> &'static str {
        "probe rings"
    }

    fn generate(&self, ctx: &CandidateContext<'_>) -> Vec<Candidate> {
        if ctx.incumbent.is_empty() {
            return Vec::new();
        }
        let (min, max) = bbox(&ctx.instance.buildings);
        let diagonal = (max - min).norm().max(1.0);
        let initial_radius = diagonal * self.radius_fraction;
        let search_radius = diagonal * 4.0;
        let phase = seed_phase(ctx.seed);

        ctx.incumbent
            .par_iter()
            .flat_map_iter(|&center| {
                let directions = self.directions;
                (0..self.rings).flat_map(move |ring| {
                    let radius = initial_radius * 0.5_f64.powi(ring as i32);
                    (0..directions).map(move |direction| {
                        let angle = phase
                            + std::f64::consts::PI * ring as f64 / directions as f64
                            + std::f64::consts::TAU * direction as f64 / directions as f64;
                        center + Point::new(angle.cos(), angle.sin()) * radius
                    })
                })
            })
            .filter_map(|position| {
                project_candidate_to_valid(
                    ctx.evaluator,
                    Candidate {
                        position,
                        building: 0,
                        kind: CandidateKind::Probe,
                    },
                    search_radius,
                )
            })
            .collect()
    }
}

pub(crate) fn seed_phase(seed: Option<u64>) -> f64 {
    let unit = seed.unwrap_or(0).wrapping_mul(0x9e37_79b9_7f4a_7c15) as f64 / u64::MAX as f64;
    std::f64::consts::TAU * unit.fract()
}

use framework::{IntervalSet, Point, line_segment_intersection, ring_edges};
use rayon::prelude::*;

use super::near_threshold::near_threshold;
use super::{CandidateContext, CandidateSource};
use crate::antennas::snap_candidate_to_valid;
use crate::sampling::{Candidate, CandidateKind};

const MERGE_EPS: f64 = 1e-9;

pub struct ThresholdBreakpoints {
    progress_window: (f64, f64),
    viewers_per_building: usize,
    edge_budget: usize,
    emit_wall_crossings: bool,
}

impl ThresholdBreakpoints {
    pub fn new() -> Self {
        Self {
            progress_window: (0.0, 1.0),
            viewers_per_building: 4,
            edge_budget: 32,
            emit_wall_crossings: false,
        }
    }

    pub fn with_progress_window(mut self, low: f64, high: f64) -> Self {
        assert!(low < high, "progress window must be non-empty");
        assert!(
            high <= 1.0,
            "a fraction above 1.0 can never be near threshold"
        );
        self.progress_window = (low, high);
        self
    }

    pub fn with_viewers(mut self, viewers: usize) -> Self {
        self.viewers_per_building = viewers;
        self
    }

    pub fn with_edge_budget(mut self, edge_budget: usize) -> Self {
        self.edge_budget = edge_budget;
        self
    }

    pub fn with_wall_crossings(mut self, emit: bool) -> Self {
        self.emit_wall_crossings = emit;
        self
    }
}

impl Default for ThresholdBreakpoints {
    fn default() -> Self {
        Self::new()
    }
}

impl CandidateSource for ThresholdBreakpoints {
    fn name(&self) -> &'static str {
        "threshold breakpoints"
    }

    fn generate(&self, ctx: &CandidateContext<'_>) -> Vec<Candidate> {
        let limit = (ctx.incumbent.len() * 4).max(64);
        let Some(near) =
            near_threshold(ctx, self.progress_window, limit, self.viewers_per_building)
        else {
            return Vec::new();
        };

        near.buildings
            .par_iter()
            .flat_map_iter(|&(building, ref viewers)| {
                let need = near.goals.needs[building];
                let covered = &near.acc[building];
                self.candidates_for_building(ctx, building, need, covered, viewers)
            })
            .collect()
    }
}

impl ThresholdBreakpoints {
    fn candidates_for_building(
        &self,
        ctx: &CandidateContext<'_>,
        building: usize,
        need: f64,
        covered: &IntervalSet,
        viewers: &[usize],
    ) -> Vec<Candidate> {
        let remaining = need - covered.measure();
        if remaining <= 0.0 {
            return Vec::new();
        }
        let walls: Vec<(Point, Point)> =
            ring_edges(&ctx.instance.buildings[building].vertices).collect();
        let edges: Vec<(Point, Point, usize)> = viewers
            .iter()
            .filter(|&&viewer| viewer != building)
            .flat_map(|&viewer| {
                ring_edges(&ctx.instance.buildings[viewer].vertices)
                    .map(move |(a, b)| (a, b, viewer))
            })
            .take(self.edge_budget)
            .collect();

        edges
            .into_iter()
            .flat_map(|(e0, e1, viewer)| {
                self.candidates_for_edge(ctx, building, need, covered, &walls, e0, e1, viewer)
            })
            .collect()
    }

    #[allow(clippy::too_many_arguments)]
    fn candidates_for_edge(
        &self,
        ctx: &CandidateContext<'_>,
        building: usize,
        need: f64,
        covered: &IntervalSet,
        walls: &[(Point, Point)],
        e0: Point,
        e1: Point,
        viewer: usize,
    ) -> Vec<Candidate> {
        let mut breaks: Vec<f64> = walls
            .iter()
            .filter_map(|&(w0, w1)| line_segment_intersection(w0, w1, e0, e1))
            .collect();
        breaks.push(0.0);
        breaks.push(1.0);
        breaks.sort_unstable_by(f64::total_cmp);
        breaks.dedup_by(|a, b| (*a - *b).abs() <= MERGE_EPS);

        let gain_at = |t: f64| -> f64 {
            let point = e0 + (e1 - e0) * t;
            let view = ctx.evaluator.antenna_view(point).covered;
            let building = building as u32;
            view.iter()
                .find(|&&(b, _)| b == building)
                .map_or(covered.measure(), |(_, set)| covered.union_measure(set))
        };

        let mut out = Vec::new();
        let mut prev = None;
        for &t in &breaks {
            let gain = gain_at(t);
            if self.emit_wall_crossings && t > MERGE_EPS && t < 1.0 - MERGE_EPS {
                out.extend(breakpoint_candidate(ctx, e0, e1, t, viewer));
            }
            if let Some((t0, g0)) = prev
                && crosses(g0, gain, need)
            {
                let frac = ((need - g0) / (gain - g0)).clamp(0.0, 1.0);
                let t_star = t0 + frac * (t - t0);
                out.extend(breakpoint_candidate(ctx, e0, e1, t_star, viewer));
            }
            prev = Some((t, gain));
        }
        out
    }
}

fn crosses(a: f64, b: f64, need: f64) -> bool {
    (a < need) != (b < need)
}

fn breakpoint_candidate(
    ctx: &CandidateContext<'_>,
    e0: Point,
    e1: Point,
    t: f64,
    building: usize,
) -> Option<Candidate> {
    let position = e0 + (e1 - e0) * t;
    snap_candidate_to_valid(
        ctx.evaluator,
        Candidate {
            position,
            building: building as u32,
            kind: CandidateKind::Breakpoint,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::candidates::{CandidatePhase, CandidatePlan, UniformBoundary};
    use crate::coverage::CoverageData;
    use framework::test_utils::p;
    use framework::{Building, Evaluator, Instance};

    fn notch_scene() -> (Building, Building) {
        let target = Building {
            id: 0,
            vertices: vec![
                p(0.0, 0.0),
                p(10.0, 0.0),
                p(10.0, 10.0),
                p(0.0, 10.0),
                p(0.0, 7.0),
                p(7.0, 7.0),
                p(7.0, 3.0),
                p(0.0, 3.0),
            ],
        };
        let viewer = Building {
            id: 1,
            vertices: vec![
                p(-21.0, -10.0),
                p(-20.0, -10.0),
                p(-20.0, 20.0),
                p(-21.0, 20.0),
            ],
        };
        (target, viewer)
    }

    fn context_fixture(instance: &Instance) -> (Evaluator<'_>, CoverageData) {
        let evaluator = Evaluator::new(instance);
        let data = CandidatePlan::new()
            .with(CandidatePhase::Initial, UniformBoundary::new(100.0))
            .build(instance, None);
        (evaluator, data)
    }

    fn view_measure(evaluator: &Evaluator<'_>, point: Point) -> f64 {
        evaluator
            .antenna_view(point)
            .covered
            .into_iter()
            .find(|&(b, _)| b == 0)
            .map_or(0.0, |(_, set)| set.measure())
    }

    #[test]
    fn no_incumbent_is_a_no_op() {
        let (target, viewer) = notch_scene();
        let instance = Instance {
            buildings: vec![target, viewer],
            k: 2,
            tau: 0.1,
        };
        let (evaluator, data) = context_fixture(&instance);
        let ctx = CandidateContext {
            instance: &instance,
            evaluator: &evaluator,
            incumbent: &[],
            coverage: &data,
            seed: None,
        };
        assert!(ThresholdBreakpoints::new().generate(&ctx).is_empty());
    }

    #[test]
    fn wall_crossings_land_exactly_on_the_target_walls() {
        let (target, viewer) = notch_scene();
        let scan_instance = Instance {
            buildings: vec![target.clone(), viewer.clone()],
            k: 2,
            tau: 0.5,
        };
        let scan_evaluator = Evaluator::new(&scan_instance);
        let incumbent_pos = p(-20.0, -10.0);
        let need = view_measure(&scan_evaluator, incumbent_pos) + 1.0;
        let tau = need / scan_evaluator.perimeter(0);

        let instance = Instance {
            buildings: vec![target, viewer],
            k: 2,
            tau,
        };
        let (evaluator, data) = context_fixture(&instance);
        let incumbent = vec![incumbent_pos];
        let ctx = CandidateContext {
            instance: &instance,
            evaluator: &evaluator,
            incumbent: &incumbent,
            coverage: &data,
            seed: None,
        };
        let candidates = ThresholdBreakpoints::new()
            .with_wall_crossings(true)
            .generate(&ctx);
        for expected_y in [0.0, 3.0, 7.0, 10.0] {
            assert!(
                candidates.iter().any(|c| c.building == 1
                    && (c.position.y - expected_y).abs() < 1e-6
                    && (c.position.x - (-20.0)).abs() < 1e-3),
                "missing wall crossing at y={expected_y}: {candidates:?}"
            );
        }
    }

    #[test]
    fn threshold_crossing_is_found_within_its_own_brackets() {
        let (target, viewer) = notch_scene();
        let scan_instance = Instance {
            buildings: vec![target.clone(), viewer.clone()],
            k: 2,
            tau: 0.5,
        };
        let scan_evaluator = Evaluator::new(&scan_instance);
        let (e0, e1) = (p(-20.0, -10.0), p(-20.0, 20.0));
        let baseline = scan_evaluator
            .antenna_view(e0)
            .covered
            .into_iter()
            .find(|&(b, _)| b == 0)
            .map_or(IntervalSet::new(), |(_, set)| set);

        let ts = [0.0, 1.0 / 3.0, 13.0 / 30.0, 17.0 / 30.0, 2.0 / 3.0, 1.0];
        let gains: Vec<f64> = ts
            .iter()
            .map(|&t| {
                let point = e0 + (e1 - e0) * t;
                let view = scan_evaluator.antenna_view(point).covered;
                match view.into_iter().find(|&(b, _)| b == 0) {
                    Some((_, set)) => baseline.union_measure(&set),
                    None => baseline.measure(),
                }
            })
            .collect();
        let (min, max) = gains
            .iter()
            .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), &g| {
                (lo.min(g), hi.max(g))
            });
        assert!(
            max - min > 1.0,
            "scene has no real gain gradient across its own brackets: {gains:?}"
        );
        let need = (min + max) / 2.0;
        let tau = need / scan_evaluator.perimeter(0);

        let instance = Instance {
            buildings: vec![target, viewer],
            k: 2,
            tau,
        };
        let (evaluator, data) = context_fixture(&instance);
        let incumbent = vec![e0];
        let ctx = CandidateContext {
            instance: &instance,
            evaluator: &evaluator,
            incumbent: &incumbent,
            coverage: &data,
            seed: None,
        };
        let candidates = ThresholdBreakpoints::new().generate(&ctx);
        assert!(
            !candidates.is_empty(),
            "expected at least one threshold candidate"
        );

        let reaches_threshold = candidates.iter().any(|c| {
            baseline.union_measure(
                &evaluator
                    .antenna_view(c.position)
                    .covered
                    .into_iter()
                    .find(|&(b, _)| b == 0)
                    .map_or(IntervalSet::new(), |(_, set)| set),
            ) >= need - 1e-6
        });
        assert!(
            reaches_threshold,
            "no emitted candidate reaches the threshold: {candidates:?}"
        );
    }
}

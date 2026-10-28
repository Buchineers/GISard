use framework::{Evaluator, Instance, IntervalSet, Point};
use rayon::prelude::*;

use crate::antennas::ANTENNA_SNAP_DIST;
use crate::candidates::{CandidatePhase, CandidatePlan, UniformBoundary, VisibilityEvents};
use crate::sampling::{Candidate, CandidateKind};

#[derive(Clone, Debug)]
pub struct CoverageConfig {
    pub candidate_spacing: f64,
    pub event_candidate_radius: f64,
    pub event_candidate_limit: usize,
}

impl Default for CoverageConfig {
    fn default() -> Self {
        Self {
            candidate_spacing: 8.0,
            event_candidate_radius: 0.0,
            event_candidate_limit: 0,
        }
    }
}

impl CoverageConfig {
    pub fn plan(&self) -> CandidatePlan {
        let plan = CandidatePlan::new().with(
            CandidatePhase::Initial,
            UniformBoundary::new(self.candidate_spacing),
        );
        if self.event_candidate_radius > 0.0 {
            plan.with(
                CandidatePhase::Initial,
                VisibilityEvents::new(self.event_candidate_radius, self.event_candidate_limit),
            )
        } else {
            plan
        }
    }
}

pub struct CoverageData {
    pub candidates: Vec<Candidate>,
    pub visibility: Vec<Vec<(u32, IntervalSet)>>,
    pub by_building: Vec<Vec<(usize, f64)>>,
    pub perimeters: Vec<f64>,
    pub flat: FlatMeasures,
}

#[derive(Default)]
pub struct FlatMeasures {
    offsets: Vec<u32>,
    building: Vec<u32>,
    measure: Vec<f64>,
}

impl FlatMeasures {
    fn with_capacity(candidates: usize) -> Self {
        let mut offsets = Vec::with_capacity(candidates + 1);
        offsets.push(0);
        Self {
            offsets,
            building: Vec::new(),
            measure: Vec::new(),
        }
    }

    fn push_view(&mut self, view: &[(u32, IntervalSet)]) {
        for (building, set) in view {
            self.building.push(*building);
            self.measure.push(set.measure());
        }
        self.offsets.push(self.building.len() as u32);
    }

    pub fn entries(&self, candidate: usize) -> (&[u32], &[f64]) {
        let lo = self.offsets[candidate] as usize;
        let hi = self.offsets[candidate + 1] as usize;
        (&self.building[lo..hi], &self.measure[lo..hi])
    }

    pub fn candidate_count(&self) -> usize {
        self.offsets.len() - 1
    }
}

impl CoverageData {
    pub fn empty(instance: &Instance, evaluator: &Evaluator<'_>) -> Self {
        let perimeters = (0..instance.buildings.len())
            .map(|b| evaluator.perimeter(b))
            .collect();
        Self::new(Vec::new(), Vec::new(), perimeters)
    }

    pub fn new(
        candidates: Vec<Candidate>,
        visibility: Vec<Vec<(u32, IntervalSet)>>,
        perimeters: Vec<f64>,
    ) -> Self {
        let mut by_building = vec![Vec::new(); perimeters.len()];
        let mut flat = FlatMeasures::with_capacity(candidates.len());
        for (candidate, view) in visibility.iter().enumerate() {
            for (b, set) in view {
                by_building[*b as usize].push((candidate, set.measure()));
            }
            flat.push_view(view);
        }
        Self {
            candidates,
            visibility,
            by_building,
            perimeters,
            flat,
        }
    }

    pub fn estimated_bytes(&self) -> usize {
        use std::mem::size_of;

        let candidates = self.candidates.capacity() * size_of::<Candidate>();
        let visibility_outer = self.visibility.capacity() * size_of::<Vec<(u32, IntervalSet)>>();
        let visibility_rows: usize = self
            .visibility
            .iter()
            .map(|row| {
                row.capacity() * size_of::<(u32, IntervalSet)>()
                    + row
                        .iter()
                        .map(|(_, set)| std::mem::size_of_val(set.spans()))
                        .sum::<usize>()
            })
            .sum();
        let by_building_outer = self.by_building.capacity() * size_of::<Vec<(usize, f64)>>();
        let by_building_rows: usize = self
            .by_building
            .iter()
            .map(|row| row.capacity() * size_of::<(usize, f64)>())
            .sum();
        let flat = self.flat.offsets.capacity() * size_of::<u32>()
            + self.flat.building.capacity() * size_of::<u32>()
            + self.flat.measure.capacity() * size_of::<f64>();
        size_of::<Self>()
            + candidates
            + visibility_outer
            + visibility_rows
            + by_building_outer
            + by_building_rows
            + self.perimeters.capacity() * size_of::<f64>()
            + flat
    }
}

pub fn build_coverage(instance: &Instance, config: &CoverageConfig) -> CoverageData {
    config.plan().build(instance, None)
}

const DOMINANCE_COMPARISONS: usize = 16;
const DOMINANCE_EPS: f64 = 1e-9;

pub fn prune_dominated(data: CoverageData, keep_at_least: usize) -> CoverageData {
    if data.candidates.len() <= keep_at_least.max(2) {
        return data;
    }
    let flat = &data.flat;
    let mut order: Vec<(u64, f64, usize)> = (0..data.candidates.len())
        .into_par_iter()
        .map(|candidate| {
            let (buildings, measures) = flat.entries(candidate);
            (signature(buildings), measures.iter().sum(), candidate)
        })
        .collect();
    order.par_sort_unstable_by(|a, b| a.0.cmp(&b.0).then(b.1.total_cmp(&a.1)).then(a.2.cmp(&b.2)));

    let mut keep = vec![true; data.candidates.len()];
    let mut dropped = 0usize;
    let mut survivors: Vec<usize> = Vec::new();
    for group in order.chunk_by(|a, b| a.0 == b.0) {
        survivors.clear();
        for &(_, _, candidate) in group {
            let dominated = survivors
                .iter()
                .rev()
                .take(DOMINANCE_COMPARISONS)
                .any(|&sup| {
                    measures_allow(flat.entries(sup), flat.entries(candidate))
                        && contains(&data.visibility[sup], &data.visibility[candidate])
                });
            if dominated {
                keep[candidate] = false;
                dropped += 1;
            } else {
                survivors.push(candidate);
            }
        }
    }
    if dropped == 0 {
        return data;
    }
    let mut kept = data.candidates.len() - dropped;
    for flag in keep.iter_mut().filter(|flag| !**flag) {
        if kept >= keep_at_least {
            break;
        }
        *flag = true;
        kept += 1;
    }

    let CoverageData {
        candidates,
        visibility,
        perimeters,
        ..
    } = data;
    let mut kept_candidates = Vec::with_capacity(kept);
    let mut kept_visibility = Vec::with_capacity(kept);
    for (candidate, (position, view)) in candidates.into_iter().zip(visibility).enumerate() {
        if keep[candidate] {
            kept_candidates.push(position);
            kept_visibility.push(view);
        }
    }
    CoverageData::new(kept_candidates, kept_visibility, perimeters)
}

fn signature(buildings: &[u32]) -> u64 {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    buildings.hash(&mut hasher);
    hasher.finish()
}

fn measures_allow(sup: (&[u32], &[f64]), sub: (&[u32], &[f64])) -> bool {
    sup.0 == sub.0
        && sup
            .1
            .iter()
            .zip(sub.1)
            .all(|(&sup, &sub)| sub <= sup + DOMINANCE_EPS)
}

fn contains(sup: &[(u32, IntervalSet)], sub: &[(u32, IntervalSet)]) -> bool {
    sup.len() == sub.len()
        && sup
            .iter()
            .zip(sub)
            .all(|((sup_b, sup_set), (sub_b, sub_set))| {
                sup_b == sub_b
                    && sup_set.union_measure(sub_set) <= sup_set.measure() + DOMINANCE_EPS
            })
}

pub fn extend_coverage(instance: &Instance, data: &mut CoverageData, candidates: Vec<Candidate>) {
    if candidates.is_empty() {
        return;
    }
    extend_coverage_with(&Evaluator::new(instance), data, candidates);
}

pub fn extend_coverage_with(
    evaluator: &Evaluator<'_>,
    data: &mut CoverageData,
    candidates: Vec<Candidate>,
) {
    if candidates.is_empty() {
        return;
    }
    let first = data.candidates.len();
    let visibility = candidates
        .par_iter()
        .map(|candidate| evaluator.antenna_view(candidate.position).covered)
        .collect::<Vec<_>>();
    data.candidates.extend(candidates);
    for (offset, view) in visibility.iter().enumerate() {
        let candidate = first + offset;
        for (b, set) in view {
            data.by_building[*b as usize].push((candidate, set.measure()));
        }
        data.flat.push_view(view);
    }
    data.visibility.extend(visibility);
}

pub fn adopt_positions(
    evaluator: &Evaluator<'_>,
    data: &mut CoverageData,
    positions: &[Point],
    kind: CandidateKind,
) -> Vec<usize> {
    let first = data.candidates.len();
    let candidates = positions
        .iter()
        .map(|&position| Candidate {
            position,
            building: evaluator
                .snap(position, ANTENNA_SNAP_DIST)
                .map_or(0, |(building, _, _)| building as u32),
            kind,
        })
        .collect();
    extend_coverage_with(evaluator, data, candidates);
    (first..first + positions.len()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use framework::test_utils::{p, square};

    fn vertices_only() -> CoverageConfig {
        CoverageConfig {
            candidate_spacing: 100.0,
            event_candidate_radius: 0.0,
            event_candidate_limit: 0,
        }
    }

    #[test]
    fn candidate_sees_own_and_facing_building() {
        let instance = Instance {
            buildings: vec![square(0, 0.0, 0.0), square(1, 20.0, 0.0)],
            k: 1,
            tau: 0.5,
        };
        let data = build_coverage(&instance, &vertices_only());
        let i = data
            .candidates
            .iter()
            .position(|c| c.position == p(10.0, 0.0))
            .unwrap();
        let vis = &data.visibility[i];
        let own = vis.iter().find(|(b, _)| *b == 0).unwrap();
        let facing = vis.iter().find(|(b, _)| *b == 1).unwrap();
        assert_eq!(own.1.measure() / data.perimeters[0], 0.5);
        assert!(facing.1.measure() > 0.0);
    }

    #[test]
    fn matches_evaluator_per_candidate() {
        let instance = Instance {
            buildings: vec![square(0, 0.0, 0.0), square(1, 20.0, 0.0)],
            k: 1,
            tau: 0.5,
        };
        let data = build_coverage(&instance, &vertices_only());
        let evaluator = Evaluator::new(&instance);
        for (c, vis) in data.candidates.iter().zip(&data.visibility) {
            let eval = evaluator.evaluate(&[c.position]);
            for (bi, set) in vis {
                let fraction = set.measure() / data.perimeters[*bi as usize];
                assert_eq!(
                    fraction, eval.fractions[*bi as usize],
                    "candidate {:?}",
                    c.position
                );
            }
        }
    }

    #[test]
    fn empty_instance() {
        let instance = Instance {
            buildings: vec![],
            k: 1,
            tau: 0.5,
        };
        let data = build_coverage(&instance, &CoverageConfig::default());
        assert!(data.candidates.is_empty());
        assert!(data.visibility.is_empty());
        assert!(data.by_building.is_empty());
    }

    fn two_squares() -> Instance {
        Instance {
            buildings: vec![square(0, 0.0, 0.0), square(1, 20.0, 0.0)],
            k: 1,
            tau: 0.5,
        }
    }

    #[test]
    fn pruning_drops_exact_copies() {
        let instance = two_squares();
        let mut data = build_coverage(&instance, &vertices_only());
        let alone = prune_dominated(build_coverage(&instance, &vertices_only()), 1)
            .candidates
            .len();
        let copies = data.candidates.clone();
        extend_coverage(&instance, &mut data, copies);

        let pruned = prune_dominated(data, 1);
        assert_eq!(pruned.candidates.len(), alone);
        for (i, candidate) in pruned.candidates.iter().enumerate() {
            assert!(
                !pruned.candidates[i + 1..]
                    .iter()
                    .any(|other| other.position == candidate.position)
            );
        }
    }

    #[test]
    fn pruning_keeps_every_building_reachable() {
        let instance = two_squares();
        let data = build_coverage(&instance, &CoverageConfig::default());
        let reachable: Vec<bool> = data
            .by_building
            .iter()
            .map(|viewers| !viewers.is_empty())
            .collect();
        let best: Vec<f64> = data
            .by_building
            .iter()
            .map(|viewers| viewers.iter().map(|&(_, m)| m).fold(0.0, f64::max))
            .collect();

        let pruned = prune_dominated(data, 1);
        for (building, (&was_reachable, &was_best)) in reachable.iter().zip(&best).enumerate() {
            let viewers = &pruned.by_building[building];
            assert_eq!(!viewers.is_empty(), was_reachable, "building {building}");
            let now_best = viewers.iter().map(|&(_, m)| m).fold(0.0, f64::max);
            assert!(now_best >= was_best - 1e-9, "building {building}");
        }
    }

    #[test]
    fn pruning_never_goes_below_the_budget() {
        let instance = two_squares();
        let mut data = build_coverage(&instance, &vertices_only());
        let copies = data.candidates.clone();
        let total = 2 * data.candidates.len();
        extend_coverage(&instance, &mut data, copies);

        let pruned = prune_dominated(data, total);
        assert_eq!(pruned.candidates.len(), total);
    }

    #[test]
    fn extend_coverage_updates_inverted_index() {
        let instance = Instance {
            buildings: vec![square(0, 0.0, 0.0), square(1, 20.0, 0.0)],
            k: 1,
            tau: 0.5,
        };
        let mut data = build_coverage(
            &instance,
            &CoverageConfig {
                candidate_spacing: 100.0,
                event_candidate_radius: 0.0,
                event_candidate_limit: 0,
            },
        );
        let first = data.candidates.len();
        extend_coverage(
            &instance,
            &mut data,
            vec![Candidate {
                position: p(10.0, 0.0),
                building: 0,
                kind: crate::sampling::CandidateKind::Uniform,
            }],
        );
        assert!(data.by_building.iter().any(|entries| {
            entries
                .iter()
                .any(|&(candidate, measure)| candidate == first && measure > 0.0)
        }));
    }
}

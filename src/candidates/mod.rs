
mod boundary;
mod breakpoints;
mod event;
mod local;
mod near_threshold;
mod probe;

pub use boundary::UniformBoundary;
pub use breakpoints::ThresholdBreakpoints;
pub use event::VisibilityEvents;
pub use local::LocalBoundary;
pub use probe::ProbeRings;
pub(crate) use probe::seed_phase;

use framework::{Evaluator, Instance, Point};

use crate::coverage::{CoverageData, extend_coverage_with};
use crate::sampling::{Candidate, candidate_key, dedup_candidates};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CandidatePhase {
    Initial,
    Seeded,
    Polish,
}

pub struct CandidateContext<'a> {
    pub instance: &'a Instance,
    pub evaluator: &'a Evaluator<'a>,
    pub incumbent: &'a [Point],
    pub coverage: &'a CoverageData,
    pub seed: Option<u64>,
}

pub trait CandidateSource: Send + Sync {
    fn name(&self) -> &'static str;

    fn generate(&self, ctx: &CandidateContext<'_>) -> Vec<Candidate>;
}

#[derive(Default)]
pub struct CandidatePlan {
    stages: Vec<(CandidatePhase, Box<dyn CandidateSource>)>,
}

impl CandidatePlan {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(mut self, phase: CandidatePhase, source: impl CandidateSource + 'static) -> Self {
        self.stages.push((phase, Box::new(source)));
        self
    }

    pub fn has(&self, phase: CandidatePhase) -> bool {
        self.stages.iter().any(|(p, _)| *p == phase)
    }

    pub fn sources(&self, phase: CandidatePhase) -> impl Iterator<Item = &'static str> {
        self.stages
            .iter()
            .filter(move |(p, _)| *p == phase)
            .map(|(_, source)| source.name())
    }

    pub fn build(&self, instance: &Instance, seed: Option<u64>) -> CoverageData {
        let evaluator = Evaluator::new(instance);
        let mut data = CoverageData::empty(instance, &evaluator);
        self.extend_with(&evaluator, CandidatePhase::Initial, &mut data, &[], seed);
        data
    }

    pub fn extend(
        &self,
        phase: CandidatePhase,
        instance: &Instance,
        data: &mut CoverageData,
        incumbent: &[Point],
        seed: Option<u64>,
    ) -> usize {
        if !self.has(phase) {
            return 0;
        }
        let evaluator = Evaluator::new(instance);
        self.extend_with(&evaluator, phase, data, incumbent, seed)
    }

    pub fn extend_with(
        &self,
        evaluator: &Evaluator<'_>,
        phase: CandidatePhase,
        data: &mut CoverageData,
        incumbent: &[Point],
        seed: Option<u64>,
    ) -> usize {
        let mut generated = Vec::new();
        for (_, source) in self.stages.iter().filter(|(p, _)| *p == phase) {
            let ctx = CandidateContext {
                instance: evaluator.instance(),
                evaluator,
                incumbent,
                coverage: data,
                seed,
            };
            let candidates = source.generate(&ctx);
            eprintln!(
                "candidates: {} generated {} ({phase:?})",
                source.name(),
                candidates.len(),
            );
            generated.extend(candidates);
        }
        let existing: std::collections::HashSet<_> = data
            .candidates
            .iter()
            .map(|candidate| candidate_key(candidate.position))
            .collect();
        let candidates = dedup_candidates(generated)
            .into_iter()
            .filter(|candidate| !existing.contains(&candidate_key(candidate.position)))
            .collect::<Vec<_>>();
        let added = candidates.len();
        extend_coverage_with(evaluator, data, candidates);
        added
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use framework::test_utils::square;
    use framework::{CoverageGoals, Instance};

    fn two_squares() -> Instance {
        Instance {
            buildings: vec![square(0, 0.0, 0.0), square(1, 40.0, 0.0)],
            k: 2,
            tau: 0.5,
        }
    }

    #[test]
    fn initial_phase_fills_an_empty_pool() {
        let instance = two_squares();
        let plan = CandidatePlan::new().with(CandidatePhase::Initial, UniformBoundary::new(100.0));
        let data = plan.build(&instance, None);
        assert_eq!(data.candidates.len(), 8, "four corners per square");
        assert_eq!(data.visibility.len(), data.candidates.len());
        assert_eq!(data.perimeters.len(), 2);
    }

    #[test]
    fn overlapping_sources_are_deduplicated_within_a_phase() {
        let instance = two_squares();
        let once = CandidatePlan::new()
            .with(CandidatePhase::Initial, UniformBoundary::new(100.0))
            .build(&instance, None);
        let twice = CandidatePlan::new()
            .with(CandidatePhase::Initial, UniformBoundary::new(100.0))
            .with(CandidatePhase::Initial, UniformBoundary::new(100.0))
            .build(&instance, None);
        assert_eq!(once.candidates.len(), twice.candidates.len());
    }

    #[test]
    fn later_phases_do_not_reappend_existing_candidates() {
        let instance = two_squares();
        let plan = CandidatePlan::new()
            .with(CandidatePhase::Initial, UniformBoundary::new(100.0))
            .with(CandidatePhase::Polish, UniformBoundary::new(100.0));
        let mut data = plan.build(&instance, None);
        let before = data.candidates.len();
        assert_eq!(
            plan.extend(CandidatePhase::Polish, &instance, &mut data, &[], None),
            0
        );
        assert_eq!(data.candidates.len(), before);
    }

    #[test]
    fn later_phases_only_grow_the_pool() {
        let instance = two_squares();
        let plan = CandidatePlan::new()
            .with(CandidatePhase::Initial, UniformBoundary::new(100.0))
            .with(CandidatePhase::Polish, ProbeRings::new(2, 4));
        let mut data = plan.build(&instance, None);
        let before = data.candidates.len();
        let incumbent = vec![data.candidates[0].position];
        let added = plan.extend(
            CandidatePhase::Polish,
            &instance,
            &mut data,
            &incumbent,
            None,
        );
        assert!(added > 0);
        assert_eq!(data.candidates.len(), before + added);
        assert_eq!(data.visibility.len(), data.candidates.len());
        assert_eq!(data.flat.candidate_count(), data.candidates.len());
        let goals = CoverageGoals::new(&data.perimeters, instance.tau);
        assert_eq!(goals.needs.len(), data.by_building.len());
    }

    #[test]
    fn absent_phase_is_a_no_op() {
        let instance = two_squares();
        let plan = CandidatePlan::new().with(CandidatePhase::Initial, UniformBoundary::new(100.0));
        let mut data = plan.build(&instance, None);
        let before = data.candidates.len();
        assert!(!plan.has(CandidatePhase::Seeded));
        assert_eq!(
            plan.extend(CandidatePhase::Seeded, &instance, &mut data, &[], None),
            0
        );
        assert_eq!(data.candidates.len(), before);
    }
}

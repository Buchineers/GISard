use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use framework::{CoverageGoals, Evaluator, Instance, IntervalSet, Point, Solution, Solver, bbox};
use rand::rngs::StdRng;
use rand::{RngExt, SeedableRng};
use rayon::prelude::*;

use crate::antennas::{project_to_valid_antenna, require_valid_antennas};
use crate::candidates::{
    CandidatePhase, CandidatePlan, LocalBoundary, ProbeRings, ThresholdBreakpoints,
    VisibilityEvents, seed_phase,
};
use crate::coverage::{CoverageConfig, CoverageData, adopt_positions};
use crate::sampling::CandidateKind;
use crate::solvers::greedy_coverage::{select_coverability_indices, select_indices};
use crate::solvers::swap_refine::{
    Accumulation, Fitness, RepairBound, accumulated, accumulated_fitness, refine,
};

const EPS: f64 = 1e-9;

#[derive(Clone, Debug)]
pub struct LnsOptions {
    pub lns_iterations: usize,
    pub lns_destroy: usize,
    pub lns_rcl: usize,
    pub lns_repair_rounds: usize,
    pub lns_restarts: usize,
    pub exhaustive_repair_rounds: usize,
    pub pair_repair_rounds: usize,
    pub pair_removals: usize,
    pub pair_candidates: usize,
    pub recombine_rounds: usize,
    pub local_spacing: f64,
    pub threshold_breakpoints: bool,
    pub free_polish_steps: usize,
    pub event_radius: f64,
    pub event_partners: usize,
    pub seed_strategy: LnsSeedStrategy,
    pub coverability_alpha: f64,
    pub seed: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default)]
pub enum LnsSeedStrategy {
    Greedy,
    Coverability,
    Threshold,
    Direct,
    #[default]
    BestOfAll,
}

impl Default for LnsOptions {
    fn default() -> Self {
        Self {
            lns_iterations: 100,
            lns_destroy: 5,
            lns_rcl: 12,
            lns_repair_rounds: 3,
            lns_restarts: 4,
            exhaustive_repair_rounds: 5,
            pair_repair_rounds: 3,
            pair_removals: 12,
            pair_candidates: 64,
            recombine_rounds: 3,
            local_spacing: 0.0,
            threshold_breakpoints: false,
            free_polish_steps: 0,
            event_radius: 0.0,
            event_partners: 32,
            seed_strategy: LnsSeedStrategy::BestOfAll,
            coverability_alpha: 1.0,
            seed: None,
        }
    }
}

#[derive(Default)]
pub struct LnsSolver {
    pub coverage: CoverageConfig,
    pub options: LnsOptions,
    pub initial_antennas: Option<Vec<Point>>,
    plan: Option<CandidatePlan>,
    progress: Option<Arc<dyn Fn(LnsProgress) + Send + Sync>>,
}

#[derive(Clone, Debug)]
pub struct LnsProgress {
    pub stage: String,
    pub restart: usize,
    pub iteration: usize,
    pub total_iterations: usize,
    pub serviced: usize,
    pub progress: f64,
    pub coverage_fraction: f64,
    pub antennas: Vec<Point>,
    pub added: Vec<Point>,
    pub removed: Vec<Point>,
    pub coverage: Vec<LnsCoverage>,
}

#[derive(Clone, Debug)]
pub struct LnsCoverage {
    pub building: usize,
    pub progress: f64,
}

impl LnsSolver {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_coverage_config(mut self, coverage: CoverageConfig) -> Self {
        self.coverage = coverage;
        self
    }

    pub fn with_options(mut self, options: LnsOptions) -> Self {
        self.options = options;
        self
    }

    pub fn with_initial_antennas(mut self, antennas: Vec<Point>) -> Self {
        self.initial_antennas = Some(antennas);
        self
    }

    pub fn with_candidate_plan(mut self, plan: CandidatePlan) -> Self {
        self.plan = Some(plan);
        self
    }

    fn implied_plan(&self) -> CandidatePlan {
        let mut plan = self.coverage.plan();
        if self.options.event_radius > 0.0 {
            plan = plan.with(
                CandidatePhase::Initial,
                VisibilityEvents::new(self.options.event_radius, self.options.event_partners),
            );
        }
        if self.options.local_spacing > 0.0 {
            plan = plan.with(
                CandidatePhase::Seeded,
                LocalBoundary::new(self.options.local_spacing),
            );
        }
        if self.options.threshold_breakpoints {
            plan = plan
                .with(CandidatePhase::Seeded, ThresholdBreakpoints::new())
                .with(CandidatePhase::Polish, ThresholdBreakpoints::new());
        }
        if self.probes_wanted() {
            plan = plan.with(CandidatePhase::Polish, ProbeRings::new(10, 16));
        }
        plan
    }

    fn probes_wanted(&self) -> bool {
        self.options.free_polish_steps > 0
            && (self.options.exhaustive_repair_rounds > 0 || self.options.pair_repair_rounds > 0)
    }

    pub fn with_progress_callback(
        mut self,
        progress: impl Fn(LnsProgress) + Send + Sync + 'static,
    ) -> Self {
        self.progress = Some(Arc::new(progress));
        self
    }
}

impl Solver for LnsSolver {
    fn solve(&self, instance: Instance) -> Solution {
        let implied;
        let plan = match &self.plan {
            Some(plan) => plan,
            None => {
                implied = self.implied_plan();
                &implied
            }
        };
        let evaluator = Evaluator::new(&instance);
        let mut data = CoverageData::empty(&instance, &evaluator);
        plan.extend_with(
            &evaluator,
            CandidatePhase::Initial,
            &mut data,
            &[],
            self.options.seed,
        );

        let loaded_incumbent = self.initial_antennas.is_some();
        let mut selected = if let Some(antennas) = &self.initial_antennas {
            let antennas = require_valid_antennas(&instance, antennas.clone());
            assert_eq!(
                antennas.len(),
                instance.k,
                "initial solution must contain exactly k antennas"
            );
            eprintln!("loaded {} incumbent antennas", antennas.len());
            let adopted =
                adopt_positions(&evaluator, &mut data, &antennas, CandidateKind::Incumbent);
            polish_selected(&data, instance.tau, adopted, &self.options)
        } else {
            select_lns_indices(
                &data,
                instance.k,
                instance.tau,
                &self.options,
                self.progress.as_ref(),
            )
        };

        let searching = loaded_incumbent && self.options.lns_iterations > 0;
        let mut seeded = false;
        if searching && !selected.is_empty() {
            self.extend_seeded(plan, &evaluator, &mut data, &selected);
            seeded = true;
        }
        if searching {
            let goals = CoverageGoals::new(&data.perimeters, instance.tau);
            let mut state = PlacementState::new(&data, &goals, selected);
            let mut rng =
                StdRng::seed_from_u64(self.options.seed.unwrap_or_else(|| rand::rng().random()));
            threshold_repair(&data, &goals, &mut state, &self.options, &mut rng);
            large_neighborhood_search(
                &data,
                &goals,
                &mut state,
                &self.options,
                &mut rng,
                0,
                self.progress.as_ref(),
                None,
            );
            selected = state.selected;
        }
        if plan.has(CandidatePhase::Seeded) && !selected.is_empty() {
            if !seeded {
                self.extend_seeded(plan, &evaluator, &mut data, &selected);
            }
            selected = polish_selected(&data, instance.tau, selected, &self.options);
        }

        let antennas = selected
            .iter()
            .map(|&candidate| data.candidates[candidate].position)
            .collect::<Vec<_>>();
        let antennas = free_space_polish(
            &instance,
            antennas,
            self.options.free_polish_steps,
            self.options.seed,
        );
        let antennas = if plan.has(CandidatePhase::Polish) {
            let antennas = probe_repair(
                plan,
                &evaluator,
                &mut data,
                antennas,
                instance.tau,
                &self.options,
            );
            free_space_polish(
                &instance,
                antennas,
                self.options.free_polish_steps,
                self.options.seed.map(|seed| seed.wrapping_add(1)),
            )
        } else {
            antennas
        };
        Solution {
            antennas: require_valid_antennas(&instance, antennas),
        }
    }
}

impl LnsSolver {
    fn extend_seeded(
        &self,
        plan: &CandidatePlan,
        evaluator: &Evaluator<'_>,
        data: &mut CoverageData,
        selected: &[usize],
    ) {
        let incumbent: Vec<_> = selected
            .iter()
            .map(|&candidate| data.candidates[candidate].position)
            .collect();
        plan.extend_with(
            evaluator,
            CandidatePhase::Seeded,
            data,
            &incumbent,
            self.options.seed,
        );
    }
}

pub fn select_lns_antennas(
    data: &CoverageData,
    k: usize,
    tau: f64,
    options: &LnsOptions,
) -> Vec<Point> {
    select_lns_antennas_with_progress(data, k, tau, options, None)
}

pub fn select_lns_antennas_with_progress(
    data: &CoverageData,
    k: usize,
    tau: f64,
    options: &LnsOptions,
    progress: Option<Arc<dyn Fn(LnsProgress) + Send + Sync>>,
) -> Vec<Point> {
    select_lns_indices(data, k, tau, options, progress.as_ref())
        .iter()
        .map(|&i| data.candidates[i].position)
        .collect()
}

pub fn select_lns_antennas_resuming(
    data: &CoverageData,
    k: usize,
    tau: f64,
    options: &LnsOptions,
    resume: &[Point],
    progress: Option<Arc<dyn Fn(LnsProgress) + Send + Sync>>,
) -> Vec<Point> {
    select_lns_indices_from(data, k, tau, options, Some(resume), progress.as_ref(), None)
        .iter()
        .map(|&i| data.candidates[i].position)
        .collect()
}

pub fn select_lns_antennas_until(
    data: &CoverageData,
    k: usize,
    tau: f64,
    options: &LnsOptions,
    resume: Option<&[Point]>,
    deadline: Instant,
) -> Vec<Point> {
    select_lns_indices_from(data, k, tau, options, resume, None, Some(deadline))
        .iter()
        .map(|&i| data.candidates[i].position)
        .collect()
}

pub(crate) fn resume_indices(data: &CoverageData, resume: &[Point]) -> Vec<usize> {
    let mut exact: HashMap<(u64, u64), usize> = HashMap::new();
    for (i, c) in data.candidates.iter().enumerate() {
        exact
            .entry((c.position.x.to_bits(), c.position.y.to_bits()))
            .or_insert(i);
    }
    let mut used = vec![false; data.candidates.len()];
    let mut out = Vec::with_capacity(resume.len());
    for p in resume {
        let hit = exact
            .get(&(p.x.to_bits(), p.y.to_bits()))
            .copied()
            .or_else(|| {
                data.candidates
                    .iter()
                    .enumerate()
                    .min_by(|a, b| {
                        (a.1.position - *p)
                            .norm_sq()
                            .total_cmp(&(b.1.position - *p).norm_sq())
                    })
                    .map(|(i, _)| i)
            });
        if let Some(i) = hit
            && !used[i]
        {
            used[i] = true;
            out.push(i);
        }
    }
    out
}

fn select_lns_indices(
    data: &CoverageData,
    k: usize,
    tau: f64,
    options: &LnsOptions,
    progress: Option<&Arc<dyn Fn(LnsProgress) + Send + Sync>>,
) -> Vec<usize> {
    select_lns_indices_from(data, k, tau, options, None, progress, None)
}

fn select_lns_indices_from(
    data: &CoverageData,
    k: usize,
    tau: f64,
    options: &LnsOptions,
    resume: Option<&[Point]>,
    progress: Option<&Arc<dyn Fn(LnsProgress) + Send + Sync>>,
    deadline: Option<Instant>,
) -> Vec<usize> {
    let goals = CoverageGoals::new(&data.perimeters, tau);
    let initial = match resume {
        Some(points) if !points.is_empty() => {
            let mut selected = resume_indices(data, points);
            selected.truncate(k);
            let mut state = PlacementState::new(data, &goals, selected);
            if state.selected.len() < k {
                fill_to_k(data, &goals, &mut state, k);
            }
            vec![state]
        }
        _ => initial_states(data, &goals, k, tau, options),
    };
    let base_seed = options.seed.unwrap_or_else(|| rand::rng().random());
    let restarts = options.lns_restarts.max(1);
    let mut results: Vec<_> = (0..restarts)
        .into_par_iter()
        .map(|restart| {
            let mut state = initial[restart % initial.len()].clone();
            let seed_round = restart / initial.len();
            let mut rng = StdRng::seed_from_u64(restart_seed(base_seed, seed_round));
            if !deadline_reached(deadline) {
                threshold_repair(data, &goals, &mut state, options, &mut rng);
            }
            emit_progress(
                data,
                &goals,
                &state,
                "seed",
                restart,
                0,
                options.lns_iterations,
                None,
                progress,
            );
            large_neighborhood_search(
                data, &goals, &mut state, options, &mut rng, restart, progress, deadline,
            );
            state
        })
        .collect();
    if !deadline_reached(deadline) {
        recombine_elites(data, &goals, k, &mut results, options.recombine_rounds);
    }
    let mut best = results
        .into_iter()
        .reduce(|best, state| {
            if state.metric.better_than(best.metric) {
                state
            } else {
                best
            }
        })
        .expect("at least one LNS restart");
    if !deadline_reached(deadline) {
        refine(
            data,
            &goals,
            &mut best.selected,
            options.exhaustive_repair_rounds,
            0,
        );
    }
    best.used = used_mask(data.candidates.len(), &best.selected);
    best.refresh(data, &goals);
    pair_repair(data, &goals, &mut best, options, progress, deadline);
    emit_progress(
        data,
        &goals,
        &best,
        "final",
        0,
        options.lns_iterations,
        options.lns_iterations,
        None,
        progress,
    );

    best.selected
}

fn fill_to_k(data: &CoverageData, goals: &CoverageGoals, state: &mut PlacementState, k: usize) {
    while state.selected.len() < k {
        let base = accumulated(data, goals, state.selected.iter().copied());
        let repairs = ranked_repairs(data, goals, &base, &state.used, 1);
        let Some(candidate) = repairs
            .first()
            .map(|r| r.candidate)
            .or_else(|| (0..data.candidates.len()).find(|&i| !state.used[i]))
        else {
            break;
        };
        state.add(data, goals, candidate);
    }
}

fn polish_selected(
    data: &CoverageData,
    tau: f64,
    selected: Vec<usize>,
    options: &LnsOptions,
) -> Vec<usize> {
    let goals = CoverageGoals::new(&data.perimeters, tau);
    let mut state = PlacementState::new(data, &goals, selected);
    refine(
        data,
        &goals,
        &mut state.selected,
        options.exhaustive_repair_rounds,
        0,
    );
    state.used = used_mask(data.candidates.len(), &state.selected);
    state.refresh(data, &goals);
    pair_repair(data, &goals, &mut state, options, None, None);
    state.selected
}

fn initial_states(
    data: &CoverageData,
    goals: &CoverageGoals,
    k: usize,
    tau: f64,
    options: &LnsOptions,
) -> Vec<PlacementState> {
    let greedy = || select_indices(data, goals, k).0;
    let coverability = || select_coverability_indices(data, k, tau, options.coverability_alpha).0;
    let threshold = || select_threshold_indices(data, goals, k);
    let direct = || select_direct_indices(data, goals, k);
    match options.seed_strategy {
        LnsSeedStrategy::Greedy => vec![PlacementState::new(data, goals, greedy())],
        LnsSeedStrategy::Coverability => {
            vec![PlacementState::new(data, goals, coverability())]
        }
        LnsSeedStrategy::Threshold => vec![PlacementState::new(data, goals, threshold())],
        LnsSeedStrategy::Direct => vec![PlacementState::new(data, goals, direct())],
        LnsSeedStrategy::BestOfAll => {
            let ((greedy, coverability), (threshold, direct)) = rayon::join(
                || rayon::join(greedy, coverability),
                || rayon::join(threshold, direct),
            );
            let mut states = vec![
                PlacementState::new(data, goals, greedy),
                PlacementState::new(data, goals, coverability),
                PlacementState::new(data, goals, threshold),
                PlacementState::new(data, goals, direct),
            ];
            states.sort_unstable_by(|a, b| metric_cmp(b.metric, a.metric));
            states
        }
    }
}

pub(super) fn seed_index_sets(
    data: &CoverageData,
    goals: &CoverageGoals,
    k: usize,
    tau: f64,
) -> Vec<Vec<usize>> {
    let mut seeds: Vec<Vec<usize>> = initial_states(data, goals, k, tau, &LnsOptions::default())
        .into_iter()
        .map(|state| state.selected)
        .collect();
    let mut seen: Vec<Vec<usize>> = Vec::with_capacity(seeds.len());
    seeds.retain(|selected| {
        let mut key = selected.clone();
        key.sort_unstable();
        let fresh = !seen.contains(&key);
        if fresh {
            seen.push(key);
        }
        fresh
    });
    seeds
}

fn select_threshold_indices(data: &CoverageData, goals: &CoverageGoals, k: usize) -> Vec<usize> {
    let pool: Vec<_> = (0..data.candidates.len()).collect();
    select_threshold_from_pool(data, goals, k, &pool)
}

fn select_threshold_from_pool(
    data: &CoverageData,
    goals: &CoverageGoals,
    k: usize,
    pool: &[usize],
) -> Vec<usize> {
    fill_threshold(data, goals, k, pool, Vec::new())
}

fn fill_threshold(
    data: &CoverageData,
    goals: &CoverageGoals,
    k: usize,
    pool: &[usize],
    mut selected: Vec<usize>,
) -> Vec<usize> {
    selected.truncate(k);
    let mut used = used_mask(data.candidates.len(), &selected);
    let mut base = accumulated(data, goals, selected.iter().copied());
    while selected.len() < k {
        let Some(candidate) = best_addition(data, goals, &base, pool, &used) else {
            break;
        };
        used[candidate] = true;
        selected.push(candidate);
        base.add(goals, &data.visibility[candidate]);
    }
    selected
}

fn best_addition(
    data: &CoverageData,
    goals: &CoverageGoals,
    base: &Accumulation,
    pool: &[usize],
    used: &[bool],
) -> Option<usize> {
    let remaining = base.remaining(goals);
    let lowest = || {
        pool.par_iter()
            .copied()
            .filter(|&candidate| !used[candidate])
            .min()
    };
    let mut bounds: Vec<_> = pool
        .par_iter()
        .copied()
        .filter(|&candidate| !used[candidate])
        .map(|candidate| {
            (
                RepairBound::of(data.flat.entries(candidate), &remaining),
                candidate,
            )
        })
        .filter(|&(bound, _)| bound.can_qualify())
        .collect();

    let shortlist = SHORTLIST_FACTOR.min(bounds.len());
    if shortlist < bounds.len() {
        bounds.select_nth_unstable_by(shortlist, |&a, &b| bound_cmp(a, b));
    }
    let Some(bar) = best_evaluated(data, goals, base, &bounds[..shortlist]) else {
        return lowest();
    };
    let contenders: Vec<_> = bounds
        .par_iter()
        .copied()
        .filter(|&(bound, _)| metric_cmp(bound.optimistic(base.fitness), bar.0) != Ordering::Less)
        .collect();
    let best = best_evaluated(data, goals, base, &contenders)?;
    best.0
        .better_than(base.fitness)
        .then_some(best.1)
        .or_else(lowest)
}

fn best_evaluated(
    data: &CoverageData,
    goals: &CoverageGoals,
    base: &Accumulation,
    entries: &[(RepairBound, usize)],
) -> Option<(Fitness, usize)> {
    entries
        .par_iter()
        .map(|&(_, candidate)| {
            (
                base.after_adding(goals, &data.visibility[candidate]),
                candidate,
            )
        })
        .reduce_with(
            |a, b| match metric_cmp(a.0, b.0).then_with(|| b.1.cmp(&a.1)) {
                Ordering::Less => b,
                _ => a,
            },
        )
}

fn recombine_elites(
    data: &CoverageData,
    goals: &CoverageGoals,
    k: usize,
    population: &mut Vec<PlacementState>,
    rounds: usize,
) {
    const MAX_ELITES: usize = 16;
    for round in 1..=rounds {
        let parent_count = population.len();
        let parent_pairs: Vec<_> = (0..parent_count)
            .flat_map(|a| ((a + 1)..parent_count).map(move |b| (a, b)))
            .collect();
        let mut children: Vec<_> = parent_pairs
            .into_par_iter()
            .map(|(a, b)| {
                let mut pool = population[a].selected.clone();
                pool.extend_from_slice(&population[b].selected);
                pool.sort_unstable();
                pool.dedup();
                PlacementState::new(
                    data,
                    goals,
                    select_threshold_from_pool(data, goals, k, &pool),
                )
            })
            .collect();
        let mut global_pool: Vec<_> = population
            .iter()
            .flat_map(|state| state.selected.iter().copied())
            .collect();
        global_pool.sort_unstable();
        global_pool.dedup();
        children.push(PlacementState::new(
            data,
            goals,
            select_threshold_from_pool(data, goals, k, &global_pool),
        ));
        let before = population
            .iter()
            .map(|state| state.metric)
            .max_by(|a, b| metric_cmp(*a, *b))
            .expect("nonempty elite population");
        population.append(&mut children);
        population.sort_unstable_by(|a, b| metric_cmp(b.metric, a.metric));
        population.truncate(MAX_ELITES);
        if population[0].metric.better_than(before) {
            eprintln!(
                "elite recombination {round}: accepted {} serviced, progress {:.1}",
                population[0].metric.serviced, population[0].metric.progress
            );
        } else {
            break;
        }
    }
}

fn select_direct_indices(data: &CoverageData, goals: &CoverageGoals, k: usize) -> Vec<usize> {
    let services = direct_services(data, goals);
    let (mut selected, mut counts, mut used) =
        direct_greedy(&services, goals.needs.len(), data.candidates.len(), k);
    direct_swap_refine(&services, &mut selected, &mut counts, &mut used);
    selected
}

fn direct_greedy(
    services: &[Vec<u32>],
    buildings: usize,
    candidate_count: usize,
    k: usize,
) -> (Vec<usize>, Vec<u16>, Vec<bool>) {
    let mut selected = Vec::with_capacity(k.min(candidate_count));
    let mut used = vec![false; candidate_count];
    let mut counts = vec![0u16; buildings];

    let mut servicers = vec![Vec::new(); buildings];
    for (candidate, serviced) in services.iter().enumerate() {
        for &building in serviced {
            servicers[building as usize].push(candidate as u32);
        }
    }
    let mut gains: Vec<u32> = services.iter().map(|s| s.len() as u32).collect();

    while selected.len() < k {
        let best = gains
            .par_iter()
            .enumerate()
            .filter(|&(candidate, _)| !used[candidate])
            .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(&a.0)));
        let Some((candidate, _)) = best else {
            break;
        };
        used[candidate] = true;
        selected.push(candidate);
        for &building in &services[candidate] {
            if counts[building as usize] == 0 {
                for &viewer in &servicers[building as usize] {
                    gains[viewer as usize] -= 1;
                }
            }
            counts[building as usize] += 1;
        }
    }

    (selected, counts, used)
}

fn free_space_polish(
    instance: &Instance,
    mut antennas: Vec<Point>,
    steps: usize,
    seed: Option<u64>,
) -> Vec<Point> {
    if steps == 0 || antennas.is_empty() {
        return antennas;
    }

    let evaluator = Evaluator::new(instance);
    let (min, max) = bbox(&instance.buildings);
    let search_radius = (max - min).norm().max(1.0) * 4.0;
    for antenna in &mut antennas {
        *antenna = project_to_valid_antenna(&evaluator, *antenna, search_radius)
            .expect("polish input contains an invalid antenna");
    }
    let perimeters: Vec<_> = (0..instance.buildings.len())
        .map(|building| evaluator.perimeter(building))
        .collect();
    let goals = CoverageGoals::new(&perimeters, instance.tau);
    let mut views: Vec<_> = antennas
        .iter()
        .map(|&antenna| evaluator.antenna_view(antenna).covered)
        .collect();
    let mut step = (max - min).norm().max(1.0) * 0.02;
    let phase = seed_phase(seed);
    let mut current = fitness_from_views(&goals, &views);

    for round in 1..=steps {
        let mut moved = 0;
        let round_phase = phase + round as f64 * 0.381_966_011_250_105_1;
        for antenna in 0..antennas.len() {
            let base = accumulated_views(
                &goals,
                views
                    .iter()
                    .enumerate()
                    .filter_map(|(index, view)| (index != antenna).then_some(view.as_slice())),
            );
            let mut best_point = antennas[antenna];
            let mut best_view = None;
            let mut best_fitness = current;
            for direction_index in 0..16 {
                let angle = round_phase + std::f64::consts::TAU * direction_index as f64 / 16.0;
                let direction = Point::new(angle.cos(), angle.sin());
                let Some(point) = project_to_valid_antenna(
                    &evaluator,
                    antennas[antenna] + direction * step,
                    search_radius,
                ) else {
                    continue;
                };
                let view = evaluator.antenna_view(point).covered;
                let fitness = base.after_view(&goals, &view);
                if fitness.better_than(best_fitness) {
                    best_point = point;
                    best_view = Some(view);
                    best_fitness = fitness;
                }
            }
            if let Some(view) = best_view {
                antennas[antenna] = best_point;
                views[antenna] = view;
                current = best_fitness;
                moved += 1;
            }
        }
        if moved > 0 {
            eprintln!(
                "free polish {round}: moved {moved}, {} serviced, progress {:.1}",
                current.serviced, current.progress
            );
        }
        step *= 0.5;
    }
    antennas
}

fn probe_repair(
    plan: &CandidatePlan,
    evaluator: &Evaluator<'_>,
    data: &mut CoverageData,
    mut antennas: Vec<Point>,
    tau: f64,
    options: &LnsOptions,
) -> Vec<Point> {
    let (min, max) = bbox(&evaluator.instance().buildings);
    let search_radius = (max - min).norm().max(1.0) * 4.0;
    for antenna in &mut antennas {
        *antenna = project_to_valid_antenna(evaluator, *antenna, search_radius)
            .expect("probe repair input contains an invalid antenna");
    }
    let adopted = adopt_positions(evaluator, data, &antennas, CandidateKind::Incumbent);
    plan.extend_with(
        evaluator,
        CandidatePhase::Polish,
        data,
        &antennas,
        options.seed,
    );
    polish_selected(data, tau, adopted, options)
        .iter()
        .map(|&candidate| data.candidates[candidate].position)
        .collect()
}

struct ViewAccumulation {
    acc: Vec<IntervalSet>,
    covered: Vec<f64>,
    serviced: Vec<bool>,
    fitness: Fitness,
}

impl ViewAccumulation {
    fn after_view(&self, goals: &CoverageGoals, view: &[(u32, IntervalSet)]) -> Fitness {
        let mut fitness = self.fitness;
        for (building, set) in view {
            let building = *building as usize;
            let covered = self.acc[building].union_measure(set);
            if !self.serviced[building]
                && goals.needs[building] > 0.0
                && covered >= goals.needs[building]
            {
                fitness.serviced += 1;
            }
            fitness.progress += goals.needs[building].min(covered)
                - goals.needs[building].min(self.covered[building]);
        }
        fitness
    }
}

fn accumulated_views<'a>(
    goals: &CoverageGoals,
    views: impl IntoIterator<Item = &'a [(u32, IntervalSet)]>,
) -> ViewAccumulation {
    let mut acc = vec![IntervalSet::new(); goals.needs.len()];
    for view in views {
        for (building, set) in view {
            acc[*building as usize].union_with(set);
        }
    }
    let covered: Vec<_> = acc.iter().map(IntervalSet::measure).collect();
    let serviced: Vec<_> = covered
        .iter()
        .zip(&goals.needs)
        .map(|(&covered, &need)| need > 0.0 && covered >= need)
        .collect();
    let fitness = Fitness {
        serviced: serviced.iter().filter(|&&value| value).count(),
        progress: goals.progress(&acc),
    };
    ViewAccumulation {
        acc,
        covered,
        serviced,
        fitness,
    }
}

fn fitness_from_views(goals: &CoverageGoals, views: &[Vec<(u32, IntervalSet)>]) -> Fitness {
    accumulated_views(goals, views.iter().map(Vec::as_slice)).fitness
}

fn direct_services(data: &CoverageData, goals: &CoverageGoals) -> Vec<Vec<u32>> {
    data.visibility
        .par_iter()
        .map(|view| {
            view.iter()
                .filter_map(|(building, set)| {
                    let building_index = *building as usize;
                    (goals.needs[building_index] > 0.0
                        && set.measure() + EPS >= goals.needs[building_index])
                        .then_some(*building)
                })
                .collect()
        })
        .collect()
}

fn direct_swap_refine(
    services: &[Vec<u32>],
    selected: &mut [usize],
    counts: &mut [u16],
    used: &mut [bool],
) {
    let mut score = counts.iter().filter(|&&count| count > 0).count();
    loop {
        let best = (0..selected.len())
            .into_par_iter()
            .filter_map(|remove_pos| {
                let removed = selected[remove_pos];
                let lost: Vec<_> = services[removed]
                    .iter()
                    .copied()
                    .filter(|&building| counts[building as usize] == 1)
                    .collect();
                let base_score = score - lost.len();
                let best_add = (0..services.len())
                    .filter(|&candidate| !used[candidate])
                    .map(|candidate| {
                        let gain = services[candidate]
                            .iter()
                            .filter(|&&building| {
                                counts[building as usize] == 0
                                    || lost.binary_search(&building).is_ok()
                            })
                            .count();
                        (base_score + gain, candidate)
                    })
                    .max_by(|a, b| a.0.cmp(&b.0).then_with(|| b.1.cmp(&a.1)))?;
                Some((best_add.0, remove_pos, best_add.1))
            })
            .max_by(|a, b| {
                a.0.cmp(&b.0)
                    .then_with(|| b.1.cmp(&a.1))
                    .then_with(|| b.2.cmp(&a.2))
            });
        let Some((new_score, remove_pos, add)) = best else {
            break;
        };
        if new_score <= score {
            break;
        }
        let removed = std::mem::replace(&mut selected[remove_pos], add);
        used[removed] = false;
        used[add] = true;
        for &building in &services[removed] {
            counts[building as usize] -= 1;
        }
        for &building in &services[add] {
            counts[building as usize] += 1;
        }
        score = new_score;
    }
}

fn restart_seed(base: u64, restart: usize) -> u64 {
    if restart == 0 {
        return base;
    }
    let mut z = base.wrapping_add((restart as u64).wrapping_mul(0x9e3779b97f4a7c15));
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
    z ^ (z >> 31)
}

#[derive(Clone)]
struct PlacementState {
    selected: Vec<usize>,
    used: Vec<bool>,
    metric: Fitness,
}

impl PlacementState {
    fn new(data: &CoverageData, goals: &CoverageGoals, selected: Vec<usize>) -> Self {
        let used = used_mask(data.candidates.len(), &selected);
        let metric = accumulated_fitness(data, goals, selected.iter().copied());
        Self {
            selected,
            used,
            metric,
        }
    }

    fn remove(&mut self, data: &CoverageData, goals: &CoverageGoals, pos: usize) -> usize {
        let removed = self.selected.remove(pos);
        self.used[removed] = false;
        self.refresh(data, goals);
        removed
    }

    fn add(&mut self, data: &CoverageData, goals: &CoverageGoals, candidate: usize) {
        assert!(!self.used[candidate], "candidate already selected");
        self.selected.push(candidate);
        self.used[candidate] = true;
        self.refresh(data, goals);
    }

    fn replace(
        &mut self,
        data: &CoverageData,
        goals: &CoverageGoals,
        pos: usize,
        candidate: usize,
        metric: Fitness,
    ) {
        let removed = std::mem::replace(&mut self.selected[pos], candidate);
        self.used[removed] = false;
        self.used[candidate] = true;
        self.metric = metric;
        debug_assert_eq!(
            self.metric.serviced,
            accumulated_fitness(data, goals, self.selected.iter().copied()).serviced
        );
    }

    fn refresh(&mut self, data: &CoverageData, goals: &CoverageGoals) {
        self.metric = accumulated_fitness(data, goals, self.selected.iter().copied());
    }
}

fn used_mask(candidate_count: usize, selected: &[usize]) -> Vec<bool> {
    let mut used = vec![false; candidate_count];
    for &candidate in selected {
        used[candidate] = true;
    }
    used
}

#[derive(Clone, Copy)]
struct Removal {
    pos: usize,
    metric: Fitness,
}

#[derive(Clone, Copy)]
struct RepairCandidate {
    candidate: usize,
    metric: Fitness,
    completions: usize,
    progress_gain: f64,
}

#[derive(Clone, Copy)]
struct PairRepair {
    remove_a: usize,
    remove_b: usize,
    add_a: usize,
    add_b: usize,
    metric: Fitness,
}

fn threshold_repair(
    data: &CoverageData,
    goals: &CoverageGoals,
    state: &mut PlacementState,
    options: &LnsOptions,
    rng: &mut StdRng,
) {
    if options.lns_repair_rounds == 0
        || state.selected.is_empty()
        || state.selected.len() >= data.candidates.len()
    {
        return;
    }

    for _ in 0..options.lns_repair_rounds {
        let removals = ranked_removals(data, goals, state);
        let removal_width = rcl_width(options.lns_rcl, removals.len());
        let removal_start = rng.random_range(0..removal_width);
        let mut accepted = false;

        for offset in 0..removal_width {
            let removal = removals[(removal_start + offset) % removal_width];
            let base = accumulation_without(data, goals, state, removal.pos);
            let mut available = state.used.clone();
            available[state.selected[removal.pos]] = false;
            let repairs = ranked_repairs(data, goals, &base, &available, options.lns_rcl);
            let repair_width = rcl_width(options.lns_rcl, repairs.len());
            if repair_width == 0 {
                continue;
            }
            let repair_start = rng.random_range(0..repair_width);

            for repair_offset in 0..repair_width {
                let repair = repairs[(repair_start + repair_offset) % repair_width];
                if repair.metric.better_than(state.metric) {
                    state.replace(data, goals, removal.pos, repair.candidate, repair.metric);
                    accepted = true;
                    break;
                }
            }
            if accepted {
                break;
            }
        }

        if !accepted {
            break;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn large_neighborhood_search(
    data: &CoverageData,
    goals: &CoverageGoals,
    state: &mut PlacementState,
    options: &LnsOptions,
    rng: &mut StdRng,
    restart: usize,
    progress: Option<&Arc<dyn Fn(LnsProgress) + Send + Sync>>,
    deadline: Option<Instant>,
) {
    if options.lns_iterations == 0 || state.selected.len() < 2 {
        return;
    }

    let mut best = state.clone();
    let mut best_metric = best.metric;
    let destroy_count = options
        .lns_destroy
        .clamp(1, best.selected.len().saturating_sub(1));

    for iteration in 1..=options.lns_iterations {
        if deadline_reached(deadline) {
            break;
        }
        let target_len = best.selected.len();
        let mut trial = best.clone();

        for _ in 0..destroy_count {
            let removals = ranked_removals(data, goals, &trial);
            let width = rcl_width(options.lns_rcl, removals.len());
            let removal = removals[rng.random_range(0..width)];
            trial.remove(data, goals, removal.pos);
        }

        let mut rebuilt = true;
        while trial.selected.len() < target_len {
            let base = accumulated(data, goals, trial.selected.iter().copied());
            let repairs = ranked_repairs(data, goals, &base, &trial.used, options.lns_rcl);
            let candidate = if repairs.is_empty() {
                random_unselected(&trial.used, rng)
            } else {
                let width = rcl_width(options.lns_rcl, repairs.len());
                Some(repairs[rng.random_range(0..width)].candidate)
            };
            let Some(candidate) = candidate else {
                rebuilt = false;
                break;
            };
            trial.add(data, goals, candidate);
        }
        if !rebuilt {
            continue;
        }

        threshold_repair(data, goals, &mut trial, options, rng);
        let mut oversubscribed = best.clone();
        let mut augmented = true;
        for _ in 0..destroy_count {
            let base = accumulated(data, goals, oversubscribed.selected.iter().copied());
            let repairs = ranked_repairs(data, goals, &base, &oversubscribed.used, options.lns_rcl);
            let candidate = if repairs.is_empty() {
                random_unselected(&oversubscribed.used, rng)
            } else {
                let width = rcl_width(options.lns_rcl, repairs.len());
                Some(repairs[rng.random_range(0..width)].candidate)
            };
            let Some(candidate) = candidate else {
                augmented = false;
                break;
            };
            oversubscribed.add(data, goals, candidate);
        }
        if augmented {
            while oversubscribed.selected.len() > target_len {
                let removals = ranked_removals(data, goals, &oversubscribed);
                let width = rcl_width(options.lns_rcl, removals.len());
                let removal = removals[rng.random_range(0..width)];
                oversubscribed.remove(data, goals, removal.pos);
            }
            threshold_repair(data, goals, &mut oversubscribed, options, rng);
            if oversubscribed.metric.better_than(trial.metric) {
                trial = oversubscribed;
            }
        }
        if trial.metric.better_than(best_metric) {
            let previous = best.selected.clone();
            best_metric = trial.metric;
            best = trial;
            eprintln!(
                "LNS restart {} iteration {iteration}: accepted {} serviced, progress {:.1}",
                restart + 1,
                best_metric.serviced,
                best_metric.progress
            );
            emit_progress(
                data,
                goals,
                &best,
                "LNS accepted",
                restart,
                iteration,
                options.lns_iterations,
                Some(&previous),
                progress,
            );
        }
    }

    *state = best;
}

fn pair_repair(
    data: &CoverageData,
    goals: &CoverageGoals,
    state: &mut PlacementState,
    options: &LnsOptions,
    progress: Option<&Arc<dyn Fn(LnsProgress) + Send + Sync>>,
    deadline: Option<Instant>,
) {
    if options.pair_repair_rounds == 0
        || state.selected.len() < 2
        || state.selected.len() >= data.candidates.len()
    {
        return;
    }

    for round in 1..=options.pair_repair_rounds {
        if deadline_reached(deadline) {
            break;
        }
        let removals = ranked_removals(data, goals, state);
        let removal_width = options.pair_removals.max(2).min(removals.len());
        let removal_pairs: Vec<_> = (0..removal_width)
            .flat_map(|a| ((a + 1)..removal_width).map(move |b| (a, b)))
            .collect();
        let best = removal_pairs
            .into_par_iter()
            .filter_map(|(a, b)| {
                let remove_a = removals[a].pos;
                let remove_b = removals[b].pos;
                let base = accumulation_without_two(data, goals, state, remove_a, remove_b);
                let mut available = state.used.clone();
                available[state.selected[remove_a]] = false;
                available[state.selected[remove_b]] = false;
                let repairs = ranked_repairs(
                    data,
                    goals,
                    &base,
                    &available,
                    options.pair_candidates.max(2),
                );
                let repair_width = options.pair_candidates.max(2).min(repairs.len());
                (0..repair_width)
                    .flat_map(|first| {
                        ((first + 1)..repair_width).map(move |second| (first, second))
                    })
                    .map(|(first, second)| {
                        let add_a = repairs[first].candidate;
                        let add_b = repairs[second].candidate;
                        PairRepair {
                            remove_a,
                            remove_b,
                            add_a,
                            add_b,
                            metric: base.after_adding_two(
                                goals,
                                &data.visibility[add_a],
                                &data.visibility[add_b],
                            ),
                        }
                    })
                    .reduce(best_pair)
            })
            .reduce_with(best_pair)
            .filter(|repair| repair.metric.better_than(state.metric));
        let Some(best) = best else {
            break;
        };
        let previous = state.selected.clone();
        let removed_a = state.selected[best.remove_a];
        let removed_b = state.selected[best.remove_b];
        state.selected[best.remove_a] = best.add_a;
        state.selected[best.remove_b] = best.add_b;
        state.used[removed_a] = false;
        state.used[removed_b] = false;
        state.used[best.add_a] = true;
        state.used[best.add_b] = true;
        state.metric = best.metric;
        eprintln!(
            "pair repair {round}: accepted {} serviced, progress {:.1}",
            state.metric.serviced, state.metric.progress
        );
        emit_progress(
            data,
            goals,
            state,
            "pair repair",
            0,
            round,
            options.pair_repair_rounds,
            Some(&previous),
            progress,
        );
    }
}

fn deadline_reached(deadline: Option<Instant>) -> bool {
    deadline.is_some_and(|deadline| Instant::now() >= deadline)
}

fn best_pair(a: PairRepair, b: PairRepair) -> PairRepair {
    if b.metric.better_than(a.metric) { b } else { a }
}

#[allow(clippy::too_many_arguments)]
fn emit_progress(
    data: &CoverageData,
    goals: &CoverageGoals,
    state: &PlacementState,
    stage: &str,
    restart: usize,
    iteration: usize,
    total_iterations: usize,
    previous: Option<&[usize]>,
    progress: Option<&Arc<dyn Fn(LnsProgress) + Send + Sync>>,
) {
    let Some(progress) = progress else {
        return;
    };
    let antennas: Vec<_> = state
        .selected
        .iter()
        .map(|&candidate| data.candidates[candidate].position)
        .collect();
    let coverage = coverage_progress(data, goals, &state.selected);
    let total_need: f64 = goals.needs.iter().sum();
    let (added, removed) = previous.map_or_else(
        || (Vec::new(), Vec::new()),
        |previous| {
            let mut old = previous.to_vec();
            let mut new = state.selected.clone();
            old.sort_unstable();
            new.sort_unstable();
            let added = new
                .iter()
                .copied()
                .filter(|candidate| old.binary_search(candidate).is_err())
                .map(|candidate| data.candidates[candidate].position)
                .collect();
            let removed = old
                .iter()
                .copied()
                .filter(|candidate| new.binary_search(candidate).is_err())
                .map(|candidate| data.candidates[candidate].position)
                .collect();
            (added, removed)
        },
    );
    progress(LnsProgress {
        stage: stage.into(),
        restart,
        iteration,
        total_iterations,
        serviced: state.metric.serviced,
        progress: state.metric.progress,
        coverage_fraction: if total_need > 0.0 {
            state.metric.progress / total_need
        } else {
            0.0
        },
        antennas,
        added,
        removed,
        coverage,
    });
}

fn coverage_progress(
    data: &CoverageData,
    goals: &CoverageGoals,
    selected: &[usize],
) -> Vec<LnsCoverage> {
    let mut acc = vec![IntervalSet::new(); goals.needs.len()];
    let mut touched = Vec::new();
    for &candidate in selected {
        for (b, set) in &data.visibility[candidate] {
            let b = *b as usize;
            if acc[b].is_empty() {
                touched.push(b);
            }
            acc[b].union_with(set);
        }
    }
    touched
        .into_iter()
        .filter_map(|building| {
            let need = goals.needs[building].max(f64::MIN_POSITIVE);
            let progress = (acc[building].measure() / need).clamp(0.0, 1.0);
            (progress > 0.0).then_some(LnsCoverage { building, progress })
        })
        .collect()
}

struct SelectionViewers {
    offsets: Vec<u32>,
    entries: Vec<(u32, u32)>,
}

impl SelectionViewers {
    fn build(data: &CoverageData, buildings: usize, selected: &[usize]) -> Self {
        let mut offsets = vec![0u32; buildings + 1];
        for &candidate in selected {
            for (building, _) in &data.visibility[candidate] {
                offsets[*building as usize + 1] += 1;
            }
        }
        for building in 0..buildings {
            offsets[building + 1] += offsets[building];
        }
        let mut cursor = offsets.clone();
        let mut entries = vec![(0, 0); offsets[buildings] as usize];
        for (pos, &candidate) in selected.iter().enumerate() {
            for (entry, (building, _)) in data.visibility[candidate].iter().enumerate() {
                let slot = &mut cursor[*building as usize];
                entries[*slot as usize] = (pos as u32, entry as u32);
                *slot += 1;
            }
        }
        Self { offsets, entries }
    }

    fn of(&self, building: usize) -> &[(u32, u32)] {
        &self.entries[self.offsets[building] as usize..self.offsets[building + 1] as usize]
    }
}

fn ranked_removals(
    data: &CoverageData,
    goals: &CoverageGoals,
    state: &PlacementState,
) -> Vec<Removal> {
    let full = accumulated(data, goals, state.selected.iter().copied());
    let viewers = SelectionViewers::build(data, goals.needs.len(), &state.selected);
    let mut removals: Vec<_> = (0..state.selected.len())
        .into_par_iter()
        .map(|pos| Removal {
            pos,
            metric: fitness_without(data, goals, state, &full, &viewers, pos),
        })
        .collect();
    removals
        .sort_unstable_by(|a, b| metric_cmp(b.metric, a.metric).then_with(|| a.pos.cmp(&b.pos)));
    removals
}

fn fitness_without(
    data: &CoverageData,
    goals: &CoverageGoals,
    state: &PlacementState,
    full: &Accumulation,
    viewers: &SelectionViewers,
    pos: usize,
) -> Fitness {
    let mut fitness = full.fitness;
    let mut rest = IntervalSet::new();
    for (building, _) in &data.visibility[state.selected[pos]] {
        let building = *building as usize;
        let need = goals.needs[building];
        rest.clear();
        for &(other, entry) in viewers.of(building) {
            if other as usize != pos {
                let candidate = state.selected[other as usize];
                rest.union_with(&data.visibility[candidate][entry as usize].1);
            }
        }
        let covered = rest.measure();
        if full.is_serviced(building) && !(need > 0.0 && covered >= need) {
            fitness.serviced -= 1;
        }
        fitness.progress += need.min(covered) - need.min(full.covered(building));
    }
    fitness
}

const SHORTLIST_FACTOR: usize = 8;

fn bound_cmp(a: (RepairBound, usize), b: (RepairBound, usize)) -> Ordering {
    b.0.completions
        .cmp(&a.0.completions)
        .then_with(|| b.0.progress.total_cmp(&a.0.progress))
        .then_with(|| a.1.cmp(&b.1))
}

fn ranked_repairs(
    data: &CoverageData,
    goals: &CoverageGoals,
    base: &Accumulation,
    used: &[bool],
    limit: usize,
) -> Vec<RepairCandidate> {
    let limit = limit.max(1);
    let remaining = base.remaining(goals);
    let mut bounds: Vec<_> = (0..data.candidates.len())
        .into_par_iter()
        .filter(|&candidate| !used[candidate])
        .map(|candidate| {
            (
                RepairBound::of(data.flat.entries(candidate), &remaining),
                candidate,
            )
        })
        .filter(|&(bound, _)| bound.can_qualify())
        .collect();

    let shortlist = limit.saturating_mul(SHORTLIST_FACTOR).min(bounds.len());
    if shortlist < bounds.len() {
        bounds.select_nth_unstable_by(shortlist, |&a, &b| bound_cmp(a, b));
    }
    let bar = evaluate_repairs(data, goals, base, &bounds[..shortlist], limit);
    if bar.len() < limit {
        return evaluate_repairs(data, goals, base, &bounds, limit);
    }
    let worst = bar.last().expect("nonempty ranking");
    let contenders: Vec<_> = bounds
        .par_iter()
        .copied()
        .filter(|&(bound, _)| bound.can_beat(worst.completions, worst.progress_gain))
        .collect();
    evaluate_repairs(data, goals, base, &contenders, limit)
}

#[cfg(test)]
fn exhaustive_repairs(
    data: &CoverageData,
    goals: &CoverageGoals,
    base: &Accumulation,
    used: &[bool],
    limit: usize,
) -> Vec<RepairCandidate> {
    let mut repairs: Vec<_> = (0..data.candidates.len())
        .into_par_iter()
        .filter(|&candidate| !used[candidate])
        .filter_map(|candidate| {
            let metric = base.after_adding(goals, &data.visibility[candidate]);
            let completions = metric.serviced.saturating_sub(base.fitness.serviced);
            let progress_gain = metric.progress - base.fitness.progress;
            (completions > 0 || progress_gain > EPS).then_some(RepairCandidate {
                candidate,
                metric,
                completions,
                progress_gain,
            })
        })
        .collect();
    let limit = limit.max(1);
    if limit < repairs.len() {
        repairs.select_nth_unstable_by(limit, repair_cmp);
        repairs.truncate(limit);
    }
    repairs.sort_unstable_by(repair_cmp);
    repairs
}

fn evaluate_repairs(
    data: &CoverageData,
    goals: &CoverageGoals,
    base: &Accumulation,
    shortlist: &[(RepairBound, usize)],
    limit: usize,
) -> Vec<RepairCandidate> {
    let mut repairs: Vec<_> = shortlist
        .par_iter()
        .map(|&(_, candidate)| candidate)
        .filter_map(|candidate| {
            let metric = base.after_adding(goals, &data.visibility[candidate]);
            let completions = metric.serviced.saturating_sub(base.fitness.serviced);
            let progress_gain = metric.progress - base.fitness.progress;
            (completions > 0 || progress_gain > EPS).then_some(RepairCandidate {
                candidate,
                metric,
                completions,
                progress_gain,
            })
        })
        .collect();
    if limit < repairs.len() {
        repairs.select_nth_unstable_by(limit, repair_cmp);
        repairs.truncate(limit);
    }
    repairs.sort_unstable_by(repair_cmp);
    repairs
}

fn repair_cmp(a: &RepairCandidate, b: &RepairCandidate) -> Ordering {
    b.completions
        .cmp(&a.completions)
        .then_with(|| b.progress_gain.total_cmp(&a.progress_gain))
        .then_with(|| a.candidate.cmp(&b.candidate))
}

fn accumulation_without(
    data: &CoverageData,
    goals: &CoverageGoals,
    state: &PlacementState,
    skip_pos: usize,
) -> Accumulation {
    accumulated(
        data,
        goals,
        state
            .selected
            .iter()
            .enumerate()
            .filter_map(|(pos, &candidate)| (pos != skip_pos).then_some(candidate)),
    )
}

fn accumulation_without_two(
    data: &CoverageData,
    goals: &CoverageGoals,
    state: &PlacementState,
    skip_a: usize,
    skip_b: usize,
) -> Accumulation {
    accumulated(
        data,
        goals,
        state
            .selected
            .iter()
            .enumerate()
            .filter_map(|(pos, &candidate)| (pos != skip_a && pos != skip_b).then_some(candidate)),
    )
}

fn random_unselected(used: &[bool], rng: &mut StdRng) -> Option<usize> {
    let available: Vec<_> = used
        .iter()
        .enumerate()
        .filter_map(|(candidate, &is_used)| (!is_used).then_some(candidate))
        .collect();
    (!available.is_empty()).then(|| available[rng.random_range(0..available.len())])
}

fn rcl_width(requested: usize, available: usize) -> usize {
    requested.max(1).min(available)
}

fn metric_cmp(a: Fitness, b: Fitness) -> Ordering {
    a.serviced
        .cmp(&b.serviced)
        .then_with(|| a.progress.total_cmp(&b.progress))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sampling::{Candidate, CandidateKind};
    use framework::IntervalSet;

    fn view(entries: &[(u32, f64, f64)]) -> Vec<(u32, IntervalSet)> {
        entries
            .iter()
            .map(|&(building, lo, hi)| (building, IntervalSet::from_spans(vec![(lo, hi)])))
            .collect()
    }

    fn synthetic_data() -> CoverageData {
        CoverageData::new(
            (0..4)
                .map(|i| Candidate {
                    position: Point::new(i as f64, 0.0),
                    building: 0,
                    kind: CandidateKind::Uniform,
                })
                .collect(),
            vec![
                view(&[(0, 0.0, 10.0)]),
                view(&[(1, 0.0, 5.0)]),
                view(&[(1, 0.0, 10.0)]),
                Vec::new(),
            ],
            vec![10.0, 10.0],
        )
    }

    fn random_data(trial: usize) -> CoverageData {
        let mut seed = 0x5eedu64.wrapping_add(trial as u64 * 0x9e3779b9);
        let mut rng = || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) as f64 / (1u64 << 31) as f64
        };
        let buildings = 6;
        let count = 25;
        let perimeters: Vec<f64> = (0..buildings).map(|_| 8.0 + rng() * 8.0).collect();
        let visibility: Vec<Vec<(u32, IntervalSet)>> = (0..count)
            .map(|_| {
                let mut view = Vec::new();
                for (building, &perimeter) in perimeters.iter().enumerate() {
                    if rng() < 0.45 {
                        let lo = rng() * perimeter;
                        let hi = (lo + rng() * perimeter).min(perimeter);
                        if hi > lo {
                            view.push((building as u32, IntervalSet::from_spans(vec![(lo, hi)])));
                        }
                    }
                }
                view
            })
            .collect();
        CoverageData::new(
            (0..count)
                .map(|i| Candidate {
                    position: Point::new(i as f64, 0.0),
                    building: 0,
                    kind: CandidateKind::Uniform,
                })
                .collect(),
            visibility,
            perimeters,
        )
    }

    #[test]
    fn bounded_ranking_matches_exhaustive() {
        for trial in 0..20 {
            let data = random_data(trial);
            let count = data.candidates.len();
            let goals = CoverageGoals::new(&data.perimeters, 0.45);
            let mut selected: Vec<usize> = (0..5).map(|i| (trial * 3 + i * 5) % count).collect();
            selected.sort_unstable();
            selected.dedup();
            let state = PlacementState::new(&data, &goals, selected);

            for pos in 0..state.selected.len() {
                let base = accumulation_without(&data, &goals, &state, pos);
                let mut available = state.used.clone();
                available[state.selected[pos]] = false;
                for limit in [1, 3, 12, 40] {
                    let bounded = ranked_repairs(&data, &goals, &base, &available, limit);
                    let exhaustive = exhaustive_repairs(&data, &goals, &base, &available, limit);
                    let describe = |r: &RepairCandidate| {
                        (r.candidate, r.completions, (r.progress_gain * 1e6).round())
                    };
                    assert_eq!(
                        bounded.iter().map(describe).collect::<Vec<_>>(),
                        exhaustive.iter().map(describe).collect::<Vec<_>>(),
                        "trial {trial}, removal {pos}, limit {limit}"
                    );
                }
            }
        }
    }

    #[test]
    fn incremental_removals_match_full_rebuilds() {
        for (trial, data) in (0..12).map(|trial| (trial, random_data(trial))) {
            let goals = CoverageGoals::new(&data.perimeters, 0.4);
            let mut selected: Vec<usize> = (0..7)
                .map(|i| (trial * 3 + i * 4) % data.candidates.len())
                .collect();
            selected.sort_unstable();
            selected.dedup();
            let state = PlacementState::new(&data, &goals, selected);

            for removal in ranked_removals(&data, &goals, &state) {
                let rebuilt = accumulation_without(&data, &goals, &state, removal.pos).fitness;
                assert_eq!(
                    removal.metric.serviced, rebuilt.serviced,
                    "trial {trial}, position {}",
                    removal.pos
                );
                assert!(
                    (removal.metric.progress - rebuilt.progress).abs() < 1e-6,
                    "trial {trial}, position {}: {} vs {}",
                    removal.pos,
                    removal.metric.progress,
                    rebuilt.progress
                );
            }
        }
    }

    fn exhaustive_addition(
        data: &CoverageData,
        goals: &CoverageGoals,
        base: &Accumulation,
        pool: &[usize],
        used: &[bool],
    ) -> Option<usize> {
        pool.iter()
            .copied()
            .filter(|&candidate| !used[candidate])
            .map(|candidate| {
                (
                    candidate,
                    base.after_adding(goals, &data.visibility[candidate]),
                )
            })
            .reduce(
                |a, b| match metric_cmp(a.1, b.1).then_with(|| b.0.cmp(&a.0)) {
                    Ordering::Less => b,
                    _ => a,
                },
            )
            .map(|(candidate, _)| candidate)
    }

    #[test]
    fn bounded_addition_matches_exhaustive() {
        for trial in 0..20 {
            let data = random_data(trial);
            let goals = CoverageGoals::new(&data.perimeters, 0.45);
            let pool: Vec<usize> = (0..data.candidates.len()).collect();
            let mut used = vec![false; data.candidates.len()];
            let mut base = accumulated(&data, &goals, std::iter::empty());

            for pick in 0..8 {
                let bounded = best_addition(&data, &goals, &base, &pool, &used);
                let exhaustive = exhaustive_addition(&data, &goals, &base, &pool, &used);
                assert_eq!(bounded, exhaustive, "trial {trial}, pick {pick}");
                let Some(candidate) = bounded else { break };
                used[candidate] = true;
                base.add(&goals, &data.visibility[candidate]);
            }
        }
    }

    #[test]
    fn incremental_additions_match_full_rebuilds() {
        for trial in 0..12 {
            let data = random_data(trial);
            let goals = CoverageGoals::new(&data.perimeters, 0.4);
            let selected: Vec<usize> = (0..9)
                .map(|i| (trial * 3 + i * 4) % data.candidates.len())
                .collect();
            let mut incremental = accumulated(&data, &goals, std::iter::empty());

            for (step, &candidate) in selected.iter().enumerate() {
                incremental.add(&goals, &data.visibility[candidate]);
                let rebuilt = accumulated(&data, &goals, selected[..=step].iter().copied());
                assert_eq!(
                    incremental.fitness.serviced, rebuilt.fitness.serviced,
                    "trial {trial}, step {step}"
                );
                assert!(
                    (incremental.fitness.progress - rebuilt.fitness.progress).abs() < 1e-6,
                    "trial {trial}, step {step}: {} vs {}",
                    incremental.fitness.progress,
                    rebuilt.fitness.progress,
                );
            }
        }
    }

    #[test]
    fn direct_selection_matches_rescanning() {
        for trial in 0..12 {
            let data = random_data(trial);
            let goals = CoverageGoals::new(&data.perimeters, 0.3);
            let services = direct_services(&data, &goals);

            for k in [1, 4, 9] {
                let mut expected = Vec::new();
                let mut expected_counts = vec![0u16; goals.needs.len()];
                let mut used = vec![false; data.candidates.len()];
                while expected.len() < k {
                    let best = (0..data.candidates.len())
                        .filter(|&candidate| !used[candidate])
                        .map(|candidate| {
                            let gain = services[candidate]
                                .iter()
                                .filter(|&&building| expected_counts[building as usize] == 0)
                                .count();
                            (gain, candidate)
                        })
                        .max_by(|a, b| a.0.cmp(&b.0).then_with(|| b.1.cmp(&a.1)));
                    let Some((_, candidate)) = best else { break };
                    used[candidate] = true;
                    expected.push(candidate);
                    for &building in &services[candidate] {
                        expected_counts[building as usize] += 1;
                    }
                }
                let (selected, counts, _) =
                    direct_greedy(&services, goals.needs.len(), data.candidates.len(), k);
                assert_eq!(selected, expected, "trial {trial}, k {k}");
                assert_eq!(counts, expected_counts, "trial {trial}, k {k}");
            }
        }
    }

    #[test]
    fn threshold_repair_accepts_strictly_better_swap() {
        let data = synthetic_data();
        let goals = CoverageGoals::new(&data.perimeters, 1.0);
        let mut state = PlacementState::new(&data, &goals, vec![0, 1]);
        let before = state.metric;
        let options = LnsOptions {
            lns_iterations: 0,
            lns_rcl: 4,
            lns_repair_rounds: 2,
            seed: Some(7),
            ..LnsOptions::default()
        };
        let mut rng = StdRng::seed_from_u64(7);

        threshold_repair(&data, &goals, &mut state, &options, &mut rng);

        assert!(state.metric.better_than(before));
        assert_eq!(state.metric.serviced, 2);
        assert!(state.selected.contains(&2));
    }

    #[test]
    fn zero_iterations_leave_repaired_state_unchanged() {
        let data = synthetic_data();
        let goals = CoverageGoals::new(&data.perimeters, 1.0);
        let mut state = PlacementState::new(&data, &goals, vec![0, 2]);
        let before = state.selected.clone();
        let options = LnsOptions {
            lns_iterations: 0,
            ..LnsOptions::default()
        };
        let mut rng = StdRng::seed_from_u64(3);

        large_neighborhood_search(&data, &goals, &mut state, &options, &mut rng, 0, None, None);

        assert_eq!(state.selected, before);
    }

    #[test]
    fn lns_never_writes_back_a_worse_state() {
        let data = synthetic_data();
        let goals = CoverageGoals::new(&data.perimeters, 1.0);
        let mut state = PlacementState::new(&data, &goals, vec![0, 2]);
        let before = state.metric;
        let options = LnsOptions {
            lns_iterations: 20,
            lns_destroy: 10,
            lns_rcl: 3,
            lns_repair_rounds: 1,
            seed: Some(11),
            ..LnsOptions::default()
        };
        let mut rng = StdRng::seed_from_u64(11);

        large_neighborhood_search(&data, &goals, &mut state, &options, &mut rng, 0, None, None);

        assert!(!before.better_than(state.metric));
        assert_eq!(state.selected.len(), 2);
    }

    #[test]
    fn ruin_and_recreate_accept_an_improving_trial() {
        let data = synthetic_data();
        let goals = CoverageGoals::new(&data.perimeters, 1.0);
        let mut state = PlacementState::new(&data, &goals, vec![0, 1]);
        let options = LnsOptions {
            lns_iterations: 1,
            lns_destroy: 1,
            lns_rcl: 1,
            lns_repair_rounds: 0,
            seed: Some(5),
            ..LnsOptions::default()
        };
        let mut rng = StdRng::seed_from_u64(5);

        large_neighborhood_search(&data, &goals, &mut state, &options, &mut rng, 0, None, None);

        assert_eq!(state.metric.serviced, 2);
        assert!(state.selected.contains(&2));
    }
}

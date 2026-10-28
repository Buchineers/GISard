use framework::{CoverageGoals, Evaluator, Instance, IntervalSet, Point, Solution, Solver};
use rand::rngs::StdRng;
use rand::{RngExt, SeedableRng};
use rayon::prelude::*;
use std::sync::Arc;
use std::time::Instant;

use crate::candidates::{
    CandidatePhase, CandidatePlan, LocalBoundary, ProbeRings, ThresholdBreakpoints,
};
use crate::coverage::{CoverageConfig, CoverageData, prune_dominated};
#[cfg(test)]
use crate::solvers::greedy_coverage::select_indices;
use crate::solvers::swap_refine::{Fitness, accumulated, refine, used_mask};

const EPS: f64 = 1e-9;
const REMOVAL_SAMPLES: usize = 4;
const TARGET_SAMPLES: usize = 32;
const CALIBRATION_SAMPLES: usize = 128;
const FINAL_REFINEMENT_SWAPS: usize = 10;
const INITIAL_ACCEPTANCE: f64 = 0.8;
const FINAL_TEMPERATURE_RATIO: f64 = 1e-3;
const HEAT_BATH_LIMIT: usize = 32;
const DEADLINE_STRIDE: usize = 64;

#[derive(Clone, Debug)]
pub struct AnnealingProgress {
    pub antennas: Vec<Point>,
    pub serviced: usize,
}

pub type AnnealingReporter = Arc<dyn Fn(AnnealingProgress) + Send + Sync>;

pub struct AnnealingTimedOptions<'a> {
    pub resume: Option<&'a [Point]>,
    pub seeds: Option<&'a [Vec<usize>]>,
    pub preferred_start: usize,
    pub deadline: Instant,
    pub progress: Option<AnnealingReporter>,
}

#[derive(Default)]
pub struct SimulatedAnnealingSolver {
    pub coverage: CoverageConfig,
    pub config: SimulatedAnnealingConfig,
    plan: Option<CandidatePlan>,
    #[cfg(feature = "visualizer")]
    pub canvas: Option<Arc<dyn visualizer::Canvas>>,
}

#[derive(Clone, Debug)]
pub struct SimulatedAnnealingConfig {
    pub iterations: usize,
    pub proposal_samples: usize,
    pub pair_probability: f64,
    pub target_probability: f64,
    pub local_spacing: f64,
    pub probe_rings: usize,
    pub threshold_breakpoints: bool,
    pub refine_fraction: f64,
    pub seed: Option<u64>,
    pub chains: usize,
    pub swap_interval: usize,
    pub ladder_span: f64,
    pub heat_bath: bool,
    pub tabu_tenure: usize,
    pub reheat_after: usize,
    pub reheat_ratio: f64,
    pub prune_dominated: bool,
}

impl Default for SimulatedAnnealingConfig {
    fn default() -> Self {
        Self {
            iterations: 50_000,
            proposal_samples: 8,
            pair_probability: 0.1,
            target_probability: 0.85,
            local_spacing: 0.0,
            probe_rings: 0,
            threshold_breakpoints: false,
            refine_fraction: 0.25,
            seed: None,
            chains: 1,
            swap_interval: 0,
            ladder_span: FINAL_TEMPERATURE_RATIO,
            heat_bath: false,
            tabu_tenure: 0,
            reheat_after: 0,
            reheat_ratio: 0.25,
            prune_dominated: false,
        }
    }
}

impl SimulatedAnnealingConfig {
    pub fn tempered() -> Self {
        let chains = std::thread::available_parallelism().map_or(4, |cores| cores.get().min(16));
        Self {
            chains,
            swap_interval: 200,
            heat_bath: true,
            tabu_tenure: 12,
            reheat_after: 5_000,
            prune_dominated: true,
            ..Self::default()
        }
    }
}

impl SimulatedAnnealingSolver {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_coverage_config(mut self, coverage: CoverageConfig) -> Self {
        self.coverage = coverage;
        self
    }

    pub fn with_config(mut self, config: SimulatedAnnealingConfig) -> Self {
        self.config = config;
        self
    }

    pub fn with_candidate_plan(mut self, plan: CandidatePlan) -> Self {
        self.plan = Some(plan);
        self
    }

    #[cfg(feature = "visualizer")]
    pub fn with_canvas(mut self, canvas: Arc<dyn visualizer::Canvas>) -> Self {
        self.canvas = Some(canvas);
        self
    }

    fn implied_plan(&self) -> CandidatePlan {
        let mut plan = self.coverage.plan();
        if self.config.local_spacing > 0.0 {
            plan = plan.with(
                CandidatePhase::Seeded,
                LocalBoundary::new(self.config.local_spacing),
            );
        }
        if self.config.probe_rings > 0 {
            plan = plan.with(
                CandidatePhase::Polish,
                ProbeRings::new(self.config.probe_rings, 16),
            );
        }
        if self.config.threshold_breakpoints {
            plan = plan
                .with(CandidatePhase::Seeded, ThresholdBreakpoints::new())
                .with(CandidatePhase::Polish, ThresholdBreakpoints::new());
        }
        plan
    }
}

impl Solver for SimulatedAnnealingSolver {
    fn solve(&self, instance: Instance) -> Solution {
        if instance.k == 0 || instance.buildings.is_empty() {
            return Solution {
                antennas: Vec::new(),
            };
        }
        #[cfg(feature = "visualizer")]
        if let Some(canvas) = &self.canvas {
            canvas.draw(visualizer::DrawCommand::Status {
                text: format!(
                    "Simulated annealing\nprecomputing candidate coverage...\nantennas: {}",
                    instance.k
                ),
            });
        }
        validate_config(&self.config);
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
            self.config.seed,
        );
        if self.config.prune_dominated {
            let before = data.candidates.len();
            data = prune_dominated(data, instance.k);
            eprintln!(
                "sa: pool {before} -> {} after dominance pruning",
                data.candidates.len()
            );
        }
        let goals = CoverageGoals::new(&data.perimeters, instance.tau);
        #[cfg(feature = "visualizer")]
        let visual = self
            .canvas
            .as_deref()
            .map(|canvas| AnnealingVisualization::new(canvas, &instance));
        let seeds = super::lns::seed_index_sets(&data, &goals, instance.k, instance.tau);
        let (mut selected, _) = search_from(
            &Run {
                data: &data,
                goals: &goals,
                config: &self.config,
                preferred_start: data.candidates.len(),
                deadline: None,
                progress: None,
                quiet: false,
                #[cfg(feature = "visualizer")]
                visual: visual.as_ref(),
            },
            seeds,
            self.config.iterations,
            self.config.seed,
        );

        let refine_iterations =
            (self.config.iterations as f64 * self.config.refine_fraction) as usize;
        for (pass, phase) in [CandidatePhase::Seeded, CandidatePhase::Polish]
            .into_iter()
            .filter(|&phase| plan.has(phase))
            .enumerate()
        {
            if selected.is_empty() {
                break;
            }
            let incumbent: Vec<_> = selected
                .iter()
                .map(|&candidate| data.candidates[candidate].position)
                .collect();
            let phase_start = data.candidates.len();
            if plan.extend_with(&evaluator, phase, &mut data, &incumbent, self.config.seed) == 0 {
                continue;
            }
            let seed = self
                .config
                .seed
                .map(|seed| seed.wrapping_add(pass as u64 + 1));
            (selected, _) = search_from(
                &Run {
                    data: &data,
                    goals: &goals,
                    config: &self.config,
                    preferred_start: phase_start,
                    deadline: None,
                    progress: None,
                    quiet: false,
                    #[cfg(feature = "visualizer")]
                    visual: visual.as_ref(),
                },
                vec![selected],
                refine_iterations,
                seed,
            );
        }

        #[cfg(feature = "visualizer")]
        if let Some(visual) = &visual {
            visual.clear();
        }
        refine_selection(&data, &goals, &mut selected);
        let antennas = selected
            .iter()
            .map(|&candidate| data.candidates[candidate].position)
            .collect();
        Solution { antennas }
    }
}

pub fn select_annealed_antennas(
    data: &CoverageData,
    k: usize,
    tau: f64,
    config: &SimulatedAnnealingConfig,
) -> (Vec<Point>, usize) {
    validate_config(config);
    let goals = CoverageGoals::new(&data.perimeters, tau);
    let seeds = super::lns::seed_index_sets(data, &goals, k, tau);
    let (mut selected, _) = search_from(
        &Run {
            data,
            goals: &goals,
            config,
            preferred_start: data.candidates.len(),
            deadline: None,
            progress: None,
            quiet: false,
            #[cfg(feature = "visualizer")]
            visual: None,
        },
        seeds,
        config.iterations,
        config.seed,
    );
    refine_selection(data, &goals, &mut selected);
    let fitness = accumulated(data, &goals, selected.iter().copied()).fitness;
    let antennas = selected
        .iter()
        .map(|&candidate| data.candidates[candidate].position)
        .collect();
    (antennas, fitness.serviced)
}

pub fn select_annealed_antennas_until(
    data: &CoverageData,
    k: usize,
    tau: f64,
    config: &SimulatedAnnealingConfig,
    resume: Option<&[Point]>,
    preferred_start: usize,
    deadline: Instant,
) -> Vec<Point> {
    select_annealed_antennas_until_with_progress(
        data,
        k,
        tau,
        config,
        AnnealingTimedOptions {
            resume,
            seeds: None,
            preferred_start,
            deadline,
            progress: None,
        },
    )
    .0
}

pub fn select_annealed_antennas_until_with_progress(
    data: &CoverageData,
    k: usize,
    tau: f64,
    config: &SimulatedAnnealingConfig,
    options: AnnealingTimedOptions<'_>,
) -> (Vec<Point>, usize) {
    validate_config(config);
    let goals = CoverageGoals::new(&data.perimeters, tau);
    let mut seeds: Vec<Vec<usize>> = options
        .resume
        .map(|points| super::lns::resume_indices(data, points))
        .filter(|selected| selected.len() == k)
        .into_iter()
        .collect();
    if seeds.is_empty() {
        seeds = options
            .seeds
            .map(<[Vec<usize>]>::to_vec)
            .unwrap_or_else(|| super::lns::seed_index_sets(data, &goals, k, tau));
    }
    let (selected, fitness) = search_from(
        &Run {
            data,
            goals: &goals,
            config,
            preferred_start: options.preferred_start,
            deadline: Some(options.deadline),
            progress: options.progress.as_ref(),
            quiet: false,
            #[cfg(feature = "visualizer")]
            visual: None,
        },
        seeds,
        usize::MAX,
        config.seed,
    );
    let antennas = selected
        .iter()
        .map(|&candidate| data.candidates[candidate].position)
        .collect();
    (antennas, fitness.serviced)
}

pub(crate) fn annealing_seed_indices(data: &CoverageData, k: usize, tau: f64) -> Vec<Vec<usize>> {
    let goals = CoverageGoals::new(&data.perimeters, tau);
    super::lns::seed_index_sets(data, &goals, k, tau)
}

fn refine_selection(data: &CoverageData, goals: &CoverageGoals, selected: &mut [usize]) {
    refine(data, goals, selected, FINAL_REFINEMENT_SWAPS, 0);
}

fn validate_config(config: &SimulatedAnnealingConfig) {
    assert!(
        config.proposal_samples > 0,
        "proposal samples must be positive"
    );
    assert!(
        (0.0..=1.0).contains(&config.pair_probability),
        "pair probability must be in [0, 1]"
    );
    assert!(
        (0.0..=1.0).contains(&config.target_probability),
        "target probability must be in [0, 1]"
    );
    assert!(
        config.refine_fraction >= 0.0,
        "refine fraction must not be negative"
    );
    assert!(
        config.reheat_after == 0 || (config.reheat_ratio > 0.0 && config.reheat_ratio <= 1.0),
        "reheat ratio must be in (0, 1]"
    );
}

#[derive(Clone, Copy)]
struct Run<'a> {
    data: &'a CoverageData,
    goals: &'a CoverageGoals,
    config: &'a SimulatedAnnealingConfig,
    preferred_start: usize,
    deadline: Option<Instant>,
    progress: Option<&'a AnnealingReporter>,
    quiet: bool,
    #[cfg(feature = "visualizer")]
    visual: Option<&'a AnnealingVisualization<'a>>,
}

fn search_from(
    run: &Run<'_>,
    seeds: Vec<Vec<usize>>,
    iterations: usize,
    seed: Option<u64>,
) -> (Vec<usize>, Fitness) {
    assert!(run.preferred_start <= run.data.candidates.len());
    assert!(!seeds.is_empty(), "a search needs at least one seed");
    match (run.config.chains.max(1), run.config.swap_interval) {
        (1, _) => anneal_from(
            run,
            seeds.into_iter().next().expect("seeds is not empty"),
            iterations,
            seed,
        ),
        (chains, 0) => independent(run, &seeds, iterations, seed, chains),
        (chains, interval) => temper(run, &seeds, iterations, seed, chains, interval),
    }
}

fn anneal_from(
    run: &Run<'_>,
    selected: Vec<usize>,
    iterations: usize,
    seed: Option<u64>,
) -> (Vec<usize>, Fitness) {
    let search = Search::new(run);
    let mut chain = Chain::new(run, selected, seed.unwrap_or_else(|| rand::rng().random()));
    if iterations == 0 || !searchable(run.data, &chain.state.selected) {
        return (chain.best, chain.best_fitness);
    }

    let started = Instant::now();
    let initial_temperature =
        calibrate_temperature(&search, &mut chain.state, &mut chain.rng, run.deadline);
    let end = match run.deadline {
        Some(deadline) => deadline.saturating_duration_since(started).as_secs_f64(),
        None => iterations.saturating_sub(1) as f64,
    };
    let mut schedule = Schedule::new(initial_temperature, FINAL_TEMPERATURE_RATIO, end);
    #[cfg(feature = "visualizer")]
    if let Some(visual) = run.visual {
        visual.draw(
            run.data,
            &chain,
            0,
            iterations,
            initial_temperature,
            initial_temperature,
        );
    }
    #[cfg(feature = "visualizer")]
    let frame_stride = (iterations / 500).max(1);

    for iteration in 0..iterations {
        let now = match run.deadline {
            Some(deadline) => {
                let now = Instant::now();
                if now >= deadline {
                    break;
                }
                now.duration_since(started).as_secs_f64()
            }
            None => iteration as f64,
        };
        let temperature = schedule.temperature(now);
        if !chain.step(&search, temperature) {
            break;
        }
        if run.config.reheat_after > 0 && chain.stagnant >= run.config.reheat_after {
            schedule.reheat(now, run.config.reheat_ratio);
            chain.restart_from_best(run);
        }
        #[cfg(feature = "visualizer")]
        if let Some(visual) = run.visual
            && ((iteration + 1) % frame_stride == 0 || iteration + 1 == iterations)
        {
            visual.draw(
                run.data,
                &chain,
                iteration + 1,
                iterations,
                temperature,
                initial_temperature,
            );
        }
    }
    if !run.quiet {
        eprintln!(
            "sa: {} proposals, {} accepted",
            chain.proposals, chain.accepted
        );
    }
    (chain.best, chain.best_fitness)
}

fn independent(
    run: &Run<'_>,
    seeds: &[Vec<usize>],
    iterations: usize,
    seed: Option<u64>,
    chains: usize,
) -> (Vec<usize>, Fitness) {
    let base = seed.unwrap_or_else(|| rand::rng().random());
    let child = Run {
        quiet: true,
        #[cfg(feature = "visualizer")]
        visual: None,
        ..*run
    };
    let best = (0..chains)
        .into_par_iter()
        .map(|chain| {
            let run = if chain == 0 { *run } else { child };
            anneal_from(
                &run,
                seeds[chain % seeds.len()].clone(),
                iterations,
                Some(base.wrapping_add(chain as u64)),
            )
        })
        .reduce_with(better_outcome)
        .expect("at least one chain");
    eprintln!(
        "sa: {chains} independent chains, best {} serviced",
        best.1.serviced
    );
    best
}

fn temper(
    run: &Run<'_>,
    seeds: &[Vec<usize>],
    iterations: usize,
    seed: Option<u64>,
    chains: usize,
    interval: usize,
) -> (Vec<usize>, Fitness) {
    let search = Search::new(run);
    let base = seed.unwrap_or_else(|| rand::rng().random());
    let mut replicas: Vec<Chain> = (0..chains)
        .map(|chain| {
            Chain::new(
                run,
                seeds[chain % seeds.len()].clone(),
                base.wrapping_add(chain as u64),
            )
        })
        .collect();
    if iterations == 0 || !searchable(run.data, &replicas[0].state.selected) {
        return best_of(replicas);
    }

    let coldest = &mut replicas[0];
    let hottest =
        calibrate_temperature(&search, &mut coldest.state, &mut coldest.rng, run.deadline);
    let ladder = temperature_ladder(hottest, chains, run.config.ladder_span);
    let mut swap_rng = StdRng::seed_from_u64(base ^ u64::from_le_bytes(*b"tempered"));
    #[cfg(feature = "visualizer")]
    let frame_stride = (iterations / interval / 500).max(1);
    let mut done = 0usize;
    let mut round = 0usize;
    let mut exchanges = 0usize;

    while done < iterations {
        if run
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            break;
        }
        let block = interval.min(iterations - done);
        replicas
            .par_iter_mut()
            .zip(&ladder)
            .for_each(|(chain, &temperature)| {
                for step in 0..block {
                    if step % DEADLINE_STRIDE == 0
                        && run
                            .deadline
                            .is_some_and(|deadline| Instant::now() >= deadline)
                    {
                        break;
                    }
                    if !chain.step(&search, temperature) {
                        break;
                    }
                }
            });
        done += block;

        for cold in (round % 2..chains - 1).step_by(2) {
            let hot = cold + 1;
            let delta = (1.0 / ladder[cold] - 1.0 / ladder[hot])
                * (search.energy(replicas[hot].state.fitness)
                    - search.energy(replicas[cold].state.fitness));
            if delta >= 0.0 || swap_rng.random::<f64>() < delta.exp() {
                let (below, above) = replicas.split_at_mut(hot);
                below[cold].exchange(&mut above[0]);
                exchanges += 1;
            }
        }
        if run.config.reheat_after > 0 && replicas[0].stagnant >= run.config.reheat_after {
            replicas[0].restart_from_best(run);
        }
        round += 1;

        #[cfg(feature = "visualizer")]
        if let Some(visual) = run.visual
            && round.is_multiple_of(frame_stride)
        {
            visual.draw_ladder(run.data, &replicas, done, iterations, ladder[0], hottest);
        }
    }

    let proposals: usize = replicas.iter().map(|chain| chain.proposals).sum();
    let accepted: usize = replicas.iter().map(|chain| chain.accepted).sum();
    eprintln!(
        "sa: {chains} tempered chains, {proposals} proposals, {accepted} accepted, \
         {exchanges} exchanges"
    );
    best_of(replicas)
}

fn best_of(replicas: Vec<Chain>) -> (Vec<usize>, Fitness) {
    replicas
        .into_iter()
        .map(|chain| (chain.best, chain.best_fitness))
        .reduce(better_outcome)
        .expect("at least one replica")
}

fn better_outcome(
    incumbent: (Vec<usize>, Fitness),
    challenger: (Vec<usize>, Fitness),
) -> (Vec<usize>, Fitness) {
    if challenger.1.better_than(incumbent.1) {
        challenger
    } else {
        incumbent
    }
}

fn searchable(data: &CoverageData, selected: &[usize]) -> bool {
    !selected.is_empty() && selected.len() < data.candidates.len()
}

fn temperature_ladder(hottest: f64, rungs: usize, span: f64) -> Vec<f64> {
    let coldest = hottest * span;
    (0..rungs)
        .map(|rung| {
            let fraction = rung as f64 / (rungs - 1).max(1) as f64;
            coldest * (1.0 / span).powf(fraction)
        })
        .collect()
}

fn calibrate_temperature(
    search: &Search<'_>,
    state: &mut AnnealingState,
    rng: &mut StdRng,
    deadline: Option<Instant>,
) -> f64 {
    let mut total_loss = 0.0;
    let mut losses = 0usize;
    for _ in 0..CALIBRATION_SAMPLES {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            break;
        }
        let context = StepContext::calibration(state.fitness);
        let Some(proposal) = search.propose(state, context, rng) else {
            break;
        };
        let delta = search.energy(proposal.fitness) - search.energy(state.fitness);
        if delta < -EPS {
            total_loss -= delta;
            losses += 1;
        }
    }
    if losses > 0 {
        let average_loss = total_loss / losses as f64;
        average_loss / -INITIAL_ACCEPTANCE.ln()
    } else {
        0.01
    }
}

#[derive(Clone, Copy)]
struct Schedule {
    initial: f64,
    final_ratio: f64,
    base: f64,
    start: f64,
    end: f64,
}

impl Schedule {
    fn new(initial: f64, final_ratio: f64, end: f64) -> Self {
        Self {
            initial,
            final_ratio,
            base: initial,
            start: 0.0,
            end,
        }
    }

    fn temperature(&self, now: f64) -> f64 {
        let span = self.end - self.start;
        let fraction = if span > 0.0 {
            (now - self.start) / span
        } else {
            0.0
        };
        self.base * self.final_ratio.powf(fraction.clamp(0.0, 1.0))
    }

    fn reheat(&mut self, now: f64, ratio: f64) {
        self.base = self.initial * ratio;
        self.start = now.min(self.end);
    }
}

struct Chain {
    state: AnnealingState,
    rng: StdRng,
    tabu: Option<Tabu>,
    best: Vec<usize>,
    best_fitness: Fitness,
    stagnant: usize,
    proposals: usize,
    accepted: usize,
    #[cfg(feature = "visualizer")]
    last_accepted: Option<Proposal>,
}

impl Chain {
    fn new(run: &Run<'_>, selected: Vec<usize>, seed: u64) -> Self {
        let state = AnnealingState::new(run.data, run.goals, selected);
        Self {
            best: state.selected.clone(),
            best_fitness: state.fitness,
            tabu: Tabu::new(run.data.candidates.len(), run.config.tabu_tenure),
            state,
            rng: StdRng::seed_from_u64(seed),
            stagnant: 0,
            proposals: 0,
            accepted: 0,
            #[cfg(feature = "visualizer")]
            last_accepted: None,
        }
    }

    fn step(&mut self, search: &Search<'_>, temperature: f64) -> bool {
        let context = StepContext {
            temperature,
            heat_bath: search.config.heat_bath,
            tabu: self.tabu.as_ref(),
            aspiration: self.best_fitness,
        };
        let Some(proposal) = search.propose(&mut self.state, context, &mut self.rng) else {
            return false;
        };
        self.proposals += 1;
        let delta = search.energy(proposal.fitness) - search.energy(self.state.fitness);
        if accept(delta, temperature, &mut self.rng) {
            proposal.apply(search.data, search.goals, &mut self.state);
            self.accepted += 1;
            if let Some(tabu) = &mut self.tabu {
                for exchange in proposal.exchanges() {
                    tabu.remember(exchange.removed);
                }
            }
            #[cfg(feature = "visualizer")]
            {
                self.last_accepted = Some(proposal);
            }
            debug_assert_eq!(self.state.fitness.serviced, proposal.fitness.serviced);
            debug_assert!((self.state.fitness.progress - proposal.fitness.progress).abs() <= 1e-6);
            if self.state.fitness.better_than(self.best_fitness) {
                self.best.clone_from(&self.state.selected);
                self.best_fitness = self.state.fitness;
                self.stagnant = 0;
                search.report(&self.best, self.best_fitness);
            }
        }
        if let Some(tabu) = &mut self.tabu {
            tabu.tick();
        }
        self.stagnant += 1;
        true
    }

    fn restart_from_best(&mut self, run: &Run<'_>) {
        self.state = AnnealingState::new(run.data, run.goals, self.best.clone());
        if let Some(tabu) = &mut self.tabu {
            tabu.clear();
        }
        self.stagnant = 0;
    }

    fn exchange(&mut self, other: &mut Self) {
        std::mem::swap(&mut self.state, &mut other.state);
        std::mem::swap(&mut self.tabu, &mut other.tabu);
        self.stagnant = 0;
        other.stagnant = 0;
    }
}

struct Tabu {
    until: Vec<u64>,
    tenure: u64,
    now: u64,
}

impl Tabu {
    fn new(candidates: usize, tenure: usize) -> Option<Self> {
        (tenure > 0).then(|| Self {
            until: vec![0; candidates],
            tenure: tenure as u64,
            now: 0,
        })
    }

    fn blocked(&self, candidate: usize) -> bool {
        self.until[candidate] > self.now
    }

    fn remember(&mut self, candidate: usize) {
        self.until[candidate] = self.now + self.tenure;
    }

    fn tick(&mut self) {
        self.now += 1;
    }

    fn clear(&mut self) {
        self.until.fill(0);
    }
}

fn accept(delta: f64, temperature: f64, rng: &mut StdRng) -> bool {
    delta >= -EPS || rng.random::<f64>() < (delta / temperature).exp()
}

fn search_energy(fitness: Fitness, progress_scale: f64) -> f64 {
    fitness.serviced as f64 + progress_scale * fitness.progress
}

#[derive(Clone, Copy)]
struct Exchange {
    remove_pos: usize,
    removed: usize,
    add: usize,
}

#[derive(Clone, Copy)]
struct Proposal {
    exchanges: [Exchange; 2],
    paired: bool,
    fitness: Fitness,
    #[cfg(feature = "visualizer")]
    target: Option<usize>,
}

impl Proposal {
    fn exchanges(&self) -> &[Exchange] {
        &self.exchanges[..1 + usize::from(self.paired)]
    }

    fn apply(self, data: &CoverageData, goals: &CoverageGoals, state: &mut AnnealingState) {
        for &exchange in self.exchanges() {
            state.apply_exchange(data, goals, exchange);
        }
    }
}

#[derive(Clone, Copy)]
struct StepContext<'a> {
    temperature: f64,
    heat_bath: bool,
    tabu: Option<&'a Tabu>,
    aspiration: Fitness,
}

impl StepContext<'_> {
    fn calibration(fitness: Fitness) -> Self {
        Self {
            temperature: 0.0,
            heat_bath: false,
            tabu: None,
            aspiration: fitness,
        }
    }
}

#[derive(Clone, Copy)]
struct Availability<'a> {
    used: &'a [bool],
    forbidden: Option<usize>,
}

impl Availability<'_> {
    fn allows(&self, candidate: usize) -> bool {
        !self.used[candidate] && Some(candidate) != self.forbidden
    }
}

struct Search<'a> {
    data: &'a CoverageData,
    goals: &'a CoverageGoals,
    config: &'a SimulatedAnnealingConfig,
    preferred_start: usize,
    progress: Option<&'a AnnealingReporter>,
    progress_scale: f64,
}

impl<'a> Search<'a> {
    fn new(run: &Run<'a>) -> Self {
        let total_progress: f64 = run.goals.needs.iter().sum();
        Self {
            data: run.data,
            goals: run.goals,
            config: run.config,
            preferred_start: run.preferred_start,
            progress: run.progress,
            progress_scale: if total_progress > 0.0 {
                0.5 / total_progress
            } else {
                0.0
            },
        }
    }

    fn energy(&self, fitness: Fitness) -> f64 {
        search_energy(fitness, self.progress_scale)
    }

    fn report(&self, selected: &[usize], fitness: Fitness) {
        let Some(progress) = self.progress else {
            return;
        };
        progress(AnnealingProgress {
            antennas: selected
                .iter()
                .map(|&candidate| self.data.candidates[candidate].position)
                .collect(),
            serviced: fitness.serviced,
        });
    }

    fn better(&self, candidate: Fitness, incumbent: Fitness) -> bool {
        self.energy(candidate) > self.energy(incumbent) + EPS
    }

    fn propose(
        &self,
        state: &mut AnnealingState,
        context: StepContext<'_>,
        rng: &mut StdRng,
    ) -> Option<Proposal> {
        if state.selected.len() >= 2
            && self.data.candidates.len() >= state.selected.len() + 2
            && rng.random::<f64>() < self.config.pair_probability
            && let Some(proposal) = self.pair(state, context, rng)
        {
            return Some(proposal);
        }
        self.single(state, None, context, rng)
    }

    fn pair(
        &self,
        state: &mut AnnealingState,
        context: StepContext<'_>,
        rng: &mut StdRng,
    ) -> Option<Proposal> {
        let target = self.target(state, rng);
        let first = target
            .and_then(|building| self.exchange(state, Some(building), None, context, rng))
            .or_else(|| self.single(state, None, context, rng))?;
        let first = first.exchanges[0];

        let fitness = state.fitness;
        state.apply_exchange(self.data, self.goals, first);
        let targeted = target
            .and_then(|building| self.exchange(state, Some(building), Some(first), context, rng));
        let exploratory = self.single(state, Some(first), context, rng);
        let second = match (targeted, exploratory) {
            (Some(a), Some(b)) if self.better(b.fitness, a.fitness) => Some(b),
            (Some(a), _) => Some(a),
            (None, b) => b,
        };
        state.apply_exchange(
            self.data,
            self.goals,
            Exchange {
                remove_pos: first.remove_pos,
                removed: first.add,
                add: first.removed,
            },
        );
        state.fitness = fitness;
        let second = second?;
        Some(Proposal {
            exchanges: [first, second.exchanges[0]],
            paired: true,
            fitness: second.fitness,
            #[cfg(feature = "visualizer")]
            target,
        })
    }

    fn single(
        &self,
        state: &AnnealingState,
        avoid: Option<Exchange>,
        context: StepContext<'_>,
        rng: &mut StdRng,
    ) -> Option<Proposal> {
        if rng.random::<f64>() < self.config.target_probability
            && let Some(target) = self.target(state, rng)
            && let Some(proposal) = self.exchange(state, Some(target), avoid, context, rng)
        {
            return Some(proposal);
        }
        self.exchange(state, None, avoid, context, rng)
    }

    fn exchange(
        &self,
        state: &AnnealingState,
        target: Option<usize>,
        avoid: Option<Exchange>,
        context: StepContext<'_>,
        rng: &mut StdRng,
    ) -> Option<Proposal> {
        let mut removal = None;
        for _ in 0..target.map_or(1, |_| REMOVAL_SAMPLES) {
            let pos = random_remove_pos(state.selected.len(), avoid.map(|e| e.remove_pos), rng)?;
            let base = RemovalBase::new(self.data, self.goals, state, pos);
            if removal
                .as_ref()
                .is_none_or(|(_, current): &(usize, RemovalBase)| {
                    self.better(base.fitness, current.fitness)
                })
            {
                removal = Some((pos, base));
            }
        }
        let (remove_pos, base) = removal?;
        let removed = state.selected[remove_pos];
        let pool = target.map(|building| self.data.by_building[building].as_slice());
        let preferred = pool.map_or(self.preferred_start, |viewers| {
            viewers.partition_point(|&(candidate, _)| candidate < self.preferred_start)
        });
        let available = Availability {
            used: &state.used,
            forbidden: avoid.map(|exchange| exchange.removed),
        };
        let trial = |add: usize| Proposal {
            exchanges: [Exchange {
                remove_pos,
                removed,
                add,
            }; 2],
            paired: false,
            fitness: base.after_adding(self.data, self.goals, state, add),
            #[cfg(feature = "visualizer")]
            target,
        };

        let direct = !context.heat_bath && context.tabu.is_none();
        let mut best = None;
        let mut trials = if direct {
            Vec::new()
        } else {
            Vec::with_capacity(self.config.proposal_samples)
        };
        let mut consider = |proposal: Proposal| {
            if direct {
                if best
                    .is_none_or(|current: Proposal| self.better(proposal.fitness, current.fitness))
                {
                    best = Some(proposal);
                }
            } else {
                trials.push(proposal);
            }
        };
        match pool.filter(|viewers| context.heat_bath && viewers.len() <= HEAT_BATH_LIMIT) {
            Some(viewers) => viewers
                .iter()
                .map(|&(candidate, _)| candidate)
                .filter(|&add| available.allows(add))
                .map(&trial)
                .for_each(&mut consider),
            None => {
                for sample in 0..self.config.proposal_samples {
                    let start = if sample * 2 < self.config.proposal_samples {
                        preferred
                    } else {
                        0
                    };
                    let add = random_available(pool, start, available, rng)
                        .or_else(|| random_available(pool, 0, available, rng))?;
                    consider(trial(add));
                }
            }
        }
        if direct {
            best
        } else {
            self.select(&trials, context, rng)
        }
    }

    fn select(
        &self,
        trials: &[Proposal],
        context: StepContext<'_>,
        rng: &mut StdRng,
    ) -> Option<Proposal> {
        let permitted = |proposal: &Proposal| {
            context.tabu.is_none_or(|tabu| {
                !tabu.blocked(proposal.exchanges[0].add)
                    || proposal.fitness.better_than(context.aspiration)
            })
        };
        let starved = !trials.iter().any(&permitted);
        let eligible = || trials.iter().filter(|&p| starved || permitted(p));
        if context.heat_bath && context.temperature > 0.0 {
            return self.boltzmann(eligible(), context.temperature, rng);
        }
        eligible().copied().reduce(|best, proposal| {
            if self.better(proposal.fitness, best.fitness) {
                proposal
            } else {
                best
            }
        })
    }

    fn boltzmann<'p>(
        &self,
        trials: impl Iterator<Item = &'p Proposal>,
        temperature: f64,
        rng: &mut StdRng,
    ) -> Option<Proposal> {
        let mut best: Option<(f64, Proposal)> = None;
        for proposal in trials {
            let key = self.energy(proposal.fitness) / temperature + gumbel(rng);
            if best.is_none_or(|(current, _)| key > current) {
                best = Some((key, *proposal));
            }
        }
        best.map(|(_, proposal)| proposal)
    }

    fn target(&self, state: &AnnealingState, rng: &mut StdRng) -> Option<usize> {
        let mut best = None;
        let mut best_fraction = -1.0;
        for _ in 0..TARGET_SAMPLES {
            let candidate = state.selected[rng.random_range(0..state.selected.len())];
            let view = &self.data.visibility[candidate];
            if view.is_empty() {
                continue;
            }
            let building = view[rng.random_range(0..view.len())].0 as usize;
            let need = self.goals.needs[building];
            if need <= 0.0
                || state.measures[building] >= need
                || self.data.by_building[building].is_empty()
            {
                continue;
            }
            let fraction = state.measures[building] / need;
            if fraction > best_fraction {
                best = Some(building);
                best_fraction = fraction;
            }
        }
        best
    }
}

fn random_remove_pos(selected: usize, excluded: Option<usize>, rng: &mut StdRng) -> Option<usize> {
    if selected == usize::from(excluded.is_some()) {
        return None;
    }
    loop {
        let pos = rng.random_range(0..selected);
        if Some(pos) != excluded {
            return Some(pos);
        }
    }
}

fn gumbel(rng: &mut StdRng) -> f64 {
    -(-rng.random::<f64>().max(f64::MIN_POSITIVE).ln()).ln()
}

fn random_available(
    pool: Option<&[(usize, f64)]>,
    start: usize,
    available: Availability<'_>,
    rng: &mut StdRng,
) -> Option<usize> {
    let len = pool.map_or(available.used.len(), <[_]>::len);
    if start >= len {
        return None;
    }
    let at = |index: usize| pool.map_or(index, |viewers| viewers[index].0);
    for _ in 0..32 {
        let candidate = at(rng.random_range(start..len));
        if available.allows(candidate) {
            return Some(candidate);
        }
    }
    let mut chosen = None;
    for (seen, candidate) in (start..len)
        .map(at)
        .filter(|&candidate| available.allows(candidate))
        .enumerate()
    {
        if rng.random_range(0..=seen) == 0 {
            chosen = Some(candidate);
        }
    }
    chosen
}

struct AnnealingState {
    selected: Vec<usize>,
    used: Vec<bool>,
    selected_by_building: Vec<Vec<usize>>,
    covered: Vec<IntervalSet>,
    measures: Vec<f64>,
    fitness: Fitness,
}

impl AnnealingState {
    fn new(data: &CoverageData, goals: &CoverageGoals, selected: Vec<usize>) -> Self {
        let used = used_mask(data.candidates.len(), &selected);
        let mut selected_by_building = vec![Vec::new(); goals.needs.len()];
        for &candidate in &selected {
            for (building, _) in &data.visibility[candidate] {
                selected_by_building[*building as usize].push(candidate);
            }
        }
        let coverage = accumulated(data, goals, selected.iter().copied());
        Self {
            selected,
            used,
            selected_by_building,
            covered: coverage.acc,
            measures: coverage.covered,
            fitness: coverage.fitness,
        }
    }

    fn apply_exchange(&mut self, data: &CoverageData, goals: &CoverageGoals, exchange: Exchange) {
        assert_eq!(
            self.selected[exchange.remove_pos], exchange.removed,
            "proposal removal no longer matches state"
        );
        assert!(
            !self.used[exchange.add],
            "proposal addition is already selected"
        );
        let removed = std::mem::replace(&mut self.selected[exchange.remove_pos], exchange.add);
        self.used[removed] = false;
        self.used[exchange.add] = true;

        let mut affected = Vec::with_capacity(
            data.visibility[removed].len() + data.visibility[exchange.add].len(),
        );
        for (building, _) in &data.visibility[removed] {
            let candidates = &mut self.selected_by_building[*building as usize];
            let pos = candidates
                .iter()
                .position(|&candidate| candidate == removed)
                .expect("removed candidate missing from building index");
            candidates.swap_remove(pos);
            affected.push(*building);
        }
        for (building, _) in &data.visibility[exchange.add] {
            self.selected_by_building[*building as usize].push(exchange.add);
            affected.push(*building);
        }
        affected.sort_unstable();
        affected.dedup();

        for building in affected {
            let building = building as usize;
            let old_measure = self.measures[building];
            let replacement =
                union_for_building(data, &self.selected_by_building[building], building, None);
            let new_measure = replacement.measure();
            self.fitness
                .replace_coverage(goals.needs[building], old_measure, new_measure);
            self.covered[building] = replacement;
            self.measures[building] = new_measure;
        }
    }
}

struct RemovedBuilding {
    building: u32,
    covered: IntervalSet,
    measure: f64,
}

struct RemovalBase {
    changed: Vec<RemovedBuilding>,
    fitness: Fitness,
}

impl RemovalBase {
    fn new(
        data: &CoverageData,
        goals: &CoverageGoals,
        state: &AnnealingState,
        remove_pos: usize,
    ) -> Self {
        let removed = state.selected[remove_pos];
        let mut fitness = state.fitness;
        let mut changed = Vec::with_capacity(data.visibility[removed].len());
        for (building, _) in &data.visibility[removed] {
            let building_index = *building as usize;
            let covered = union_for_building(
                data,
                &state.selected_by_building[building_index],
                building_index,
                Some(removed),
            );
            let measure = covered.measure();
            fitness.replace_coverage(
                goals.needs[building_index],
                state.measures[building_index],
                measure,
            );
            changed.push(RemovedBuilding {
                building: *building,
                covered,
                measure,
            });
        }
        Self { changed, fitness }
    }

    fn after_adding(
        &self,
        data: &CoverageData,
        goals: &CoverageGoals,
        state: &AnnealingState,
        add: usize,
    ) -> Fitness {
        let mut fitness = self.fitness;
        for (building, set) in &data.visibility[add] {
            let building_index = *building as usize;
            let (base, old_measure) = match self
                .changed
                .binary_search_by_key(building, |entry| entry.building)
            {
                Ok(pos) => (&self.changed[pos].covered, self.changed[pos].measure),
                Err(_) => (
                    &state.covered[building_index],
                    state.measures[building_index],
                ),
            };
            let new_measure = base.union_measure(set);
            fitness.replace_coverage(goals.needs[building_index], old_measure, new_measure);
        }
        fitness
    }
}

fn union_for_building(
    data: &CoverageData,
    candidates: &[usize],
    building: usize,
    excluded: Option<usize>,
) -> IntervalSet {
    let mut covered = IntervalSet::new();
    let building = building as u32;
    for &candidate in candidates
        .iter()
        .filter(|&&candidate| Some(candidate) != excluded)
    {
        let view = &data.visibility[candidate];
        let pos = view
            .binary_search_by_key(&building, |(index, _)| *index)
            .expect("candidate missing from building index");
        covered.union_with(&view[pos].1);
    }
    covered
}

#[cfg(feature = "visualizer")]
const L_SA_COVERAGE: &str = "sa coverage";
#[cfg(feature = "visualizer")]
const L_SA_TARGET: &str = "sa target";
#[cfg(feature = "visualizer")]
const L_SA_CURRENT: &str = "sa current";
#[cfg(feature = "visualizer")]
const L_SA_BEST: &str = "sa best";
#[cfg(feature = "visualizer")]
const L_SA_MOVE: &str = "sa move";
#[cfg(feature = "visualizer")]
const SA_LAYERS: [&str; 5] = [
    L_SA_COVERAGE,
    L_SA_TARGET,
    L_SA_CURRENT,
    L_SA_BEST,
    L_SA_MOVE,
];

#[cfg(feature = "visualizer")]
struct AnnealingVisualization<'a> {
    canvas: &'a dyn visualizer::Canvas,
    instance: &'a Instance,
}

#[cfg(feature = "visualizer")]
impl<'a> AnnealingVisualization<'a> {
    fn new(canvas: &'a dyn visualizer::Canvas, instance: &'a Instance) -> Self {
        for layer in SA_LAYERS {
            canvas.clear(layer);
        }
        Self { canvas, instance }
    }

    fn draw(
        &self,
        data: &CoverageData,
        chain: &Chain,
        iteration: usize,
        iterations: usize,
        temperature: f64,
        initial_temperature: f64,
    ) {
        self.draw_chain(
            data,
            chain,
            &chain.best,
            chain.best_fitness,
            iteration,
            iterations,
            temperature,
            initial_temperature,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_ladder(
        &self,
        data: &CoverageData,
        replicas: &[Chain],
        iteration: usize,
        iterations: usize,
        coldest: f64,
        hottest: f64,
    ) {
        let (best, best_fitness) = replicas
            .iter()
            .map(|chain| (&chain.best, chain.best_fitness))
            .reduce(|incumbent, challenger| {
                if challenger.1.better_than(incumbent.1) {
                    challenger
                } else {
                    incumbent
                }
            })
            .expect("at least one replica");
        self.draw_chain(
            data,
            &replicas[0],
            best,
            best_fitness,
            iteration,
            iterations,
            coldest,
            hottest,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_chain(
        &self,
        data: &CoverageData,
        chain: &Chain,
        best: &[usize],
        best_fitness: Fitness,
        iteration: usize,
        iterations: usize,
        temperature: f64,
        initial_temperature: f64,
    ) {
        use visualizer::{Color, DrawCommand, tessellate};

        let state = &chain.state;
        let accepted = chain.accepted;
        let last_accepted = chain.last_accepted;
        for layer in SA_LAYERS {
            self.canvas.clear(layer);
        }
        for (building_index, &measure) in state.measures.iter().enumerate() {
            let need = self.instance.tau * data.perimeters[building_index];
            if measure <= 0.0 || need <= 0.0 {
                continue;
            }
            let building = &self.instance.buildings[building_index];
            self.canvas.draw(DrawCommand::Polygon {
                vertices: building.vertices.clone(),
                triangles: tessellate(&building.vertices),
                fill: Some(visualizer::coverage_color(measure / need)),
                layer: L_SA_COVERAGE.into(),
            });
        }
        if let Some(target) = last_accepted.and_then(|proposal| proposal.target) {
            let building = &self.instance.buildings[target];
            self.canvas.draw(DrawCommand::Polygon {
                vertices: building.vertices.clone(),
                triangles: tessellate(&building.vertices),
                fill: Some(Color(255, 190, 40, 85)),
                layer: L_SA_TARGET.into(),
            });
        }

        for (selected, radius, color, layer) in [
            (
                state.selected.as_slice(),
                3.0,
                Color(80, 180, 255, 180),
                L_SA_CURRENT,
            ),
            (best, 4.5, Color::YELLOW, L_SA_BEST),
        ] {
            self.canvas.draw(DrawCommand::Points {
                positions: selected
                    .iter()
                    .map(|&candidate| data.candidates[candidate].position)
                    .collect(),
                radius,
                color,
                layer: layer.into(),
            });
        }

        let mut removed = Vec::new();
        let mut added = Vec::new();
        if let Some(proposal) = last_accepted {
            for exchange in proposal.exchanges() {
                removed.push(data.candidates[exchange.removed].position);
                added.push(data.candidates[exchange.add].position);
            }
        }
        if !removed.is_empty() {
            for (positions, color) in [
                (removed, Color(255, 70, 70, 235)),
                (added, Color(70, 255, 120, 235)),
            ] {
                self.canvas.draw(DrawCommand::Points {
                    positions,
                    radius: 7.0,
                    color,
                    layer: L_SA_MOVE.into(),
                });
            }
        }

        let temperature_ratio = if initial_temperature > 0.0 {
            (temperature / initial_temperature).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let target = last_accepted
            .and_then(|proposal| proposal.target)
            .map_or_else(
                || "global".into(),
                |building| format!("building {}", self.instance.buildings[building].id),
            );
        let move_kind = last_accepted.map_or("none", |proposal| match proposal.paired {
            false => "single exchange",
            true => "paired exchange",
        });
        let total = self.instance.buildings.len();
        self.canvas.draw(DrawCommand::Status {
            text: format!(
                "Simulated annealing\n\
                 iteration: {iteration} / {iterations}\n\
                 current serviced: {} / {}\n\
                 best serviced: {} / {}\n\
                 accepted moves: {accepted}\n\
                 temperature: {temperature:.4}\n\
                 last move: {move_kind}\n\
                target: {target}",
                state.fitness.serviced, total, best_fitness.serviced, total,
            ),
        });
        self.canvas.draw(DrawCommand::SetProgress {
            items: vec![
                (
                    format!("SA iteration {iteration}/{iterations}"),
                    iteration as f32 / iterations.max(1) as f32,
                ),
                (
                    format!("best serviced {}", best_fitness.serviced),
                    best_fitness.serviced as f32 / total.max(1) as f32,
                ),
                (
                    format!("current serviced {}", state.fitness.serviced),
                    state.fitness.serviced as f32 / total.max(1) as f32,
                ),
                (
                    format!("temperature {:.2}%", 100.0 * temperature_ratio),
                    temperature_ratio as f32,
                ),
            ],
        });
    }

    fn clear(&self) {
        for layer in SA_LAYERS {
            self.canvas.clear(layer);
        }
        self.canvas
            .draw(visualizer::DrawCommand::SetProgress { items: Vec::new() });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::candidates::UniformBoundary;
    use crate::coverage::build_coverage;
    use framework::test_utils::{serviced_ids, square};
    #[cfg(feature = "visualizer")]
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn vertices_only() -> CoverageConfig {
        CoverageConfig {
            candidate_spacing: 100.0,
            event_candidate_radius: 0.0,
            event_candidate_limit: 0,
        }
    }

    fn fast_config(seed: u64) -> SimulatedAnnealingConfig {
        SimulatedAnnealingConfig {
            iterations: 200,
            proposal_samples: 3,
            seed: Some(seed),
            ..SimulatedAnnealingConfig::default()
        }
    }

    fn two_squares(distance: f64, k: usize) -> Instance {
        Instance {
            buildings: vec![square(1, 0.0, 0.0), square(2, distance, 0.0)],
            k,
            tau: 0.75,
        }
    }

    fn state_fixture() -> (Instance, CoverageData, CoverageGoals, AnnealingState) {
        let instance = two_squares(20.0, 2);
        let data = build_coverage(&instance, &vertices_only());
        let goals = CoverageGoals::new(&data.perimeters, instance.tau);
        let selected = select_indices(&data, &goals, instance.k).0;
        let state = AnnealingState::new(&data, &goals, selected);
        (instance, data, goals, state)
    }

    fn assert_matches_rebuild(data: &CoverageData, goals: &CoverageGoals, state: &AnnealingState) {
        let rebuilt = AnnealingState::new(data, goals, state.selected.clone());
        assert_eq!(state.fitness.serviced, rebuilt.fitness.serviced);
        assert!((state.fitness.progress - rebuilt.fitness.progress).abs() <= 1e-9);
        assert_eq!(state.measures, rebuilt.measures);
    }

    #[test]
    fn an_improved_chain_reports_a_checkpoint() {
        let (_, data, goals, state) = state_fixture();
        let config = fast_config(53);
        let seen = Arc::new(AtomicUsize::new(0));
        let reporter: AnnealingReporter = {
            let seen = seen.clone();
            Arc::new(move |progress| {
                assert_eq!(progress.antennas.len(), 2);
                seen.store(progress.serviced + 1, Ordering::Relaxed);
            })
        };
        let run = Run {
            data: &data,
            goals: &goals,
            config: &config,
            preferred_start: data.candidates.len(),
            deadline: None,
            progress: Some(&reporter),
            quiet: true,
            #[cfg(feature = "visualizer")]
            visual: None,
        };
        let search = Search::new(&run);
        let mut chain = Chain::new(&run, state.selected, 53);
        chain.best_fitness = Fitness {
            serviced: 0,
            progress: -1.0,
        };
        assert!(chain.step(&search, f64::INFINITY));
        assert!(seen.load(Ordering::Relaxed) > 0);
    }

    #[test]
    fn fixed_seed_is_reproducible_and_preserves_cardinality() {
        let instance = two_squares(100.0, 3);
        let solver = || {
            SimulatedAnnealingSolver::new()
                .with_coverage_config(vertices_only())
                .with_config(fast_config(7))
                .solve(instance.clone())
        };
        let first = solver();
        let second = solver();
        assert_eq!(first.antennas, second.antennas);
        assert_eq!(first.antennas.len(), instance.k);
        for (i, antenna) in first.antennas.iter().enumerate() {
            assert!(!first.antennas[i + 1..].contains(antenna));
        }
    }

    fn tempered_config(seed: u64) -> SimulatedAnnealingConfig {
        SimulatedAnnealingConfig {
            chains: 4,
            swap_interval: 20,
            heat_bath: true,
            tabu_tenure: 6,
            reheat_after: 50,
            prune_dominated: true,
            ..fast_config(seed)
        }
    }

    #[test]
    fn the_default_config_is_the_original_single_chain() {
        let config = SimulatedAnnealingConfig::default();
        assert_eq!(config.chains, 1);
        assert_eq!(config.swap_interval, 0);
        assert!(!config.heat_bath);
        assert_eq!(config.pair_probability, 0.1);
        assert_eq!(config.target_probability, 0.85);
        assert_eq!(config.tabu_tenure, 0);
        assert_eq!(config.reheat_after, 0);
        assert!(!config.prune_dominated);
    }

    #[test]
    fn tempering_is_reproducible_and_preserves_cardinality() {
        let instance = two_squares(100.0, 3);
        let solver = || {
            SimulatedAnnealingSolver::new()
                .with_coverage_config(vertices_only())
                .with_config(tempered_config(13))
                .solve(instance.clone())
        };
        let first = solver();
        let second = solver();
        assert_eq!(first.antennas, second.antennas);
        assert_eq!(first.antennas.len(), instance.k);
        for (i, antenna) in first.antennas.iter().enumerate() {
            assert!(!first.antennas[i + 1..].contains(antenna));
        }
    }

    #[test]
    fn independent_chains_are_reproducible_and_preserve_cardinality() {
        let instance = two_squares(100.0, 3);
        let solver = || {
            SimulatedAnnealingSolver::new()
                .with_coverage_config(vertices_only())
                .with_config(SimulatedAnnealingConfig {
                    swap_interval: 0,
                    ..tempered_config(17)
                })
                .solve(instance.clone())
        };
        assert_eq!(solver().antennas, solver().antennas);
        assert_eq!(solver().antennas.len(), instance.k);
    }

    #[test]
    fn tempering_is_no_worse_than_greedy() {
        let instance = two_squares(100.0, 3);
        let greedy = crate::solvers::GreedyCoverageSolver::new()
            .with_coverage_config(vertices_only())
            .solve(instance.clone());
        let tempered = SimulatedAnnealingSolver::new()
            .with_coverage_config(vertices_only())
            .with_config(tempered_config(19))
            .solve(instance.clone());
        assert!(serviced_ids(&instance, &tempered).len() >= serviced_ids(&instance, &greedy).len());
    }

    #[test]
    fn tempering_survives_the_pool_growing_under_it() {
        let instance = two_squares(100.0, 3);
        let solution = SimulatedAnnealingSolver::new()
            .with_coverage_config(vertices_only())
            .with_config(SimulatedAnnealingConfig {
                local_spacing: 5.0,
                probe_rings: 3,
                threshold_breakpoints: true,
                ..tempered_config(29)
            })
            .solve(instance.clone());
        assert_eq!(solution.antennas.len(), instance.k);
    }

    #[test]
    fn returned_incumbent_is_no_worse_than_greedy() {
        let instance = two_squares(100.0, 3);
        let greedy = crate::solvers::GreedyCoverageSolver::new()
            .with_coverage_config(vertices_only())
            .solve(instance.clone());
        let annealed = SimulatedAnnealingSolver::new()
            .with_coverage_config(vertices_only())
            .with_config(fast_config(11))
            .solve(instance.clone());
        assert!(serviced_ids(&instance, &annealed).len() >= serviced_ids(&instance, &greedy).len());
    }

    #[test]
    fn refinement_passes_never_return_a_worse_placement() {
        let instance = two_squares(100.0, 3);
        let base = SimulatedAnnealingSolver::new()
            .with_coverage_config(vertices_only())
            .with_config(fast_config(23))
            .solve(instance.clone());
        let refined = SimulatedAnnealingSolver::new()
            .with_coverage_config(vertices_only())
            .with_config(SimulatedAnnealingConfig {
                local_spacing: 5.0,
                probe_rings: 3,
                threshold_breakpoints: true,
                ..fast_config(23)
            })
            .solve(instance.clone());
        assert_eq!(refined.antennas.len(), instance.k);
        assert!(serviced_ids(&instance, &refined).len() >= serviced_ids(&instance, &base).len());
    }

    #[test]
    fn a_custom_plan_replaces_the_config_implied_one() {
        let instance = two_squares(100.0, 2);
        let solution = SimulatedAnnealingSolver::new()
            .with_config(fast_config(5))
            .with_candidate_plan(
                CandidatePlan::new()
                    .with(CandidatePhase::Initial, UniformBoundary::new(100.0))
                    .with(CandidatePhase::Polish, ProbeRings::new(2, 8)),
            )
            .solve(instance.clone());
        assert_eq!(solution.antennas.len(), instance.k);
    }

    #[test]
    fn incremental_exchange_matches_full_rebuild() {
        let (_, data, goals, mut state) = state_fixture();
        let add = (0..data.candidates.len())
            .find(|&candidate| !state.used[candidate])
            .unwrap();
        state.apply_exchange(
            &data,
            &goals,
            Exchange {
                remove_pos: 0,
                removed: state.selected[0],
                add,
            },
        );

        assert_matches_rebuild(&data, &goals, &state);
    }

    #[test]
    fn geometric_schedule_reaches_requested_ratio() {
        let initial = 12.0;
        let schedule = Schedule::new(initial, 0.01, 100.0);
        assert_eq!(schedule.temperature(0.0), initial);
        assert!((schedule.temperature(100.0) - 0.12).abs() <= 1e-12);
    }

    #[test]
    fn a_reheat_restarts_the_cycle_over_what_is_left() {
        let mut schedule = Schedule::new(12.0, 0.01, 100.0);
        schedule.reheat(50.0, 0.5);
        assert!((schedule.temperature(50.0) - 6.0).abs() <= 1e-12);
        assert!((schedule.temperature(100.0) - 0.06).abs() <= 1e-12);
    }

    #[test]
    fn the_ladder_spans_its_ratio_coldest_first() {
        let ladder = temperature_ladder(1.0, 5, 0.0625);
        assert!((ladder[0] - 0.0625).abs() <= 1e-12);
        assert!((ladder[4] - 1.0).abs() <= 1e-12);
        assert!(ladder.windows(2).all(|rungs| rungs[0] < rungs[1]));
        let gap = ladder[1] / ladder[0];
        assert!(
            ladder
                .windows(2)
                .all(|r| (r[1] / r[0] - gap).abs() <= 1e-12)
        );
        assert!((gap - 2.0).abs() <= 1e-12);
    }

    #[test]
    fn tabu_bars_a_removed_candidate_for_its_tenure() {
        let mut tabu = Tabu::new(4, 2).unwrap();
        tabu.remember(1);
        assert!(tabu.blocked(1) && !tabu.blocked(0));
        tabu.tick();
        assert!(tabu.blocked(1));
        tabu.tick();
        assert!(!tabu.blocked(1));
        assert!(Tabu::new(4, 0).is_none());
    }

    #[test]
    fn a_cold_heat_bath_concentrates_on_the_best_move() {
        let (_, data, goals, state) = state_fixture();
        let config = fast_config(41);
        let search = Search {
            data: &data,
            goals: &goals,
            config: &config,
            preferred_start: data.candidates.len(),
            progress: None,
            progress_scale: 1.0,
        };
        let trials: Vec<Proposal> = [0.0, 1.0, 2.0]
            .into_iter()
            .enumerate()
            .map(|(add, progress)| Proposal {
                exchanges: [Exchange {
                    remove_pos: 0,
                    removed: state.selected[0],
                    add,
                }; 2],
                paired: false,
                fitness: Fitness {
                    serviced: 0,
                    progress,
                },
                #[cfg(feature = "visualizer")]
                target: None,
            })
            .collect();
        let context = |temperature| StepContext {
            temperature,
            heat_bath: true,
            tabu: None,
            aspiration: Fitness {
                serviced: 0,
                progress: 0.0,
            },
        };
        let mut rng = StdRng::seed_from_u64(41);
        let mut picks = |temperature| -> Vec<usize> {
            (0..64)
                .filter_map(|_| search.select(&trials, context(temperature), &mut rng))
                .map(|proposal| proposal.exchanges[0].add)
                .collect()
        };
        assert!(picks(1e-4).iter().all(|&add| add == 2));
        assert!(picks(100.0).iter().any(|&add| add != 2));
    }

    #[test]
    fn tabu_yields_to_a_move_that_beats_the_best() {
        let (_, data, goals, state) = state_fixture();
        let config = fast_config(43);
        let search = Search {
            data: &data,
            goals: &goals,
            config: &config,
            preferred_start: data.candidates.len(),
            progress: None,
            progress_scale: 0.0,
        };
        let trial = |add, serviced| Proposal {
            exchanges: [Exchange {
                remove_pos: 0,
                removed: state.selected[0],
                add,
            }; 2],
            paired: false,
            fitness: Fitness {
                serviced,
                progress: 0.0,
            },
            #[cfg(feature = "visualizer")]
            target: None,
        };
        let mut tabu = Tabu::new(data.candidates.len(), 5).unwrap();
        tabu.remember(1);
        tabu.remember(2);
        let mut rng = StdRng::seed_from_u64(43);
        let mut pick = |trials: &[Proposal], aspiration| {
            search
                .select(
                    trials,
                    StepContext {
                        temperature: 1.0,
                        heat_bath: false,
                        tabu: Some(&tabu),
                        aspiration: Fitness {
                            serviced: aspiration,
                            progress: 0.0,
                        },
                    },
                    &mut rng,
                )
                .map(|proposal| proposal.exchanges[0].add)
        };
        assert_eq!(pick(&[trial(1, 3), trial(0, 1)], 3), Some(0));
        assert_eq!(pick(&[trial(1, 5), trial(0, 1)], 3), Some(1));
        assert_eq!(pick(&[trial(1, 1), trial(2, 2)], 3), Some(2));
    }

    #[test]
    fn search_energy_prioritizes_a_serviced_building() {
        assert!(
            search_energy(
                Fitness {
                    serviced: 1,
                    progress: 0.0,
                },
                0.5 / 30.0,
            ) > search_energy(
                Fitness {
                    serviced: 0,
                    progress: 30.0,
                },
                0.5 / 30.0,
            )
        );
    }

    #[test]
    fn refinement_proposals_sample_the_new_suffix() {
        let (_, data, goals, _) = state_fixture();
        let preferred_start = data.candidates.len() / 2;
        let state = AnnealingState::new(&data, &goals, vec![0, 1]);
        let config = SimulatedAnnealingConfig {
            proposal_samples: 1,
            ..fast_config(31)
        };
        let search = Search {
            data: &data,
            goals: &goals,
            config: &config,
            preferred_start,
            progress: None,
            progress_scale: 0.0,
        };
        let proposal = search
            .exchange(
                &state,
                None,
                None,
                StepContext::calibration(state.fitness),
                &mut StdRng::seed_from_u64(31),
            )
            .unwrap();
        assert!(proposal.exchanges[0].add >= preferred_start);
    }

    #[test]
    fn paired_proposal_restores_the_state_after_its_trial_move() {
        let (_, data, goals, mut state) = state_fixture();
        let selected = state.selected.clone();
        let config = fast_config(37);
        let search = Search {
            data: &data,
            goals: &goals,
            config: &config,
            preferred_start: data.candidates.len(),
            progress: None,
            progress_scale: 0.0,
        };
        let mut rng = StdRng::seed_from_u64(37);
        let mut tabu = Tabu::new(data.candidates.len(), 4).unwrap();
        tabu.remember(state.selected[0]);
        for _ in 0..100 {
            let context = StepContext {
                temperature: 1.0,
                heat_bath: true,
                tabu: Some(&tabu),
                aspiration: state.fitness,
            };
            assert!(search.pair(&mut state, context, &mut rng).is_some());
        }
        assert_eq!(state.selected, selected);
        assert_eq!(state.used, used_mask(data.candidates.len(), &selected));
        assert_matches_rebuild(&data, &goals, &state);
    }

    #[test]
    fn paired_exchange_matches_full_rebuild() {
        let (_, data, goals, mut state) = state_fixture();
        let additions: Vec<_> = (0..data.candidates.len())
            .filter(|&candidate| !state.used[candidate])
            .take(2)
            .collect();
        let first = Exchange {
            remove_pos: 0,
            removed: state.selected[0],
            add: additions[0],
        };
        state.apply_exchange(&data, &goals, first);
        let second = Exchange {
            remove_pos: 1,
            removed: state.selected[1],
            add: additions[1],
        };
        state.apply_exchange(&data, &goals, second);

        assert_matches_rebuild(&data, &goals, &state);
    }

    #[cfg(feature = "visualizer")]
    #[derive(Default)]
    struct RecordingCanvas {
        commands: Mutex<Vec<visualizer::DrawCommand>>,
    }

    #[cfg(feature = "visualizer")]
    impl visualizer::Canvas for RecordingCanvas {
        fn draw(&self, command: visualizer::DrawCommand) {
            self.commands.lock().unwrap().push(command);
        }

        fn clear(&self, layer: &str) {
            self.draw(visualizer::DrawCommand::ClearLayer {
                layer: layer.into(),
            });
        }
    }

    #[cfg(feature = "visualizer")]
    #[test]
    fn visualizer_emits_search_layers_and_status() {
        let (instance, data, goals, state) = state_fixture();
        let selected = state.selected.clone();
        let add = (0..data.candidates.len())
            .find(|&candidate| !state.used[candidate])
            .unwrap();
        let config = fast_config(47);
        let run = Run {
            data: &data,
            goals: &goals,
            config: &config,
            preferred_start: data.candidates.len(),
            deadline: None,
            progress: None,
            quiet: true,
            visual: None,
        };
        let mut chain = Chain::new(&run, selected.clone(), 47);
        chain.accepted = 3;
        chain.last_accepted = Some(Proposal {
            exchanges: [Exchange {
                remove_pos: 0,
                removed: selected[0],
                add,
            }; 2],
            paired: false,
            fitness: state.fitness,
            target: Some(0),
        });
        let canvas = RecordingCanvas::default();
        let visual = AnnealingVisualization::new(&canvas, &instance);
        visual.draw(&data, &chain, 5, 10, 0.5, 1.0);

        let commands = canvas.commands.lock().unwrap();
        assert!(commands.iter().any(|command| matches!(
            command,
            visualizer::DrawCommand::Polygon { layer, .. } if layer == L_SA_COVERAGE
        )));
        assert!(commands.iter().any(|command| matches!(
            command,
            visualizer::DrawCommand::Polygon { layer, .. } if layer == L_SA_TARGET
        )));
        for expected in [L_SA_CURRENT, L_SA_BEST, L_SA_MOVE] {
            assert!(commands.iter().any(|command| matches!(
                command,
                visualizer::DrawCommand::Points { layer, .. } if layer == expected
            )));
        }
        assert!(commands.iter().any(|command| matches!(
            command,
            visualizer::DrawCommand::Status { text } if text.contains("best serviced")
        )));
        assert!(commands.iter().any(|command| matches!(
            command,
            visualizer::DrawCommand::SetProgress { items } if items.len() == 4
        )));
    }
}

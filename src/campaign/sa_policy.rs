use std::collections::HashSet;

use super::manifest::Manifest;
use super::policy::{Policy, Proposal, lane, mix, scan_from};
use super::trial::{Trial, TrialConfig, TrialResult};

pub const SA_BUDGETS: [usize; 3] = [15 * 60, 60 * 60, 4 * 60 * 60];

const REDUCTION_FACTOR: usize = 4;
const DEFAULT_COHORT_SIZE: usize = 64;
const RECENT_TRIALS: usize = 12;
const STARVATION_FLOOR: f64 = 0.2;
pub const EXPLORE_END: f64 = 0.60;
pub const STARTUP_ALLOWANCE_SECONDS: usize = 3 * 60;

#[derive(Clone, Debug)]
pub struct SaLadder {
    pub budgets: Vec<usize>,
}

impl Default for SaLadder {
    fn default() -> Self {
        Self {
            budgets: SA_BUDGETS.to_vec(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SaPhase {
    Explore,
    Exploit,
    Polish,
}

pub struct SaPolicy {
    pub config: TrialConfig,
    pub spacings: Vec<f64>,
    pub ladder: SaLadder,
    pub cohort_size: usize,
    pub max_trials: usize,
}

impl Default for SaPolicy {
    fn default() -> Self {
        Self {
            config: TrialConfig::default(),
            spacings: vec![TrialConfig::default().spacing],
            ladder: SaLadder::default(),
            cohort_size: DEFAULT_COHORT_SIZE,
            max_trials: usize::MAX,
        }
    }
}

impl SaPolicy {
    pub fn with_spacing(self, spacing: f64) -> Self {
        self.with_spacings(vec![spacing])
    }

    pub fn with_spacings(mut self, mut spacings: Vec<f64>) -> Self {
        assert!(!spacings.is_empty(), "SA needs at least one spacing");
        assert!(
            spacings
                .iter()
                .all(|spacing| spacing.is_finite() && *spacing > 0.0),
            "SA spacings must be positive and finite"
        );
        spacings.sort_by(f64::total_cmp);
        spacings.dedup_by(|a, b| same_spacing(*a, *b));
        self.config.spacing = spacings[0];
        self.spacings = spacings;
        self
    }

    pub fn with_budgets(mut self, budgets: Vec<usize>) -> Self {
        assert!(!budgets.is_empty(), "SA needs at least one time budget");
        assert!(
            budgets.windows(2).all(|pair| pair[0] < pair[1]),
            "SA budgets must be strictly increasing"
        );
        self.ladder.budgets = budgets;
        self
    }

    pub fn with_cohort_size(mut self, cohort_size: usize) -> Self {
        assert!(cohort_size > 0, "SA cohort must be positive");
        self.cohort_size = cohort_size;
        self
    }

    pub fn with_pair_probability(mut self, probability: f64) -> Self {
        assert!(
            (0.0..=1.0).contains(&probability),
            "SA pair probability must be in [0, 1]"
        );
        self.config.sa_pair_probability = probability;
        self
    }

    pub fn with_target_probability(mut self, probability: f64) -> Self {
        assert!(
            (0.0..=1.0).contains(&probability),
            "SA target probability must be in [0, 1]"
        );
        self.config.sa_target_probability = probability;
        self
    }

    pub fn phase(&self, manifest: &Manifest) -> SaPhase {
        match search_fraction(manifest) {
            fraction if fraction < EXPLORE_END => SaPhase::Explore,
            _ if Self::available_seconds(manifest)
                >= self.ladder.budgets[0].saturating_add(STARTUP_ALLOWANCE_SECONDS) =>
            {
                SaPhase::Exploit
            }
            _ => SaPhase::Polish,
        }
    }

    fn available_seconds(manifest: &Manifest) -> usize {
        if manifest.deadline_unix == 0 {
            return usize::MAX;
        }
        manifest
            .deadline_unix
            .saturating_sub(manifest.reserve_seconds)
            .saturating_sub(super::worker::now_unix()) as usize
    }

    fn fits(&self, trial: &Trial, manifest: &Manifest) -> bool {
        trial
            .remaining_iterations()
            .saturating_add(STARTUP_ALLOWANCE_SECONDS)
            <= Self::available_seconds(manifest)
    }

    fn spacing_for_worker(&self, worker_lane: u64, cached: Option<f64>) -> f64 {
        cached
            .filter(|value| self.spacings.iter().any(|s| same_spacing(*s, *value)))
            .unwrap_or(self.spacings[worker_lane as usize % self.spacings.len()])
    }

    fn cohort_results<'a>(
        &self,
        results: &'a [TrialResult],
        scenario: &str,
        spacing: f64,
        rung: usize,
        cohort: usize,
    ) -> Vec<&'a TrialResult> {
        let first = cohort as u64 * self.cohort_size as u64;
        let end = first + self.cohort_size as u64;
        results
            .iter()
            .filter(|result| {
                result.trial.scenario == scenario
                    && result.trial.rung == rung
                    && same_spacing(result.trial.config.spacing, spacing)
                    && (first..end).contains(&result.trial.seed)
            })
            .collect()
    }

    fn expected_at_rung(&self, rung: usize) -> usize {
        (0..rung).fold(self.cohort_size, |count, _| survivor_count(count))
    }

    fn ranked_survivors<'a>(&self, mut source: Vec<&'a TrialResult>) -> Vec<&'a TrialResult> {
        source.sort_unstable_by(|a, b| {
            b.serviced
                .cmp(&a.serviced)
                .then_with(|| a.trial.id.cmp(&b.trial.id))
        });
        source.truncate(survivor_count(source.len()));
        source
    }

    fn promote_cohort(
        &self,
        manifest: &Manifest,
        results: &[TrialResult],
        key: (&str, f64, usize, usize),
        avoid: &HashSet<String>,
        worker_lane: u64,
    ) -> Option<Trial> {
        let (scenario, spacing, cohort, source_rung) = key;
        let target_rung = source_rung + 1;
        if target_rung >= self.ladder.budgets.len() {
            return None;
        }
        let source = self.cohort_results(results, scenario, spacing, source_rung, cohort);
        if source.len() != self.expected_at_rung(source_rung) {
            return None;
        }
        let survivors = self.ranked_survivors(source);
        for index in scan_from(worker_lane, survivors.len()) {
            let below = &survivors[index].trial;
            let already_done = results.iter().any(|result| {
                result.trial.scenario == scenario
                    && result.trial.rung == target_rung
                    && result.trial.seed == below.seed
                    && same_spacing(result.trial.config.spacing, spacing)
            });
            if already_done {
                continue;
            }
            let trial = Trial::new(
                scenario,
                below.config,
                target_rung,
                self.ladder.budgets[target_rung],
                below.seed,
            )
            .resuming(below);
            if self.fits(&trial, manifest) && !avoid.contains(&trial.id) {
                return Some(trial);
            }
        }
        None
    }

    fn cohorts_seen(&self, results: &[TrialResult], scenario: &str, spacing: f64) -> usize {
        results
            .iter()
            .filter(|result| {
                result.trial.scenario == scenario
                    && same_spacing(result.trial.config.spacing, spacing)
            })
            .map(|result| result.trial.seed as usize / self.cohort_size + 1)
            .max()
            .unwrap_or(0)
    }

    fn promotion(
        &self,
        manifest: &Manifest,
        results: &[TrialResult],
        scenario_order: &[String],
        spacing: f64,
        avoid: &HashSet<String>,
        worker_lane: u64,
    ) -> Option<Trial> {
        for source_rung in (0..self.ladder.budgets.len().saturating_sub(1)).rev() {
            for scenario in scenario_order {
                let cohorts = self.cohorts_seen(results, scenario, spacing);
                for cohort_index in scan_from(worker_lane, cohorts) {
                    if let Some(trial) = self.promote_cohort(
                        manifest,
                        results,
                        (scenario, spacing, cohort_index, source_rung),
                        avoid,
                        worker_lane,
                    ) {
                        return Some(trial);
                    }
                }
            }
        }
        None
    }

    fn explore(
        &self,
        manifest: &Manifest,
        results: &[TrialResult],
        scenario_order: &[String],
        spacing: f64,
        avoid: &HashSet<String>,
        worker_lane: u64,
    ) -> Option<Trial> {
        let mut config = self.config;
        config.spacing = spacing;
        for scenario in scenario_order {
            let cohorts = self.cohorts_seen(results, scenario, spacing).max(1);
            for cohort in 0..=cohorts {
                let complete = self.cohort_results(results, scenario, spacing, 0, cohort);
                if complete.len() == self.cohort_size {
                    continue;
                }
                let first = cohort as u64 * self.cohort_size as u64;
                for offset in scan_from(worker_lane, self.cohort_size) {
                    let seed = first + offset as u64;
                    if complete.iter().any(|result| result.trial.seed == seed) {
                        continue;
                    }
                    let trial = Trial::new(scenario, config, 0, self.ladder.budgets[0], seed);
                    if self.fits(&trial, manifest) && !avoid.contains(&trial.id) {
                        return Some(trial);
                    }
                }
                break;
            }
        }
        None
    }

    fn pick_scenario(
        &self,
        manifest: &Manifest,
        results: &[TrialResult],
        worker_lane: u64,
        draw: u64,
    ) -> Option<String> {
        let raw: Vec<(String, f64)> = manifest
            .scenarios
            .iter()
            .map(|scenario| {
                let mut mine: Vec<_> = results
                    .iter()
                    .filter(|result| result.trial.scenario == scenario.id)
                    .collect();
                mine.sort_unstable_by(|a, b| {
                    a.completed_unix_ms
                        .cmp(&b.completed_unix_ms)
                        .then_with(|| a.trial.id.cmp(&b.trial.id))
                });
                if mine.len() < 4 {
                    return (scenario.id.clone(), f64::NAN);
                }
                let first = mine.len().saturating_sub(RECENT_TRIALS);
                let best_before = mine[..first]
                    .iter()
                    .map(|result| result.serviced)
                    .max()
                    .unwrap_or(0);
                let best_after = mine[first..]
                    .iter()
                    .map(|result| result.serviced)
                    .fold(best_before, usize::max);
                let seconds: f64 = mine[first..]
                    .iter()
                    .map(|result| result.seconds.max(1.0))
                    .sum();
                let relative_gain = if best_after > 0 {
                    best_after.saturating_sub(best_before) as f64 / best_after as f64
                } else {
                    0.0
                };
                (scenario.id.clone(), relative_gain / seconds.max(1.0))
            })
            .collect();
        let peak = raw
            .iter()
            .filter(|(_, rate)| rate.is_finite())
            .map(|(_, rate)| *rate)
            .fold(0.0f64, f64::max);
        let weights: Vec<_> = raw
            .into_iter()
            .map(|(id, rate)| {
                let scaled = if !rate.is_finite() {
                    1.0
                } else if peak > 0.0 {
                    rate / peak
                } else {
                    0.0
                };
                (id, STARVATION_FLOOR + scaled)
            })
            .collect();
        let total: f64 = weights.iter().map(|(_, weight)| weight).sum();
        let mut pick = (mix(worker_lane, draw) % 1_000_000) as f64 / 1_000_000.0 * total;
        for (id, weight) in &weights {
            pick -= weight;
            if pick <= 0.0 {
                return Some(id.clone());
            }
        }
        weights.last().map(|(id, _)| id.clone())
    }

    fn solved(&self, manifest: &Manifest, results: &[TrialResult], scenario: &str) -> bool {
        manifest.is_solved(
            results
                .iter()
                .filter(|result| result.trial.scenario == scenario)
                .map(|result| result.serviced)
                .max()
                .unwrap_or(0),
        )
    }

    fn scenario_order(
        &self,
        manifest: &Manifest,
        results: &[TrialResult],
        worker_lane: u64,
        draw: u64,
    ) -> Vec<String> {
        let preferred = self
            .pick_scenario(manifest, results, worker_lane, draw)
            .filter(|id| !self.solved(manifest, results, id));
        let mut order = Vec::with_capacity(manifest.scenarios.len());
        if let Some(preferred) = preferred {
            order.push(preferred);
        }
        for index in scan_from(worker_lane, manifest.scenarios.len()) {
            let id = &manifest.scenarios[index].id;
            if !order.contains(id) && !self.solved(manifest, results, id) {
                order.push(id.clone());
            }
        }
        order
    }
}

impl Policy for SaPolicy {
    fn finished(&self, manifest: &Manifest, results: &[TrialResult]) -> bool {
        results.len() >= self.max_trials
            || Self::available_seconds(manifest)
                < self.ladder.budgets[0].saturating_add(STARTUP_ALLOWANCE_SECONDS)
            || manifest
                .scenarios
                .iter()
                .all(|scenario| self.solved(manifest, results, &scenario.id))
    }

    fn propose(
        &self,
        manifest: &Manifest,
        results: &[TrialResult],
        avoid: &HashSet<String>,
        worker: &str,
        cached_spacing: Option<f64>,
    ) -> Option<Proposal> {
        if self.finished(manifest, results) {
            return None;
        }
        let worker_lane = lane(worker);
        let spacing = self.spacing_for_worker(worker_lane, cached_spacing);
        let mine = results
            .iter()
            .filter(|result| result.worker == worker)
            .count() as u64;
        let scenarios = self.scenario_order(manifest, results, worker_lane, mine);

        if let Some(trial) =
            self.promotion(manifest, results, &scenarios, spacing, avoid, worker_lane)
        {
            return Some(Proposal {
                priority: 10 + trial.rung as u32,
                trial,
            });
        }

        if let Some(trial) =
            self.explore(manifest, results, &scenarios, spacing, avoid, worker_lane)
        {
            return Some(Proposal {
                trial,
                priority: 20,
            });
        }
        None
    }
}

fn search_fraction(manifest: &Manifest) -> f64 {
    if manifest.deadline_unix == 0 || manifest.deadline_unix <= manifest.start_unix {
        return 0.0;
    }
    let search_end = manifest
        .deadline_unix
        .saturating_sub(manifest.reserve_seconds);
    if search_end <= manifest.start_unix {
        return 1.0;
    }
    let elapsed = super::worker::now_unix().saturating_sub(manifest.start_unix);
    (elapsed as f64 / (search_end - manifest.start_unix) as f64).clamp(0.0, 1.0)
}

fn same_spacing(a: f64, b: f64) -> bool {
    a.to_bits() == b.to_bits()
}

fn survivor_count(field: usize) -> usize {
    field.div_ceil(REDUCTION_FACTOR).max(1)
}

#[cfg(test)]
mod tests {
    use super::super::manifest::Scenario;
    use super::*;

    fn manifest() -> Manifest {
        Manifest {
            dataset: "d.geojson".into(),
            scenarios: vec![Scenario::new(0.5, 100)],
            start_unix: 0,
            deadline_unix: 0,
            reserve_seconds: 0,
            duration_seconds: 0,
            spacings: vec![8.0],
            buildings: 1000,
        }
    }

    fn result_at(spacing: f64, rung: usize, seed: u64, score: usize) -> TrialResult {
        let config = TrialConfig {
            spacing,
            ..Default::default()
        };
        TrialResult {
            trial: Trial::new("t050-k100", config, rung, SA_BUDGETS[rung], seed),
            serviced: score,
            seconds: SA_BUDGETS[0] as f64,
            worker: "w".into(),
            completed_unix_ms: seed,
        }
    }

    #[test]
    fn cohort_does_not_promote_until_every_member_finishes() {
        let policy = SaPolicy::default().with_cohort_size(8);
        let results: Vec<_> = (0..7)
            .map(|seed| result_at(8.0, 0, seed, 100 - seed as usize))
            .collect();
        assert!(
            policy
                .promotion(
                    &manifest(),
                    &results,
                    &["t050-k100".into()],
                    8.0,
                    &HashSet::new(),
                    0,
                )
                .is_none()
        );
    }

    #[test]
    fn cohort_promotes_exactly_the_fixed_top_quarter() {
        let policy = SaPolicy::default().with_cohort_size(8);
        let results: Vec<_> = (0..8)
            .map(|seed| result_at(8.0, 0, seed, 100 - seed as usize))
            .collect();
        let first = policy
            .promotion(
                &manifest(),
                &results,
                &["t050-k100".into()],
                8.0,
                &HashSet::new(),
                0,
            )
            .unwrap();
        assert_eq!(first.rung, 1);
        assert!(matches!(first.seed, 0 | 1));
        assert_eq!(first.remaining_iterations(), SA_BUDGETS[1] - SA_BUDGETS[0]);

        let avoid = HashSet::from([first.id]);
        let second = policy
            .promotion(&manifest(), &results, &["t050-k100".into()], 8.0, &avoid, 0)
            .unwrap();
        assert!(matches!(second.seed, 0 | 1));
        assert_ne!(second.seed, first.seed);
    }

    #[test]
    fn spacing_lanes_do_not_compete_or_rebuild() {
        let policy = SaPolicy::default()
            .with_spacings(vec![4.0, 8.0, 16.0])
            .with_cohort_size(8);
        let proposal = policy
            .propose(&manifest(), &[], &HashSet::new(), "worker", Some(8.0))
            .unwrap();
        assert_eq!(proposal.trial.config.spacing, 8.0);
    }

    #[test]
    fn incomplete_cohort_is_filled_before_the_next_one_opens() {
        let policy = SaPolicy::default().with_cohort_size(8);
        let results = vec![result_at(8.0, 0, 0, 100)];
        let proposal = policy
            .propose(&manifest(), &results, &HashSet::new(), "worker", Some(8.0))
            .unwrap();
        assert!(proposal.trial.seed < 8);
    }

    #[test]
    fn slower_scenarios_keep_a_nonzero_allocation() {
        let mut m = manifest();
        m.scenarios.push(Scenario::new(0.25, 200));
        let mut results = Vec::new();
        for seed in 0..16 {
            results.push(result_at(8.0, 0, seed, 100 + seed as usize));
            let mut flat = result_at(8.0, 0, seed + 100, 50);
            flat.trial = Trial::new("t025-k200", TrialConfig::default(), 0, SA_BUDGETS[0], seed);
            results.push(flat);
        }
        let picked: HashSet<_> = (0..128)
            .filter_map(|draw| SaPolicy::default().pick_scenario(&m, &results, 7, draw))
            .collect();
        assert_eq!(picked.len(), 2);
    }

    #[test]
    fn exploit_tail_still_fills_idle_capacity_with_fresh_trials() {
        let now = super::super::worker::now_unix();
        let mut m = manifest();
        m.start_unix = now.saturating_sub(2 * 3600);
        m.deadline_unix = now + 3600;
        let policy = SaPolicy::default().with_cohort_size(8);
        assert_eq!(policy.phase(&m), SaPhase::Exploit);
        let proposal = policy
            .propose(&m, &[], &HashSet::new(), "worker", Some(8.0))
            .expect("the exploit tail should run another short trial");
        assert_eq!(proposal.trial.rung, 0);
    }

    #[test]
    fn policy_stops_before_a_seed_can_cross_the_search_deadline() {
        let now = super::super::worker::now_unix();
        let mut m = manifest();
        m.start_unix = now.saturating_sub(3600);
        m.deadline_unix = now + SA_BUDGETS[0] as u64 + 120;
        let policy = SaPolicy::default();
        assert_eq!(policy.phase(&m), SaPhase::Polish);
        assert!(policy.finished(&m, &[]));
        assert!(
            policy
                .propose(&m, &[], &HashSet::new(), "worker", Some(8.0))
                .is_none()
        );
    }

    #[test]
    fn a_fully_serviced_scenario_stops_being_proposed() {
        let mut m = manifest();
        m.scenarios.push(Scenario::new(0.25, 200));
        let mut results = Vec::new();
        for seed in 0..16 {
            results.push(result_at(8.0, 0, seed, m.buildings));
            let mut open = result_at(8.0, 0, seed + 100, m.buildings - 10);
            open.trial = Trial::new("t025-k200", TrialConfig::default(), 0, SA_BUDGETS[0], seed);
            results.push(open);
        }
        let policy = SaPolicy::default();
        assert!(policy.solved(&m, &results, "t050-k100"));
        assert!(!policy.solved(&m, &results, "t025-k200"));
        for draw in 0..128 {
            assert_eq!(
                policy.scenario_order(&m, &results, 7, draw),
                vec!["t025-k200".to_string()],
                "solved scenario still offered on draw {draw}"
            );
        }
    }

    #[test]
    fn an_unknown_building_count_never_reads_as_solved() {
        let mut m = manifest();
        m.buildings = 0;
        let results: Vec<_> = (0..16)
            .map(|seed| result_at(8.0, 0, seed, usize::MAX))
            .collect();
        assert!(!SaPolicy::default().solved(&m, &results, "t050-k100"));
    }

    #[test]
    fn every_scenario_solved_finishes_the_worker() {
        let m = manifest();
        let results: Vec<_> = (0..16)
            .map(|seed| result_at(8.0, 0, seed, m.buildings))
            .collect();
        assert!(SaPolicy::default().finished(&m, &results));
    }
}

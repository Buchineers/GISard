use std::collections::{HashMap, HashSet};

use super::manifest::Manifest;
use super::policy::{Policy, Proposal, lane, mix, scan_from};
use super::trial::{SeedStrategy, Trial, TrialConfig, TrialResult};

pub const RUNGS: [usize; 4] = [60, 180, 540, 1620];

pub const SEEDS_PER_RUNG: [usize; 4] = [1, 3, 5, 5];

const PROMOTE_TOP: f64 = 1.0 / 3.0;

const MIN_PROMOTION_FIELD: usize = 3;

const BASELINE_UNTIL: f64 = 0.01;
const EXPLORE_UNTIL: f64 = 0.15;
const POLISH_FROM: f64 = 0.92;
const FREEZE_FROM: f64 = 0.98;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Baseline,
    Explore,
    Exploit,
    Polish,
    Freeze,
}

pub fn phase_at(elapsed_fraction: f64) -> Phase {
    match elapsed_fraction {
        f if f < BASELINE_UNTIL => Phase::Baseline,
        f if f < EXPLORE_UNTIL => Phase::Explore,
        f if f < POLISH_FROM => Phase::Exploit,
        f if f < FREEZE_FROM => Phase::Polish,
        _ => Phase::Freeze,
    }
}

#[derive(Clone, Debug)]
pub struct Ladder {
    pub rungs: Vec<usize>,
    pub seeds: Vec<usize>,
}

impl Default for Ladder {
    fn default() -> Self {
        Ladder {
            rungs: RUNGS.to_vec(),
            seeds: SEEDS_PER_RUNG.to_vec(),
        }
    }
}

impl Ladder {
    pub fn new(rungs: Vec<usize>) -> Self {
        let last = *SEEDS_PER_RUNG.last().expect("non-empty");
        let seeds = SEEDS_PER_RUNG
            .iter()
            .copied()
            .chain(std::iter::repeat(last))
            .take(rungs.len())
            .collect();
        Ladder { rungs, seeds }
    }
}

pub struct AshaPolicy {
    pub prior: TrialConfig,
    pub ladder: Ladder,
    pub ladders: HashMap<String, Ladder>,
    pub max_trials: usize,
}

impl Default for AshaPolicy {
    fn default() -> Self {
        AshaPolicy {
            prior: TrialConfig::default(),
            ladder: Ladder::default(),
            ladders: HashMap::new(),
            max_trials: usize::MAX,
        }
    }
}

impl AshaPolicy {
    pub fn ladder_for(&self, scenario: &str) -> &Ladder {
        self.ladders.get(scenario).unwrap_or(&self.ladder)
    }

    pub fn with_rungs(mut self, rungs: Vec<usize>) -> Self {
        self.ladder = Ladder::new(rungs);
        self
    }

    pub fn with_probe(mut self, probe: &super::probe::Probe) -> Self {
        self.prior.spacing = probe.recommended_spacing;
        self.ladder = Ladder::new(probe.rungs.clone());
        self.ladders = probe
            .scenarios
            .iter()
            .map(|s| (s.id.clone(), Ladder::new(s.rungs.clone())))
            .collect();
        self
    }
}

struct Rung {
    scores: Vec<usize>,
}

impl AshaPolicy {
    fn ladder(results: &[TrialResult], scenario: &str) -> HashMap<(String, usize), Rung> {
        let mut out: HashMap<(String, usize), Rung> = HashMap::new();
        for r in results.iter().filter(|r| r.trial.scenario == scenario) {
            out.entry((config_key(&r.trial.config), r.trial.rung))
                .or_insert_with(|| Rung { scores: Vec::new() })
                .scores
                .push(r.serviced);
        }
        out
    }

    fn promotable(
        &self,
        results: &[TrialResult],
        scenario: &str,
        avoid: &HashSet<String>,
    ) -> Option<(TrialConfig, usize)> {
        let steps = self.ladder_for(scenario);
        let ladder = Self::ladder(results, scenario);
        let configs: HashMap<String, TrialConfig> = results
            .iter()
            .filter(|r| r.trial.scenario == scenario)
            .map(|r| (config_key(&r.trial.config), r.trial.config))
            .collect();

        for rung in (0..steps.rungs.len() - 1).rev() {
            let mut complete: Vec<(&String, usize)> = ladder
                .iter()
                .filter(|((_, r), entry)| *r == rung && entry.scores.len() >= steps.seeds[rung])
                .map(|((key, _), entry)| (key, median(&entry.scores)))
                .collect();
            if complete.is_empty() {
                continue;
            }
            if complete.len() < MIN_PROMOTION_FIELD && rung > 0 {
                continue;
            }
            complete.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
            let cutoff = ((complete.len() as f64 * PROMOTE_TOP).ceil() as usize).max(1);

            for (key, _) in complete.into_iter().take(cutoff) {
                let next = rung + 1;
                let done = ladder
                    .get(&(key.clone(), next))
                    .map_or(0, |e| e.scores.len());
                if done >= steps.seeds[next] {
                    continue;
                }
                let config = configs[key];
                for seed in 0..steps.seeds[next] as u64 {
                    let trial = Trial::new(scenario, config, next, steps.rungs[next], seed);
                    if !avoid.contains(&trial.id) && !results.iter().any(|r| r.trial.id == trial.id)
                    {
                        return Some((config, next));
                    }
                }
            }
        }
        None
    }

    fn incomplete_base(
        &self,
        results: &[TrialResult],
        scenario: &str,
        avoid: &HashSet<String>,
        lane: u64,
    ) -> Option<Trial> {
        let steps = self.ladder_for(scenario);
        let ladder = Self::ladder(results, scenario);
        let mut keys: Vec<&(String, usize)> = ladder
            .keys()
            .filter(|(_, rung)| *rung == 0)
            .filter(|k| ladder[*k].scores.len() < steps.seeds[0])
            .collect();
        keys.sort_unstable();
        for i in scan_from(lane, keys.len()) {
            let key = keys[i];
            let config = results
                .iter()
                .find(|r| r.trial.scenario == scenario && config_key(&r.trial.config) == key.0)?
                .trial
                .config;
            for seed in scan_from(lane, steps.seeds[0]) {
                let trial = Trial::new(scenario, config, 0, steps.rungs[0], seed as u64);
                if !avoid.contains(&trial.id) && !results.iter().any(|r| r.trial.id == trial.id) {
                    return Some(trial);
                }
            }
        }
        None
    }

    pub fn champion(&self, results: &[TrialResult], scenario: &str) -> Option<TrialConfig> {
        let ladder = Self::ladder(results, scenario);
        let configs: HashMap<String, TrialConfig> = results
            .iter()
            .filter(|r| r.trial.scenario == scenario)
            .map(|r| (config_key(&r.trial.config), r.trial.config))
            .collect();
        for rung in (0..self.ladder_for(scenario).rungs.len()).rev() {
            let best = ladder
                .iter()
                .filter(|((_, r), _)| *r == rung)
                .max_by_key(|((key, _), entry)| (median(&entry.scores), std::cmp::Reverse(key)));
            if let Some(((key, _), _)) = best {
                return configs.get(key).copied();
            }
        }
        None
    }
}

impl Policy for AshaPolicy {
    fn finished(&self, manifest: &Manifest, results: &[TrialResult]) -> bool {
        results.len() >= self.max_trials || phase_at(elapsed_fraction(manifest)) == Phase::Freeze
    }

    fn propose(
        &self,
        manifest: &Manifest,
        results: &[TrialResult],
        avoid: &HashSet<String>,
        worker: &str,
        cached_spacing: Option<f64>,
    ) -> Option<Proposal> {
        if results.len() >= self.max_trials {
            return None;
        }
        let phase = phase_at(elapsed_fraction(manifest));
        if phase == Phase::Freeze {
            return None;
        }
        let lane = lane(worker);

        let n = manifest.scenarios.len();
        for i in 0..n {
            let scenario = &manifest.scenarios[(lane as usize).wrapping_add(i) % n];
            if !results.iter().any(|r| r.trial.scenario == scenario.id) {
                let mut config = self.prior;
                if let Some(spacing) = cached_spacing {
                    config.spacing = spacing;
                }
                let steps = self.ladder_for(&scenario.id);
                let trial = Trial::new(&scenario.id, config, 0, steps.rungs[0], 0);
                if !avoid.contains(&trial.id) {
                    return Some(Proposal { trial, priority: 0 });
                }
            }
        }

        let draw = results.iter().filter(|r| r.worker == worker).count() as u64;
        let scenario = self.pick_scenario(manifest, results, lane, draw)?;
        let mut base = self.prior;
        if let Some(spacing) = cached_spacing {
            base.spacing = spacing;
        }

        let steps = self.ladder_for(&scenario);
        if let Some((config, rung)) = self.promotable(results, &scenario, avoid) {
            for seed in scan_from(lane, steps.seeds[rung]).map(|s| s as u64) {
                let trial = Trial::new(&scenario, config, rung, steps.rungs[rung], seed);
                if !avoid.contains(&trial.id) && !results.iter().any(|r| r.trial.id == trial.id) {
                    let below = rung - 1;
                    let predecessor = Trial::new(
                        &scenario,
                        config,
                        below,
                        steps.rungs[below],
                        seed % steps.seeds[below].max(1) as u64,
                    );
                    return Some(Proposal {
                        trial: trial.resuming(&predecessor),
                        priority: 10,
                    });
                }
            }
        }

        if let Some(trial) = self.incomplete_base(results, &scenario, avoid, lane) {
            return Some(Proposal {
                trial,
                priority: 20,
            });
        }

        match phase {
            Phase::Polish => {
                let config = self.champion(results, &scenario).unwrap_or(base);
                let mine = results
                    .iter()
                    .filter(|r| r.trial.scenario == scenario && r.worker == worker)
                    .count() as u64;
                (0..1024).find_map(|attempt| {
                    let seed = mix(lane, mine + attempt);
                    let rung = steps.rungs.len() - 1;
                    let trial = Trial::new(&scenario, config, rung, steps.rungs[rung], seed);
                    (!avoid.contains(&trial.id)).then_some(Proposal {
                        trial,
                        priority: 30,
                    })
                })
            }
            _ => {
                let mine = results
                    .iter()
                    .filter(|r| r.trial.scenario == scenario && r.worker == worker)
                    .count() as u64;
                let config = sample_config(base, mix(lane, mine));
                let trial = Trial::new(&scenario, config, 0, steps.rungs[0], 0);
                if avoid.contains(&trial.id) {
                    let config = sample_config(base, mix(lane, mine + 1));
                    let trial = Trial::new(&scenario, config, 0, steps.rungs[0], 0);
                    return Some(Proposal {
                        trial,
                        priority: 40,
                    });
                }
                Some(Proposal {
                    trial,
                    priority: 40,
                })
            }
        }
    }
}

impl AshaPolicy {
    fn pick_scenario(
        &self,
        manifest: &Manifest,
        results: &[TrialResult],
        lane: u64,
        draw: u64,
    ) -> Option<String> {
        let raw: Vec<(String, f64)> = manifest
            .scenarios
            .iter()
            .map(|s| {
                let mut scores: Vec<usize> = results
                    .iter()
                    .filter(|r| r.trial.scenario == s.id)
                    .map(|r| r.serviced)
                    .collect();
                scores.sort_unstable();
                let n = scores.len();
                if n < 4 {
                    return (s.id.clone(), f64::NAN);
                }
                let recent = &scores[n.saturating_sub(RECENT_TRIALS)..];
                let best = recent[recent.len() - 1] as f64;
                let median = recent[recent.len() / 2] as f64;
                (
                    s.id.clone(),
                    if best > 0.0 {
                        (best - median) / best
                    } else {
                        0.0
                    },
                )
            })
            .collect();

        let peak = raw
            .iter()
            .filter(|(_, h)| h.is_finite())
            .map(|(_, h)| *h)
            .fold(0.0f64, f64::max);
        let weights: Vec<(String, f64)> = raw
            .into_iter()
            .map(|(id, h)| {
                let scaled = if !h.is_finite() {
                    1.0
                } else if peak > 0.0 {
                    h / peak
                } else {
                    0.0
                };
                (id, STARVATION_FLOOR + scaled)
            })
            .collect();

        let total: f64 = weights.iter().map(|(_, w)| w).sum();
        if total <= 0.0 {
            return manifest.scenarios.first().map(|s| s.id.clone());
        }
        let mut pick = (mix(lane, draw) % 100_000) as f64 / 100_000.0 * total;
        for (id, w) in &weights {
            pick -= w;
            if pick <= 0.0 {
                return Some(id.clone());
            }
        }
        weights.last().map(|(id, _)| id.clone())
    }
}

const STARVATION_FLOOR: f64 = 0.25;

const RECENT_TRIALS: usize = 12;

fn median(scores: &[usize]) -> usize {
    if scores.is_empty() {
        return 0;
    }
    let mut sorted = scores.to_vec();
    sorted.sort_unstable();
    sorted[sorted.len() / 2]
}

fn config_key(config: &TrialConfig) -> String {
    serde_json::to_string(config).expect("a config is serializable")
}

fn sample_config(base: TrialConfig, seed: u64) -> TrialConfig {
    let draw = |shift: u32, span: u64| ((seed >> shift) % span) as usize;
    let restarts = 1 + draw(13, 2);
    TrialConfig {
        destroy: 2 + draw(0, 15),
        rcl: 6 + draw(4, 43),
        repair_rounds: 1 + draw(10, 5),
        restarts,
        exhaustive_rounds: draw(15, 4),
        pair_rounds: draw(18, 6),
        pair_removals: 4 + draw(21, 21),
        pair_candidates: 16 + draw(24, 113),
        recombine_rounds: if restarts > 1 { draw(31, 5) } else { 0 },
        coverability_alpha: 0.5 + (draw(34, 16) as f64) * 0.1,
        seed_strategy: match draw(38, 5) {
            0 => SeedStrategy::Greedy,
            1 => SeedStrategy::Coverability,
            2 => SeedStrategy::Threshold,
            3 => SeedStrategy::Direct,
            _ => SeedStrategy::Best,
        },
        ..base
    }
}

fn elapsed_fraction(manifest: &Manifest) -> f64 {
    if manifest.deadline_unix == 0 || manifest.deadline_unix <= manifest.start_unix {
        return 0.5;
    }
    let now = super::worker::now_unix();
    let total = (manifest.deadline_unix - manifest.start_unix) as f64;
    ((now.saturating_sub(manifest.start_unix)) as f64 / total).clamp(0.0, 1.0)
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
            buildings: 0,
        }
    }

    fn manifest_at(fraction: f64) -> Manifest {
        let span = 10_000u64;
        let now = super::super::worker::now_unix();
        Manifest {
            start_unix: now - (span as f64 * fraction) as u64,
            deadline_unix: now + (span as f64 * (1.0 - fraction)) as u64,
            ..manifest()
        }
    }

    fn config(destroy: usize) -> TrialConfig {
        TrialConfig {
            destroy,
            ..Default::default()
        }
    }

    fn result(
        scenario: &str,
        config: TrialConfig,
        rung: usize,
        seed: u64,
        serviced: usize,
    ) -> TrialResult {
        TrialResult {
            trial: Trial::new(scenario, config, rung, RUNGS[rung], seed),
            serviced,
            seconds: 1.0,
            worker: "w".into(),
            completed_unix_ms: 0,
        }
    }

    fn complete_rung0(scenario: &str, config: TrialConfig, scores: &[usize]) -> Vec<TrialResult> {
        scores
            .iter()
            .enumerate()
            .map(|(i, &s)| result(scenario, config, 0, i as u64, s))
            .collect()
    }

    #[test]
    fn phases_follow_the_clock() {
        assert_eq!(phase_at(0.0), Phase::Baseline);
        assert_eq!(phase_at(0.05), Phase::Explore);
        assert_eq!(phase_at(0.5), Phase::Exploit);
        assert_eq!(phase_at(0.95), Phase::Polish);
        assert_eq!(phase_at(0.99), Phase::Freeze);
    }

    #[test]
    fn nothing_is_proposed_after_the_freeze() {
        let policy = AshaPolicy::default();
        let mut m = manifest();
        m.start_unix = 0;
        m.deadline_unix = 1;
        let done = complete_rung0(&m.scenarios[0].id, config(8), &[100, 100, 100]);
        assert!(
            policy
                .propose(&m, &done, &HashSet::new(), "w", None)
                .is_none()
        );
    }

    #[test]
    fn promotion_waits_for_every_seed_in_the_rung() {
        let policy = AshaPolicy::default();
        let s = &manifest().scenarios[0].id.clone();
        assert!(SEEDS_PER_RUNG[1] > 1, "rung 1 must be multi-seed");

        let mut done = Vec::new();
        for (i, score) in [5000, 4900, 4800].iter().enumerate() {
            done.extend(complete_rung0(s, config(6 + i), &[*score]));
        }
        done.push(result(s, config(6), 1, 0, 9999));
        assert!(
            policy
                .promotable(&done, s, &HashSet::new())
                .is_none_or(|(_, r)| r != 2),
            "one lucky seed at rung 1 must not buy rung 2"
        );
    }

    #[test]
    fn the_screening_rung_needs_only_one_seed() {
        assert_eq!(SEEDS_PER_RUNG[0], 1);
        let policy = AshaPolicy::default();
        let s = &manifest().scenarios[0].id.clone();
        let done = complete_rung0(s, config(8), &[5000]);
        assert!(
            policy.promotable(&done, s, &HashSet::new()).is_some(),
            "one seed is enough to screen"
        );
    }

    #[test]
    fn the_ladder_starts_past_break_even() {
        let (fixed, marginal) = (22.0, 1.03);
        let search = RUNGS[0] as f64 * marginal;
        assert!(
            search > fixed,
            "rung 0 spends {fixed}s on setup for {search}s of search"
        );
    }

    #[test]
    fn promotion_moves_up_exactly_one_rung() {
        let policy = AshaPolicy::default();
        let s = &manifest().scenarios[0].id.clone();
        let full = complete_rung0(s, config(8), &[5000, 4900, 4950]);
        let (_, rung) = policy.promotable(&full, s, &HashSet::new()).unwrap();
        assert_eq!(rung, 1);
    }

    #[test]
    fn only_the_top_third_is_promoted() {
        let policy = AshaPolicy::default();
        let s = &manifest().scenarios[0].id.clone();
        let mut done = Vec::new();
        for (i, score) in [5000, 4000, 3000, 2000, 1000].iter().enumerate() {
            done.extend(complete_rung0(s, config(2 + i), &[*score, *score, *score]));
        }
        let (promoted, _) = policy.promotable(&done, s, &HashSet::new()).unwrap();
        assert_eq!(promoted.destroy, 2, "the best config, not any other");

        let promoted_keys: Vec<usize> = (0..5)
            .filter(|i| {
                let c = config(2 + i);
                let mut with_next = done.clone();
                with_next.retain(|r| r.trial.config.destroy >= c.destroy);
                policy
                    .promotable(&with_next, s, &HashSet::new())
                    .is_some_and(|(p, _)| p.destroy == c.destroy)
            })
            .collect();
        assert!(promoted_keys.contains(&0));
    }

    #[test]
    fn promotion_ranks_by_median_not_best() {
        let policy = AshaPolicy::default();
        let s = &manifest().scenarios[0].id.clone();
        let mut done = Vec::new();
        done.extend(complete_rung0(s, config(2), &[9000, 1000, 1000]));
        done.extend(complete_rung0(s, config(3), &[5000, 5000, 5000]));

        let (promoted, _) = policy.promotable(&done, s, &HashSet::new()).unwrap();
        assert_eq!(
            promoted.destroy, 3,
            "the steady config must win over the lucky one"
        );
    }

    #[test]
    fn a_full_rung_promotes_onward_not_again() {
        let policy = AshaPolicy::default();
        let s = &manifest().scenarios[0].id.clone();

        let mut done = Vec::new();
        for (i, score) in [5100, 5000, 4900].iter().enumerate() {
            let c = config(6 + i);
            done.extend(complete_rung0(s, c, &[*score, *score, *score]));
            for seed in 0..SEEDS_PER_RUNG[1] as u64 {
                done.push(result(s, c, 1, seed, *score));
            }
        }
        let (promoted, rung) = policy.promotable(&done, s, &HashSet::new()).unwrap();
        assert_eq!(rung, 2, "rung 1 is full, so the budget goes up again");
        assert_eq!(promoted.destroy, 6, "and the best of the field goes up");
    }

    #[test]
    fn a_rung_with_no_field_promotes_nobody() {
        let policy = AshaPolicy::default();
        let s = &manifest().scenarios[0].id.clone();

        let mut done = complete_rung0(s, config(8), &[5000, 4900, 4950]);
        for seed in 0..SEEDS_PER_RUNG[1] as u64 {
            done.push(result(s, config(8), 1, seed, 5100));
        }
        assert!(
            policy
                .promotable(&done, s, &HashSet::new())
                .is_none_or(|(_, r)| r != 2),
            "a field of one is not a competition"
        );
    }

    #[test]
    fn rung_zero_promotes_without_a_field() {
        let policy = AshaPolicy::default();
        let s = &manifest().scenarios[0].id.clone();
        let done = complete_rung0(s, config(8), &[5000, 4900, 4950]);
        let (_, rung) = policy.promotable(&done, s, &HashSet::new()).unwrap();
        assert_eq!(rung, 1);
    }

    #[test]
    fn the_top_rung_is_the_end_of_the_ladder() {
        let policy = AshaPolicy::default();
        let s = &manifest().scenarios[0].id.clone();
        let mut done = complete_rung0(s, config(8), &[5000, 4900, 4950]);
        for (rung, &seeds) in SEEDS_PER_RUNG.iter().enumerate().skip(1) {
            for seed in 0..seeds as u64 {
                done.push(result(s, config(8), rung, seed, 5100));
            }
        }
        assert!(
            policy.promotable(&done, s, &HashSet::new()).is_none(),
            "nothing above the top rung to promote into"
        );
    }

    #[test]
    fn a_promotion_resumes_its_own_predecessor() {
        let policy = AshaPolicy::default();
        let m = manifest();
        let s = &m.scenarios[0].id.clone();
        let done = complete_rung0(s, config(8), &[5000]);

        let p = policy
            .propose(&m, &done, &HashSet::new(), "w", None)
            .unwrap();
        assert_eq!(p.trial.rung, 1);
        let from = p.trial.resume_from.clone().expect("a promotion resumes");

        let steps = policy.ladder_for(s);
        let expected = Trial::new(s, config(8), 0, steps.rungs[0], 0);
        assert_eq!(from, expected.id, "must continue this config's own rung 0");
    }

    #[test]
    fn a_fresh_config_does_not_resume() {
        let policy = AshaPolicy::default();
        let m = manifest();
        let p = policy.propose(&m, &[], &HashSet::new(), "w", None).unwrap();
        assert_eq!(p.trial.rung, 0);
        assert!(p.trial.resume_from.is_none());
    }

    #[test]
    fn resuming_does_not_change_a_trials_identity() {
        let a = Trial::new("s", config(8), 1, 60, 0);
        let b =
            Trial::new("s", config(8), 1, 60, 0).resuming(&Trial::new("s", config(8), 0, 20, 0));
        assert_eq!(a.id, b.id);
    }

    #[test]
    fn promotions_are_the_same_config_and_rung_for_every_worker() {
        let policy = AshaPolicy::default();
        let m = manifest();
        let s = &m.scenarios[0].id.clone();
        let done = complete_rung0(s, config(8), &[5000, 4900, 4950]);
        let seeds = policy.ladder_for(s).seeds[1] as u64;

        for worker in ["worker-a", "worker-b", "worker-c", "worker-d"] {
            let p = policy
                .propose(&m, &done, &HashSet::new(), worker, None)
                .unwrap();
            assert_eq!(p.trial.rung, 1);
            assert_eq!(p.trial.config, config(8), "same config promoted");
            assert!(
                p.trial.seed < seeds,
                "seeds stay inside the rung's fixed set"
            );
        }
    }

    #[test]
    fn workers_do_not_all_reach_for_the_same_promotion_seed() {
        let policy = AshaPolicy::default();
        let m = manifest();
        let s = &m.scenarios[0].id.clone();
        let done = complete_rung0(s, config(8), &[5000, 4900, 4950]);

        let seeds: HashSet<u64> = (0..24)
            .map(|i| {
                policy
                    .propose(&m, &done, &HashSet::new(), &format!("n{i}"), None)
                    .unwrap()
                    .trial
                    .seed
            })
            .collect();
        assert!(
            seeds.len() > 1,
            "every worker reached for the same seed; the claim would serialise them"
        );
    }

    #[test]
    fn the_champion_is_the_best_config_at_the_highest_rung() {
        let policy = AshaPolicy::default();
        let s = &manifest().scenarios[0].id.clone();
        let mut done = complete_rung0(s, config(2), &[9000, 9000, 9000]);
        for seed in 0..SEEDS_PER_RUNG[1] as u64 {
            done.push(result(s, config(7), 1, seed, 100));
        }
        assert_eq!(policy.champion(&done, s).unwrap().destroy, 7);
    }

    #[test]
    fn sampled_configs_stay_inside_the_searchable_space() {
        for seed in 0..500u64 {
            let c = sample_config(
                TrialConfig::default(),
                seed.wrapping_mul(0x9E3779B97F4A7C15),
            );
            assert!((2..=16).contains(&c.destroy), "destroy {}", c.destroy);
            assert!((6..=48).contains(&c.rcl), "rcl {}", c.rcl);
            assert!((1..=5).contains(&c.repair_rounds));
            assert!((1..=4).contains(&c.restarts));
            assert!((0..=5).contains(&c.exhaustive_rounds));
            assert!((0.5..=2.0).contains(&c.coverability_alpha));
        }
    }

    #[test]
    fn sampling_can_never_reach_an_invalid_configuration() {
        for seed in 0..500u64 {
            let c = sample_config(
                TrialConfig::default(),
                seed.wrapping_mul(0x9E3779B97F4A7C15),
            );
            let options = c.options(1, 20);
            assert_eq!(options.free_polish_steps, 0);
            assert_eq!(options.local_spacing, 0.0);
            assert_eq!(options.event_radius, 0.0);
            assert!(!options.threshold_breakpoints);
        }
    }

    #[test]
    fn spacing_is_never_sampled_away_from_the_cached_matrix() {
        let base = TrialConfig {
            spacing: 4.0,
            ..Default::default()
        };
        for seed in 0..200u64 {
            assert_eq!(sample_config(base, seed).spacing, 4.0);
        }
    }

    #[test]
    fn a_probed_ladder_keeps_seeds_aligned() {
        let policy = AshaPolicy::default().with_rungs(vec![20, 60]);
        assert_eq!(policy.ladder.rungs.len(), policy.ladder.seeds.len());

        let s = &manifest().scenarios[0].id.clone();
        let full = complete_rung0(s, config(8), &[5000, 4900, 4950]);
        let (_, rung) = policy.promotable(&full, s, &HashSet::new()).unwrap();
        assert_eq!(rung, 1);

        let mut done = full;
        for seed in 0..policy.ladder.seeds[1] as u64 {
            done.push(result(s, config(8), 1, seed, 5100));
        }
        assert!(
            policy.promotable(&done, s, &HashSet::new()).is_none(),
            "rung 1 is the top of this ladder"
        );
    }

    #[test]
    fn a_longer_ladder_extends_the_seed_table() {
        let policy = AshaPolicy::default().with_rungs(vec![20, 60, 180, 540, 1620, 4860]);
        assert_eq!(policy.ladder.seeds.len(), 6);
        assert_eq!(
            *policy.ladder.seeds.last().unwrap(),
            *SEEDS_PER_RUNG.last().unwrap()
        );
    }

    #[test]
    fn per_scenario_ladders_are_independent() {
        let mut policy = AshaPolicy::default();
        policy
            .ladders
            .insert("cheap".into(), Ladder::new(vec![20, 60, 180, 540, 1620]));
        policy
            .ladders
            .insert("dear".into(), Ladder::new(vec![20, 60]));

        assert_eq!(policy.ladder_for("cheap").rungs.len(), 5);
        assert_eq!(policy.ladder_for("dear").rungs.len(), 2);
        assert_eq!(
            policy.ladder_for("unprobed").rungs,
            RUNGS.to_vec(),
            "an unprobed scenario falls back to the default"
        );

        let mut done = complete_rung0("dear", config(8), &[10, 10, 10]);
        for seed in 0..policy.ladder_for("dear").seeds[1] as u64 {
            done.push(TrialResult {
                trial: Trial::new("dear", config(8), 1, 60, seed),
                serviced: 10,
                seconds: 1.0,
                worker: "w".into(),
                completed_unix_ms: 0,
            });
        }
        assert!(policy.promotable(&done, "dear", &HashSet::new()).is_none());

        let cheap = complete_rung0("cheap", config(8), &[10, 10, 10]);
        assert!(
            policy
                .promotable(&cheap, "cheap", &HashSet::new())
                .is_some()
        );
    }

    #[test]
    fn the_baseline_phase_still_has_work_once_baselines_exist() {
        let policy = AshaPolicy::default();
        let m = manifest_at(0.0);
        assert_eq!(phase_at(0.0), Phase::Baseline);

        let done = complete_rung0(&m.scenarios[0].id, config(8), &[5000, 4900, 4950]);
        assert!(
            policy
                .propose(&m, &done, &HashSet::new(), "w", None)
                .is_some(),
            "idling 210 nodes until the 1% mark throws away the campaign's first minutes"
        );
        assert!(
            !policy.finished(&m, &done),
            "and the worker must not retire"
        );
    }

    #[test]
    fn no_working_phase_leaves_a_worker_idle() {
        let policy = AshaPolicy::default();
        for fraction in [0.0, 0.05, 0.14, 0.16, 0.5, 0.91, 0.93, 0.97] {
            let m = manifest_at(fraction);
            let phase = phase_at(fraction);
            let done = complete_rung0(&m.scenarios[0].id, config(8), &[5000, 4900, 4950]);
            assert!(
                policy
                    .propose(&m, &done, &HashSet::new(), "w", None)
                    .is_some(),
                "{phase:?} at {fraction} proposed nothing"
            );
            assert!(!policy.finished(&m, &done), "{phase:?} retired the worker");
        }
    }

    #[test]
    fn the_freeze_stops_everything() {
        let policy = AshaPolicy::default();
        let m = manifest_at(0.99);
        let done = complete_rung0(&m.scenarios[0].id, config(8), &[5000, 4900, 4950]);
        assert_eq!(phase_at(0.99), Phase::Freeze);
        assert!(
            policy
                .propose(&m, &done, &HashSet::new(), "w", None)
                .is_none()
        );
        assert!(
            policy.finished(&m, &done),
            "at the freeze the worker must actually stop, not idle-poll"
        );
    }

    #[test]
    fn polish_still_finishes_a_started_ladder() {
        let policy = AshaPolicy::default();
        let m = manifest_at(0.95);
        assert_eq!(phase_at(0.95), Phase::Polish);

        let done = complete_rung0(&m.scenarios[0].id, config(8), &[5000, 4900, 4950]);
        let p = policy
            .propose(&m, &done, &HashSet::new(), "w", None)
            .unwrap();
        assert_eq!(p.trial.rung, 1, "the promotion comes first");
    }

    #[test]
    fn the_polish_phase_re_seeds_the_champion() {
        let policy = AshaPolicy::default();
        let m = manifest_at(0.95);
        let scenario = &m.scenarios[0].id.clone();

        let mut done = complete_rung0(scenario, config(7), &[9000, 9000, 9000]);
        for (rung, &seeds) in SEEDS_PER_RUNG.iter().enumerate().skip(1) {
            for seed in 0..seeds as u64 {
                done.push(result(scenario, config(7), rung, seed, 9500));
            }
        }
        assert!(
            policy
                .promotable(&done, scenario, &HashSet::new())
                .is_none()
        );

        let champion = policy.champion(&done, scenario).unwrap();
        let p = policy
            .propose(&m, &done, &HashSet::new(), "w", None)
            .unwrap();
        assert_eq!(p.trial.config.destroy, champion.destroy, "the champion");
        assert_eq!(
            p.trial.rung,
            policy.ladder.rungs.len() - 1,
            "re-seeded at full budget"
        );
    }

    #[test]
    fn one_worker_spreads_its_trials_over_the_scenarios() {
        let policy = AshaPolicy::default();
        let mut m = manifest();
        m.scenarios = vec![
            Scenario::new(0.25, 20),
            Scenario::new(0.5, 40),
            Scenario::new(0.75, 60),
        ];

        let mut counts: HashMap<String, usize> = HashMap::new();
        for draw in 0..90u64 {
            let picked = policy
                .pick_scenario(&m, &[], lane("w0"), draw)
                .expect("a scenario");
            *counts.entry(picked).or_default() += 1;
        }
        assert_eq!(counts.len(), 3, "every scenario must get work: {counts:?}");
        for (id, n) in &counts {
            assert!(*n >= 10, "{id} starved with {n}/90 draws: {counts:?}");
        }
    }

    #[test]
    fn the_floor_no_longer_swamps_the_headroom_signal() {
        let policy = AshaPolicy::default();
        let mut m = manifest();
        m.scenarios = vec![Scenario::new(0.25, 1000), Scenario::new(0.75, 50)];
        let (saturated, roomy) = (m.scenarios[0].id.clone(), m.scenarios[1].id.clone());

        let mut done = Vec::new();
        for i in 0..12u64 {
            done.push(result(
                &saturated,
                config(8),
                0,
                i,
                12790 + (i % 2) as usize * 6,
            ));
            done.push(result(&roomy, config(8), 0, i, 336 + i as usize * 6));
        }

        let mut counts: HashMap<String, usize> = HashMap::new();
        for draw in 0..400u64 {
            let picked = policy
                .pick_scenario(&m, &done, lane("w0"), draw)
                .expect("a scenario");
            *counts.entry(picked).or_default() += 1;
        }
        let ratio = counts[&roomy] as f64 / counts[&saturated] as f64;
        assert!(
            ratio > 2.0,
            "headroom must steer the budget, got {ratio:.2}x: {counts:?}"
        );
        assert!(
            counts[&saturated] >= 40,
            "but a saturated scenario keeps its floor: {counts:?}"
        );
    }

    #[test]
    fn allocation_follows_headroom() {
        let policy = AshaPolicy::default();
        let mut m = manifest();
        m.scenarios = vec![Scenario::new(0.25, 20), Scenario::new(0.5, 40)];
        let (flat, roomy) = (m.scenarios[0].id.clone(), m.scenarios[1].id.clone());

        let mut done = Vec::new();
        for i in 0..8 {
            done.push(result(&flat, config(8), 0, i, 100));
            done.push(result(&roomy, config(8), 0, i, 100 + i as usize * 40));
        }

        let mut counts: HashMap<String, usize> = HashMap::new();
        for draw in 0..200u64 {
            let picked = policy
                .pick_scenario(&m, &done, lane("w0"), draw)
                .expect("a scenario");
            *counts.entry(picked).or_default() += 1;
        }
        assert!(
            counts[&roomy] > counts[&flat],
            "headroom must attract budget: {counts:?}"
        );
    }

    #[test]
    fn an_unscored_scenario_outranks_every_search_decision() {
        let policy = AshaPolicy::default();
        let mut m = manifest();
        m.scenarios.push(Scenario::new(0.75, 200));
        let done = complete_rung0(&m.scenarios[0].id, config(8), &[5000, 4900, 4950]);

        let p = policy
            .propose(&m, &done, &HashSet::new(), "w", None)
            .unwrap();
        assert_eq!(p.trial.scenario, m.scenarios[1].id);
        assert_eq!(p.priority, 0, "submittability comes before tuning");
    }
}

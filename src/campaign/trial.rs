use serde::{Deserialize, Serialize};

use crate::coverage::CoverageConfig;
use crate::solvers::{LnsOptions, LnsSeedStrategy};

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct TrialConfig {
    pub spacing: f64,
    #[serde(default = "default_sa_pair_probability")]
    pub sa_pair_probability: f64,
    #[serde(default = "default_sa_target_probability")]
    pub sa_target_probability: f64,
    pub iterations: usize,
    pub destroy: usize,
    pub rcl: usize,
    pub repair_rounds: usize,
    pub restarts: usize,
    pub exhaustive_rounds: usize,
    pub pair_rounds: usize,
    pub pair_removals: usize,
    pub pair_candidates: usize,
    pub recombine_rounds: usize,
    pub coverability_alpha: f64,
    pub seed_strategy: SeedStrategy,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SeedStrategy {
    Greedy,
    Coverability,
    Threshold,
    Direct,
    #[default]
    Best,
}

impl From<SeedStrategy> for LnsSeedStrategy {
    fn from(s: SeedStrategy) -> Self {
        match s {
            SeedStrategy::Greedy => LnsSeedStrategy::Greedy,
            SeedStrategy::Coverability => LnsSeedStrategy::Coverability,
            SeedStrategy::Threshold => LnsSeedStrategy::Threshold,
            SeedStrategy::Direct => LnsSeedStrategy::Direct,
            SeedStrategy::Best => LnsSeedStrategy::BestOfAll,
        }
    }
}

impl Default for TrialConfig {
    fn default() -> Self {
        TrialConfig {
            spacing: 8.0,
            sa_pair_probability: default_sa_pair_probability(),
            sa_target_probability: default_sa_target_probability(),
            iterations: 60,
            destroy: 8,
            rcl: 16,
            repair_rounds: 3,
            restarts: 1,
            exhaustive_rounds: 2,
            pair_rounds: 3,
            pair_removals: 12,
            pair_candidates: 64,
            recombine_rounds: 3,
            coverability_alpha: 1.0,
            seed_strategy: SeedStrategy::Best,
        }
    }
}

const fn default_sa_pair_probability() -> f64 {
    0.1
}

const fn default_sa_target_probability() -> f64 {
    0.85
}

impl TrialConfig {
    pub fn coverage(&self) -> CoverageConfig {
        CoverageConfig {
            candidate_spacing: self.spacing,
            event_candidate_radius: 0.0,
            event_candidate_limit: 0,
        }
    }

    pub fn options(&self, seed: u64, iterations: usize) -> LnsOptions {
        LnsOptions {
            lns_iterations: iterations,
            lns_destroy: self.destroy,
            lns_rcl: self.rcl,
            lns_repair_rounds: self.repair_rounds,
            lns_restarts: self.restarts,
            exhaustive_repair_rounds: self.exhaustive_rounds,
            pair_repair_rounds: self.pair_rounds,
            pair_removals: self.pair_removals,
            pair_candidates: self.pair_candidates,
            recombine_rounds: self.recombine_rounds,
            local_spacing: 0.0,
            threshold_breakpoints: false,
            free_polish_steps: 0,
            event_radius: 0.0,
            event_partners: 0,
            seed_strategy: self.seed_strategy.into(),
            coverability_alpha: self.coverability_alpha,
            seed: Some(seed),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Trial {
    pub id: String,
    pub scenario: String,
    pub config: TrialConfig,
    pub rung: usize,
    pub iterations: usize,
    pub seed: u64,
    #[serde(default)]
    pub resume_from: Option<String>,
    #[serde(default)]
    pub resume_after: usize,
}

impl Trial {
    pub fn new(
        scenario: &str,
        config: TrialConfig,
        rung: usize,
        iterations: usize,
        seed: u64,
    ) -> Self {
        let key = serde_json::to_string(&(scenario, &config, rung, iterations, seed))
            .expect("trial key is serializable");
        let id = blake3::hash(key.as_bytes()).to_hex()[..16].to_string();
        Trial {
            id,
            scenario: scenario.to_string(),
            config,
            rung,
            iterations,
            seed,
            resume_from: None,
            resume_after: 0,
        }
    }

    pub fn resuming(mut self, predecessor: &Trial) -> Self {
        self.resume_from = Some(predecessor.id.clone());
        self.resume_after = predecessor.iterations;
        self
    }

    pub fn seeded_from(mut self, predecessor: &Trial) -> Self {
        self.resume_from = Some(predecessor.id.clone());
        self
    }

    pub fn remaining_iterations(&self) -> usize {
        self.iterations.saturating_sub(self.resume_after).max(1)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TrialResult {
    pub trial: Trial,
    pub serviced: usize,
    pub seconds: f64,
    pub worker: String,
    #[serde(default)]
    pub completed_unix_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_trials_derive_identical_ids() {
        let a = Trial::new("t050-k100", TrialConfig::default(), 0, 20, 7);
        let b = Trial::new("t050-k100", TrialConfig::default(), 0, 20, 7);
        assert_eq!(a.id, b.id);
        assert_eq!(a.id.len(), 16);
    }

    #[test]
    fn every_field_changes_the_id() {
        let base = Trial::new("t050-k100", TrialConfig::default(), 0, 20, 7);
        let mut config = TrialConfig::default();
        config.destroy += 1;
        for other in [
            Trial::new("t050-k500", TrialConfig::default(), 0, 20, 7),
            Trial::new("t050-k100", config, 0, 20, 7),
            Trial::new("t050-k100", TrialConfig::default(), 1, 20, 7),
            Trial::new("t050-k100", TrialConfig::default(), 0, 60, 7),
            Trial::new("t050-k100", TrialConfig::default(), 0, 20, 8),
        ] {
            assert_ne!(base.id, other.id);
        }
    }

    #[test]
    fn sa_probabilities_are_part_of_the_trial_identity() {
        let base = TrialConfig::default();
        let mut no_pairs = base;
        no_pairs.sa_pair_probability = 0.0;
        let mut global = base;
        global.sa_target_probability = 0.0;
        let id = |config| Trial::new("t050-k100", config, 0, 20, 7).id;
        assert_ne!(id(base), id(no_pairs));
        assert_ne!(id(base), id(global));
    }

    #[test]
    fn old_trial_configs_keep_the_historical_sa_defaults() {
        let json = r#"{
            "spacing":8.0,"iterations":60,"destroy":8,"rcl":16,
            "repair_rounds":3,"restarts":1,"exhaustive_rounds":2,
            "pair_rounds":3,"pair_removals":12,"pair_candidates":64,
            "recombine_rounds":3,"coverability_alpha":1.0,
            "seed_strategy":"best"
        }"#;
        let config: TrialConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.sa_pair_probability, 0.1);
        assert_eq!(config.sa_target_probability, 0.85);
    }

    #[test]
    fn a_promotion_runs_only_the_increment() {
        let below = Trial::new("s", TrialConfig::default(), 0, 60, 0);
        let above = Trial::new("s", TrialConfig::default(), 1, 180, 0).resuming(&below);
        assert_eq!(above.resume_after, 60);
        assert_eq!(above.remaining_iterations(), 120);
    }

    #[test]
    fn a_cold_trial_runs_its_whole_budget() {
        let t = Trial::new("s", TrialConfig::default(), 0, 60, 0);
        assert_eq!(t.remaining_iterations(), 60);
    }

    #[test]
    fn invalid_knobs_are_pinned_off() {
        let options = TrialConfig::default().options(1, 20);
        assert_eq!(
            options.free_polish_steps, 0,
            "would move antennas off walls"
        );
        assert_eq!(options.local_spacing, 0.0);
        assert_eq!(
            options.event_radius, 0.0,
            "would invalidate shared coverage"
        );
        assert!(
            !options.threshold_breakpoints,
            "would invalidate shared coverage"
        );
    }
}

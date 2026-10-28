use std::collections::HashSet;

use super::manifest::Manifest;
use super::trial::{Trial, TrialConfig, TrialResult};

pub(super) fn mix(lane: u64, n: u64) -> u64 {
    lane ^ n.wrapping_mul(0x9E37_79B9_7F4A_7C15)
}

pub struct Proposal {
    pub trial: Trial,
    pub priority: u32,
}

pub trait Policy: Sync {
    fn propose(
        &self,
        manifest: &Manifest,
        results: &[TrialResult],
        avoid: &HashSet<String>,
        worker: &str,
        cached_spacing: Option<f64>,
    ) -> Option<Proposal>;

    fn finished(&self, _manifest: &Manifest, results: &[TrialResult]) -> bool {
        let _ = results;
        false
    }
}

pub(super) fn lane(worker: &str) -> u64 {
    u64::from_le_bytes(
        blake3::hash(worker.as_bytes()).as_bytes()[..8]
            .try_into()
            .expect("8 bytes"),
    )
}

pub(super) fn scan_from(lane: u64, n: usize) -> impl Iterator<Item = usize> {
    let start = if n == 0 {
        0
    } else {
        (lane % n as u64) as usize
    };
    (0..n).map(move |i| (start + i) % n)
}

pub struct BaselinePolicy {
    pub config: TrialConfig,
    pub max_trials: usize,
}

impl Default for BaselinePolicy {
    fn default() -> Self {
        BaselinePolicy {
            config: TrialConfig::default(),
            max_trials: usize::MAX,
        }
    }
}

impl Policy for BaselinePolicy {
    fn finished(&self, _manifest: &Manifest, results: &[TrialResult]) -> bool {
        results.len() >= self.max_trials
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
        let lane = lane(worker);
        let mut config = self.config;
        if let Some(spacing) = cached_spacing {
            config.spacing = spacing;
        }

        let n = manifest.scenarios.len();
        for i in 0..n {
            let scenario = &manifest.scenarios[(lane as usize).wrapping_add(i) % n];
            if !results.iter().any(|r| r.trial.scenario == scenario.id) {
                let trial = Trial::new(&scenario.id, config, 0, config.iterations, 0);
                if !avoid.contains(&trial.id) {
                    return Some(Proposal { trial, priority: 0 });
                }
            }
        }

        let scenario = manifest
            .scenarios
            .iter()
            .min_by_key(|s| results.iter().filter(|r| r.trial.scenario == s.id).count())?;
        let mine = results
            .iter()
            .filter(|r| r.trial.scenario == scenario.id && r.worker == worker)
            .count() as u64;
        (0..1024).find_map(|attempt| {
            let seed = mix(lane, mine + attempt);
            let trial = Trial::new(&scenario.id, config, 0, config.iterations, seed);
            (!avoid.contains(&trial.id)).then_some(Proposal {
                trial,
                priority: 100,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::manifest::Scenario;
    use super::*;

    fn manifest() -> Manifest {
        Manifest {
            dataset: "d.geojson".into(),
            scenarios: vec![Scenario::new(0.25, 50), Scenario::new(0.5, 100)],
            start_unix: 0,
            deadline_unix: 0,
            reserve_seconds: 0,
            duration_seconds: 0,
            spacings: vec![8.0],
            buildings: 0,
        }
    }

    fn result(scenario: &str, seed: u64) -> TrialResult {
        TrialResult {
            trial: Trial::new(scenario, TrialConfig::default(), 0, 20, seed),
            serviced: 100,
            seconds: 1.0,
            worker: "w".into(),
            completed_unix_ms: 0,
        }
    }

    #[test]
    fn covers_every_scenario_before_repeating_any() {
        let policy = BaselinePolicy::default();
        let m = manifest();

        let first = policy.propose(&m, &[], &HashSet::new(), "w", None).unwrap();
        assert_eq!(first.trial.scenario, m.scenarios[0].id);
        assert_eq!(first.priority, 0, "an unscored scenario is top priority");

        let done = vec![result(&m.scenarios[0].id, 0)];
        let second = policy
            .propose(&m, &done, &HashSet::new(), "w", None)
            .unwrap();
        assert_eq!(second.trial.scenario, m.scenarios[1].id);
        assert_eq!(second.priority, 0);
    }

    #[test]
    fn one_worker_is_deterministic() {
        let policy = BaselinePolicy::default();
        let m = manifest();
        let done = vec![result(&m.scenarios[0].id, 0)];
        let a = policy
            .propose(&m, &done, &HashSet::new(), "w", None)
            .unwrap();
        let b = policy
            .propose(&m, &done, &HashSet::new(), "w", None)
            .unwrap();
        assert_eq!(a.trial.id, b.trial.id);
    }

    #[test]
    fn different_workers_spread_over_the_search_space() {
        let policy = BaselinePolicy::default();
        let m = manifest();
        let done = vec![result(&m.scenarios[0].id, 0), result(&m.scenarios[1].id, 0)];
        let proposals: HashSet<String> = (0..64)
            .map(|i| {
                policy
                    .propose(&m, &done, &HashSet::new(), &format!("w{i}"), None)
                    .unwrap()
                    .trial
                    .id
            })
            .collect();
        assert_eq!(
            proposals.len(),
            64,
            "worker-private seeds must not collide at all"
        );
    }

    #[test]
    fn workers_fan_out_over_unscored_scenarios_first() {
        let policy = BaselinePolicy::default();
        let m = manifest();
        let scenarios: HashSet<String> = (0..32)
            .map(|i| {
                policy
                    .propose(&m, &[], &HashSet::new(), &format!("w{i}"), None)
                    .unwrap()
                    .trial
                    .scenario
            })
            .collect();
        assert_eq!(
            scenarios.len(),
            m.scenarios.len(),
            "every scenario is picked up"
        );
    }

    #[test]
    fn exploration_starts_once_every_scenario_is_covered() {
        let policy = BaselinePolicy::default();
        let m = manifest();
        let done = vec![result(&m.scenarios[0].id, 0), result(&m.scenarios[1].id, 0)];
        let next = policy
            .propose(&m, &done, &HashSet::new(), "w", None)
            .unwrap();
        assert_eq!(next.priority, 100, "no longer the submittability rush");
        assert_ne!(next.trial.seed, 0, "a fresh seed, not the baseline one");
    }

    #[test]
    fn the_cached_spacing_wins_over_the_configured_one() {
        let policy = BaselinePolicy::default();
        let next = policy
            .propose(&manifest(), &[], &HashSet::new(), "w", Some(2.0))
            .unwrap();
        assert_eq!(next.trial.config.spacing, 2.0);
    }

    #[test]
    fn a_taken_trial_is_stepped_over() {
        let policy = BaselinePolicy::default();
        let m = manifest();
        let done = vec![result(&m.scenarios[0].id, 0), result(&m.scenarios[1].id, 0)];
        let next = policy
            .propose(&m, &done, &HashSet::new(), "w", None)
            .unwrap();
        let avoid: HashSet<String> = [next.trial.id.clone()].into();
        let after = policy.propose(&m, &done, &avoid, "w", None).unwrap();
        assert_ne!(
            after.trial.id, next.trial.id,
            "a known-taken trial must not be proposed again"
        );
    }

    #[test]
    fn starvation_is_avoided_across_scenarios() {
        let policy = BaselinePolicy::default();
        let m = manifest();
        let mut done = vec![result(&m.scenarios[0].id, 0), result(&m.scenarios[1].id, 0)];
        done.push(result(&m.scenarios[0].id, 1));
        let next = policy
            .propose(&m, &done, &HashSet::new(), "w", None)
            .unwrap();
        assert_eq!(next.trial.scenario, m.scenarios[1].id, "least-tried first");
    }
}

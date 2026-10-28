use std::fs;
use std::path::Path;
use std::time::Instant;

use framework::Instance;
use serde::{Deserialize, Serialize};

use crate::coverage::{CoverageConfig, build_coverage};
use crate::sampling::candidates_for_instance;

use super::manifest::Manifest;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Probe {
    pub buildings: usize,
    pub vertices: usize,
    pub spacings: Vec<SpacingCost>,
    pub recommended_spacing: f64,
    pub memory_limit_mb: u64,
    pub scenarios: Vec<ScenarioCost>,
    pub rungs: Vec<usize>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SpacingCost {
    pub spacing: f64,
    pub candidates: usize,
    pub build_seconds: f64,
    pub memory_mb: f64,
    pub fits: bool,
    pub measured: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScenarioCost {
    pub id: String,
    pub k: usize,
    pub tau: f64,
    pub fixed_seconds: f64,
    pub iteration_seconds: f64,
    pub serviced: usize,
    pub rungs: Vec<usize>,
}

pub struct ProbeOptions {
    pub spacings: Vec<f64>,
    pub memory_limit_mb: u64,
    pub iterations: usize,
    pub max_rung_fraction: f64,
}

impl Default for ProbeOptions {
    fn default() -> Self {
        ProbeOptions {
            spacings: vec![8.0, 4.0, 2.0],
            memory_limit_mb: 8192,
            iterations: 5,
            max_rung_fraction: 0.08,
        }
    }
}

impl Probe {
    pub fn load(path: &Path) -> Option<Self> {
        let data = fs::read_to_string(path).ok()?;
        serde_json::from_str(&data).ok()
    }

    pub fn save(&self, path: &Path) {
        let data = serde_json::to_string_pretty(self).expect("failed to encode probe");
        fs::write(path, data).unwrap_or_else(|e| panic!("failed to write {}: {e}", path.display()));
    }
}

pub fn probe(instance: &Instance, manifest: &Manifest, options: &ProbeOptions) -> Probe {
    let vertices: usize = instance.buildings.iter().map(|b| b.vertices.len()).sum();

    let mut coarsest = options.spacings.clone();
    coarsest.sort_by(|a, b| b.total_cmp(a));
    let reference_spacing = *coarsest.first().expect("at least one spacing");

    let before = resident_mb();
    let started = Instant::now();
    let data = build_coverage(
        instance,
        &CoverageConfig {
            candidate_spacing: reference_spacing,
            ..Default::default()
        },
    );
    let reference_seconds = started.elapsed().as_secs_f64();
    let reference_memory = (resident_mb() - before).max(1.0);
    let reference_candidates = data.candidates.len().max(1);

    let mut spacings: Vec<SpacingCost> = coarsest
        .iter()
        .map(|&spacing| {
            let candidates = if spacing == reference_spacing {
                reference_candidates
            } else {
                candidates_for_instance(instance, spacing, 0.0, 0).len()
            };
            let scale = candidates as f64 / reference_candidates as f64;
            let memory_mb = reference_memory * scale;
            SpacingCost {
                spacing,
                candidates,
                build_seconds: reference_seconds * scale,
                memory_mb,
                fits: memory_mb <= options.memory_limit_mb as f64,
                measured: spacing == reference_spacing,
            }
        })
        .collect();
    spacings.sort_by(|a, b| b.spacing.total_cmp(&a.spacing));

    let recommended_spacing = spacings
        .iter()
        .filter(|s| s.fits)
        .map(|s| s.spacing)
        .fold(f64::NEG_INFINITY, f64::max);
    let recommended_spacing = if recommended_spacing.is_finite() {
        recommended_spacing
    } else {
        reference_spacing
    };

    let scenarios: Vec<ScenarioCost> = manifest
        .scenarios
        .iter()
        .map(|s| {
            let config = super::trial::TrialConfig {
                spacing: reference_spacing,
                restarts: 1,
                ..Default::default()
            };

            let started = Instant::now();
            let setup_only =
                crate::solvers::select_lns_antennas(&data, s.k, s.tau, &config.options(0, 0));
            let fixed_seconds = started.elapsed().as_secs_f64();
            drop(setup_only);

            let started = Instant::now();
            let antennas = crate::solvers::select_lns_antennas(
                &data,
                s.k,
                s.tau,
                &config.options(0, options.iterations),
            );
            let seconds = started.elapsed().as_secs_f64();

            let mut scored = instance.clone();
            scored.k = s.k;
            scored.tau = s.tau;
            let serviced = framework::evaluate(&scored, &framework::Solution { antennas }).score;
            let iteration_seconds =
                ((seconds - fixed_seconds) / options.iterations.max(1) as f64).max(0.0);
            ScenarioCost {
                id: s.id.clone(),
                k: s.k,
                tau: s.tau,
                fixed_seconds,
                iteration_seconds,
                serviced,
                rungs: fitted_rungs(manifest, fixed_seconds, iteration_seconds, options),
            }
        })
        .collect();

    let (slow_fixed, slow_marginal) = scenarios.iter().fold((0.0f64, 0.0f64), |acc, s| {
        (acc.0.max(s.fixed_seconds), acc.1.max(s.iteration_seconds))
    });
    let rungs = fitted_rungs(manifest, slow_fixed, slow_marginal, options);

    Probe {
        buildings: instance.buildings.len(),
        vertices,
        spacings,
        recommended_spacing,
        memory_limit_mb: options.memory_limit_mb,
        scenarios,
        rungs,
    }
}

fn fitted_rungs(
    manifest: &Manifest,
    fixed_seconds: f64,
    iteration_seconds: f64,
    options: &ProbeOptions,
) -> Vec<usize> {
    let default = super::asha::RUNGS.to_vec();
    let campaign_seconds = manifest
        .deadline_unix
        .saturating_sub(manifest.start_unix.max(1)) as f64;
    if campaign_seconds <= 0.0 || iteration_seconds <= 0.0 {
        return default;
    }
    let budget = campaign_seconds * options.max_rung_fraction;
    let trimmed: Vec<usize> = default
        .iter()
        .copied()
        .filter(|&rung| fixed_seconds + rung as f64 * iteration_seconds <= budget)
        .collect();
    if trimmed.len() >= 2 {
        trimmed
    } else {
        default[..2].to_vec()
    }
}

fn resident_mb() -> f64 {
    let Ok(status) = fs::read_to_string("/proc/self/status") else {
        return 0.0;
    };
    status
        .lines()
        .find_map(|line| {
            let rest = line.strip_prefix("VmRSS:")?;
            let kb: f64 = rest.split_whitespace().next()?.parse().ok()?;
            Some(kb / 1024.0)
        })
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::super::manifest::Scenario;
    use super::*;

    fn manifest(hours: u64) -> Manifest {
        Manifest {
            dataset: "d.geojson".into(),
            scenarios: vec![Scenario::new(0.5, 100)],
            start_unix: 1,
            deadline_unix: 1 + hours * 3600,
            reserve_seconds: 0,
            duration_seconds: 0,
            spacings: vec![8.0],
            buildings: 0,
        }
    }

    #[test]
    fn a_fast_dataset_keeps_the_full_ladder() {
        let rungs = fitted_rungs(&manifest(20), 1.0, 0.05, &ProbeOptions::default());
        assert_eq!(rungs, super::super::asha::RUNGS.to_vec());
    }

    #[test]
    fn separating_fixed_from_marginal_cost_restores_the_ladder() {
        let options = ProbeOptions::default();
        let honest = fitted_rungs(&manifest(20), 32.1, 1.033, &options);
        assert_eq!(honest, super::super::asha::RUNGS.to_vec());

        let conflated = fitted_rungs(&manifest(20), 0.0, 24.475, &options);
        assert_eq!(conflated, super::super::asha::RUNGS[..2].to_vec());
    }

    #[test]
    fn a_huge_fixed_cost_still_trims() {
        let rungs = fitted_rungs(&manifest(20), 9000.0, 0.01, &ProbeOptions::default());
        assert_eq!(rungs.len(), 2, "nothing fits, but a ladder must remain");
    }

    #[test]
    fn a_slow_dataset_loses_its_top_rungs() {
        let rungs = fitted_rungs(&manifest(20), 0.0, 20.0, &ProbeOptions::default());
        assert!(rungs.len() < super::super::asha::RUNGS.len());
        let budget = 20.0 * 3600.0 * 0.05;
        assert!(
            *rungs.last().unwrap() as f64 * 20.0 <= budget,
            "the top rung must fit the per-trial budget"
        );
    }

    #[test]
    fn a_cheap_scenario_keeps_a_longer_ladder_than_an_expensive_one() {
        let options = ProbeOptions::default();
        let cheap = fitted_rungs(&manifest(20), 0.0, 0.6, &options);
        let expensive = fitted_rungs(&manifest(20), 0.0, 8.4, &options);
        assert!(
            cheap.len() > expensive.len(),
            "cheap {cheap:?} should outrun expensive {expensive:?}"
        );
    }

    #[test]
    fn there_is_always_a_ladder_to_climb() {
        let rungs = fitted_rungs(&manifest(20), 0.0, 9999.0, &ProbeOptions::default());
        assert_eq!(
            rungs.len(),
            2,
            "even an absurdly slow dataset keeps two rungs"
        );
    }

    #[test]
    fn an_unconfigured_deadline_keeps_the_default_ladder() {
        let mut m = manifest(20);
        m.deadline_unix = 0;
        assert_eq!(
            fitted_rungs(&m, 0.0, 20.0, &ProbeOptions::default()),
            super::super::asha::RUNGS.to_vec()
        );
    }
}

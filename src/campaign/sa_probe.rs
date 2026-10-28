use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use framework::Instance;
use serde::{Deserialize, Serialize};

use crate::coverage::{CoverageConfig, build_coverage};
use crate::solvers::{
    AnnealingTimedOptions, SimulatedAnnealingConfig, select_annealed_antennas_until_with_progress,
};

use super::manifest::{Manifest, Scenario};

const MATRIX_MARGIN: f64 = 1.25;
const NODE_MATRIX_FRACTION: f64 = 0.8;
const PROBE_HARD_FRACTION: f64 = 0.6;
const TARGET_FILL: f64 = 0.95;
const MAX_CALIBRATION_BUILDS: usize = 14;
const DENSITY_LEVELS: usize = 7;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SaProbe {
    pub buildings: usize,
    pub vertices: usize,
    pub spacings: Vec<SaSpacingCost>,
    pub recommended_spacing: f64,
    pub topology: WorkerTopology,
    pub node_memory_mb: u64,
    pub node_cores: usize,
    #[serde(default)]
    pub search_spacings: Vec<f64>,
    #[serde(default)]
    pub calibration: Vec<SaCalibrationPoint>,
    #[serde(default)]
    pub tiers: Vec<SaMemoryTier>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SaSpacingCost {
    pub spacing: f64,
    pub candidates: usize,
    pub build_seconds: f64,
    pub matrix_memory_mb: f64,
    pub fits: bool,
    pub scenarios: Vec<SaScenarioProbe>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SaCalibrationPoint {
    pub spacing: f64,
    pub candidates: usize,
    pub build_seconds: f64,
    pub matrix_memory_mb: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SaScenarioProbe {
    pub id: String,
    pub serviced: usize,
    pub seconds: f64,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkerTopology {
    pub workers_per_node: usize,
    pub chains_per_worker: usize,
    pub cpus_per_worker: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SaTierSpec {
    pub id: String,
    pub nominal_gib: usize,
    pub nodes: usize,
    pub workers_per_node: usize,
    pub chains_per_worker: usize,
    pub matrix_limit_mb: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SaMemoryTier {
    pub id: String,
    pub nominal_gib: usize,
    pub nodes: usize,
    pub workers_per_node: usize,
    pub chains_per_worker: usize,
    pub matrix_limit_mb: f64,
    pub spacing: f64,
    pub candidates: usize,
    pub build_seconds: f64,
    pub matrix_memory_mb: f64,
    pub aggregate_matrix_mb: f64,
    pub node_headroom_mb: f64,
    pub scenarios: Vec<SaScenarioProbe>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SaMemoryCheck {
    pub spacing: f64,
    pub chains: usize,
    pub candidates: usize,
    pub build_seconds: f64,
    pub matrix_memory_mb: f64,
    pub rss_after_build_mb: f64,
    pub peak_rss_mb: f64,
    pub scenario: String,
    pub serviced: usize,
    pub run_seconds: f64,
}

pub struct SaProbeOptions {
    pub spacings: Vec<f64>,
    pub trial_seconds: u64,
    pub node_memory_mb: u64,
    pub node_cores: usize,
    pub max_workers: usize,
    pub chains_per_worker: usize,
    pub tiers: Vec<SaTierSpec>,
}

impl Default for SaProbeOptions {
    fn default() -> Self {
        Self {
            spacings: vec![16.0, 12.0, 8.0],
            trial_seconds: 60,
            node_memory_mb: 64 * 1024,
            node_cores: 20,
            max_workers: 5,
            chains_per_worker: 4,
            tiers: Vec::new(),
        }
    }
}

pub fn default_sa_tiers() -> Vec<SaTierSpec> {
    vec![SaTierSpec {
        id: "asha".into(),
        nominal_gib: 8,
        nodes: 200,
        workers_per_node: 5,
        chains_per_worker: 4,
        matrix_limit_mb: 8.0 * 1024.0,
    }]
}

pub fn smoke_sa_tiers() -> Vec<SaTierSpec> {
    default_sa_tiers()
        .into_iter()
        .map(|mut tier| {
            tier.nodes = 3;
            tier
        })
        .collect()
}

impl SaProbe {
    pub fn load(path: &Path) -> Option<Self> {
        serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
    }

    pub fn save(&self, path: &Path) {
        let data = serde_json::to_string_pretty(self).expect("failed to encode SA probe");
        fs::write(path, data)
            .unwrap_or_else(|error| panic!("failed to write {}: {error}", path.display()));
    }

    pub fn save_portfolio(&self, path: &Path) {
        let mut data = String::from(
            "tier\tnominal_gib\tnodes\tworkers\tchains\tspacing\tmatrix_limit_mb\tmeasured_mb\tcandidates\tbuild_seconds\n",
        );
        for tier in &self.tiers {
            writeln!(
                data,
                "{}\t{}\t{}\t{}\t{}\t{:.17}\t{:.3}\t{:.3}\t{}\t{:.3}",
                tier.id,
                tier.nominal_gib,
                tier.nodes,
                tier.workers_per_node,
                tier.chains_per_worker,
                tier.spacing,
                tier.matrix_limit_mb,
                tier.matrix_memory_mb,
                tier.candidates,
                tier.build_seconds,
            )
            .expect("writing to a string cannot fail");
        }
        fs::write(path, data)
            .unwrap_or_else(|error| panic!("failed to write {}: {error}", path.display()));
    }

    pub fn save_densities(&self, path: &Path) {
        let data = self
            .search_spacings
            .iter()
            .map(|spacing| format!("{spacing:.17}"))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(path, format!("{data}\n"))
            .unwrap_or_else(|error| panic!("failed to write {}: {error}", path.display()));
    }

    pub fn production_spacings(&self) -> Vec<f64> {
        if self.search_spacings.is_empty() {
            return vec![self.recommended_spacing];
        }
        let first = self.search_spacings.len().saturating_sub(3);
        self.search_spacings[first..].to_vec()
    }
}

pub fn sa_probe(instance: &Instance, manifest: &Manifest, options: &SaProbeOptions) -> SaProbe {
    validate_options(options);
    if options.tiers.is_empty() {
        legacy_probe(instance, manifest, options)
    } else {
        portfolio_probe(instance, options)
    }
}

fn validate_options(options: &SaProbeOptions) {
    assert!(!options.spacings.is_empty(), "probe needs a spacing");
    assert!(options.trial_seconds > 0, "probe trial must be positive");
    assert!(options.chains_per_worker > 0, "probe needs a chain");
    assert!(options.node_memory_mb > 0, "node memory must be positive");
    assert!(options.node_cores > 0, "node core count must be positive");
    for spacing in &options.spacings {
        assert!(spacing.is_finite() && *spacing > 0.0, "invalid spacing");
    }
    validate_tiers(&options.tiers, options.node_memory_mb, options.node_cores);
}

fn validate_tiers(tiers: &[SaTierSpec], node_memory_mb: u64, node_cores: usize) {
    for tier in tiers {
        assert!(!tier.id.is_empty(), "memory tier needs an id");
        assert!(tier.nodes > 0, "memory tier needs a node");
        assert!(tier.workers_per_node > 0, "memory tier needs a worker");
        assert!(tier.chains_per_worker > 0, "memory tier needs a chain");
        assert!(
            tier.workers_per_node * tier.chains_per_worker <= node_cores,
            "tier {} asks for more than {node_cores} cores",
            tier.id
        );
        assert!(
            tier.matrix_limit_mb.is_finite() && tier.matrix_limit_mb > 0.0,
            "tier {} has an invalid matrix limit",
            tier.id
        );
        assert!(
            tier.matrix_limit_mb * tier.workers_per_node as f64
                <= node_memory_mb as f64 * NODE_MATRIX_FRACTION,
            "tier {} leaves less than 20% node memory outside its matrix guards",
            tier.id
        );
    }
}

fn legacy_probe(instance: &Instance, manifest: &Manifest, options: &SaProbeOptions) -> SaProbe {
    let mut requested = options.spacings.clone();
    requested.sort_by(|a, b| b.total_cmp(a));
    requested.dedup_by(|a, b| a.to_bits() == b.to_bits());
    let rows: Vec<_> = requested
        .into_iter()
        .map(|spacing| {
            measure_spacing(
                instance,
                manifest,
                spacing,
                options.chains_per_worker,
                options.trial_seconds,
                |memory| topology_for(memory, options).workers_per_node > 0,
            )
        })
        .collect();
    let recommended_spacing = recommend(&rows).unwrap_or(rows[0].spacing);
    let recommended = rows
        .iter()
        .find(|row| same_spacing(row.spacing, recommended_spacing))
        .expect("recommended row exists");
    let topology = topology_for(recommended.matrix_memory_mb, options);
    SaProbe {
        buildings: instance.buildings.len(),
        vertices: vertex_count(instance),
        spacings: rows,
        recommended_spacing,
        topology,
        node_memory_mb: options.node_memory_mb,
        node_cores: options.node_cores,
        search_spacings: vec![recommended_spacing],
        calibration: Vec::new(),
        tiers: Vec::new(),
    }
}

fn portfolio_probe(instance: &Instance, options: &SaProbeOptions) -> SaProbe {
    let calibration = calibrate(instance, options);
    let mut tiers = Vec::with_capacity(options.tiers.len());
    let mut rows = Vec::with_capacity(options.tiers.len());

    for spec in &options.tiers {
        let selected =
            select_calibration(&calibration, spec.matrix_limit_mb).unwrap_or_else(|| {
                panic!(
                    "even the coarsest measured matrix exceeds the {:.0} MB guard for {}",
                    spec.matrix_limit_mb, spec.id
                )
            });
        let row = SaSpacingCost {
            spacing: selected.spacing,
            candidates: selected.candidates,
            build_seconds: selected.build_seconds,
            matrix_memory_mb: selected.matrix_memory_mb,
            fits: true,
            scenarios: Vec::new(),
        };
        let aggregate_matrix_mb = row.matrix_memory_mb * spec.workers_per_node as f64;
        tiers.push(SaMemoryTier {
            id: spec.id.clone(),
            nominal_gib: spec.nominal_gib,
            nodes: spec.nodes,
            workers_per_node: spec.workers_per_node,
            chains_per_worker: spec.chains_per_worker,
            matrix_limit_mb: spec.matrix_limit_mb,
            spacing: row.spacing,
            candidates: row.candidates,
            build_seconds: row.build_seconds,
            matrix_memory_mb: row.matrix_memory_mb,
            aggregate_matrix_mb,
            node_headroom_mb: options.node_memory_mb as f64 - aggregate_matrix_mb,
            scenarios: row.scenarios.clone(),
        });
        rows.push(row);
    }

    let recommended = tiers
        .iter()
        .max_by_key(|tier| tier.nodes)
        .expect("portfolio has a tier");
    let recommended_spacing = recommended.spacing;
    let topology = WorkerTopology {
        workers_per_node: recommended.workers_per_node,
        chains_per_worker: recommended.chains_per_worker,
        cpus_per_worker: recommended.chains_per_worker,
    };
    SaProbe {
        buildings: instance.buildings.len(),
        vertices: vertex_count(instance),
        spacings: rows,
        recommended_spacing,
        topology,
        node_memory_mb: options.node_memory_mb,
        node_cores: options.node_cores,
        search_spacings: density_ladder(recommended_spacing),
        calibration,
        tiers,
    }
}

fn calibrate(instance: &Instance, options: &SaProbeOptions) -> Vec<SaCalibrationPoint> {
    let target = options
        .tiers
        .iter()
        .map(|tier| tier.matrix_limit_mb)
        .fold(0.0f64, f64::max);
    let hard_limit = options.node_memory_mb as f64 * PROBE_HARD_FRACTION;
    assert!(
        target < hard_limit,
        "largest matrix target must leave probe-process headroom"
    );

    let mut spacing = options
        .spacings
        .iter()
        .copied()
        .max_by(f64::total_cmp)
        .expect("probe spacing exists");
    let mut points = Vec::new();
    for _ in 0..MAX_CALIBRATION_BUILDS {
        let point = measure_build(instance, spacing);
        let memory = point.matrix_memory_mb;
        points.push(point);
        if select_calibration(&points, target)
            .is_some_and(|point| point.matrix_memory_mb >= target * 0.9)
        {
            break;
        }

        if let Some(next) = interpolated_spacing(&points, target * TARGET_FILL) {
            spacing = next;
            continue;
        }
        if select_calibration(&points, target).is_none() {
            spacing *= 2.0;
            continue;
        }
        if memory >= hard_limit {
            break;
        }

        let exponent = growth_exponent(&points);
        let desired_growth = (target * TARGET_FILL / memory).max(1.0);
        let hard_growth = (hard_limit * 0.9 / memory).max(1.0);
        let density = desired_growth
            .min(hard_growth)
            .powf(1.0 / exponent)
            .clamp(1.15, 2.0);
        let next = spacing / density;
        if !next.is_finite() || next <= 0.0 || next >= spacing * 0.999 {
            break;
        }
        spacing = next;
    }

    let mut targets: Vec<f64> = options
        .tiers
        .iter()
        .map(|tier| tier.matrix_limit_mb)
        .collect();
    targets.sort_by(f64::total_cmp);
    targets.dedup_by(|a, b| a.to_bits() == b.to_bits());
    for tier_target in targets {
        for _ in 0..2 {
            let Some(selected) = select_calibration(&points, tier_target) else {
                break;
            };
            if selected.matrix_memory_mb >= tier_target * 0.75 {
                break;
            }
            let Some(next) = interpolated_spacing(&points, tier_target * TARGET_FILL) else {
                break;
            };
            let point = measure_build(instance, next);
            if point.matrix_memory_mb >= hard_limit {
                points.push(point);
                break;
            }
            points.push(point);
        }
    }

    assert!(
        points.iter().any(|point| point.matrix_memory_mb <= target),
        "calibration found no matrix below the largest memory guard"
    );
    points
}

fn density_ladder(finest: f64) -> Vec<f64> {
    (0..DENSITY_LEVELS)
        .map(|level| finest * 2.0f64.powi(level as i32))
        .collect()
}

fn interpolated_spacing(points: &[SaCalibrationPoint], goal_mb: f64) -> Option<f64> {
    let below = points
        .iter()
        .filter(|point| point.matrix_memory_mb < goal_mb)
        .max_by(|a, b| a.matrix_memory_mb.total_cmp(&b.matrix_memory_mb))?;
    let above = points
        .iter()
        .filter(|point| point.matrix_memory_mb > goal_mb)
        .min_by(|a, b| a.matrix_memory_mb.total_cmp(&b.matrix_memory_mb))?;
    if below.spacing <= above.spacing {
        return None;
    }
    let exponent = (above.matrix_memory_mb / below.matrix_memory_mb).ln()
        / (below.spacing / above.spacing).ln();
    let exponent = if exponent.is_finite() {
        exponent.clamp(0.35, 2.0)
    } else {
        1.0
    };
    let estimated = below.spacing / (goal_mb / below.matrix_memory_mb).powf(1.0 / exponent);
    let lower = above.spacing * 1.001;
    let upper = below.spacing * 0.999;
    if lower >= upper {
        return None;
    }
    let next = estimated.clamp(lower, upper);
    (!points
        .iter()
        .any(|point| (point.spacing - next).abs() <= next.abs() * 1e-9))
    .then_some(next)
}

fn growth_exponent(points: &[SaCalibrationPoint]) -> f64 {
    let [.., previous, current] = points else {
        return 1.0;
    };
    let spacing_ratio = previous.spacing / current.spacing;
    let memory_ratio = current.matrix_memory_mb / previous.matrix_memory_mb;
    if spacing_ratio <= 1.0 || memory_ratio <= 1.0 {
        1.0
    } else {
        (memory_ratio.ln() / spacing_ratio.ln()).clamp(0.35, 2.0)
    }
}

fn select_calibration(
    points: &[SaCalibrationPoint],
    matrix_limit_mb: f64,
) -> Option<&SaCalibrationPoint> {
    points
        .iter()
        .filter(|point| point.matrix_memory_mb <= matrix_limit_mb)
        .max_by(|a, b| {
            a.matrix_memory_mb
                .total_cmp(&b.matrix_memory_mb)
                .then_with(|| b.spacing.total_cmp(&a.spacing))
        })
}

fn measure_build(instance: &Instance, spacing: f64) -> SaCalibrationPoint {
    let rss_before = resident_mb();
    let started = Instant::now();
    let data = build_coverage(
        instance,
        &CoverageConfig {
            candidate_spacing: spacing,
            ..Default::default()
        },
    );
    SaCalibrationPoint {
        spacing,
        candidates: data.candidates.len(),
        build_seconds: started.elapsed().as_secs_f64(),
        matrix_memory_mb: guarded_matrix_mb(data.estimated_bytes(), rss_before),
    }
}

fn measure_spacing(
    instance: &Instance,
    manifest: &Manifest,
    spacing: f64,
    chains: usize,
    trial_seconds: u64,
    fits: impl FnOnce(f64) -> bool,
) -> SaSpacingCost {
    let rss_before = resident_mb();
    let started = Instant::now();
    let data = build_coverage(
        instance,
        &CoverageConfig {
            candidate_spacing: spacing,
            ..Default::default()
        },
    );
    let build_seconds = started.elapsed().as_secs_f64();
    let matrix_memory_mb = guarded_matrix_mb(data.estimated_bytes(), rss_before);
    let scenarios = manifest
        .scenarios
        .iter()
        .map(|scenario| run_short_sa(&data, scenario, chains, trial_seconds, 0))
        .collect();
    SaSpacingCost {
        spacing,
        candidates: data.candidates.len(),
        build_seconds,
        matrix_memory_mb,
        fits: fits(matrix_memory_mb),
        scenarios,
    }
}

pub fn sa_memory_check(
    instance: &Instance,
    scenario: &Scenario,
    spacing: f64,
    chains: usize,
    seconds: u64,
    seed: u64,
) -> SaMemoryCheck {
    assert!(spacing.is_finite() && spacing > 0.0, "invalid spacing");
    assert!(chains > 0, "memory check needs a chain");
    assert!(seconds > 0, "memory check duration must be positive");
    let rss_before = resident_mb();
    let started = Instant::now();
    let data = build_coverage(
        instance,
        &CoverageConfig {
            candidate_spacing: spacing,
            ..Default::default()
        },
    );
    let build_seconds = started.elapsed().as_secs_f64();
    let matrix_memory_mb = guarded_matrix_mb(data.estimated_bytes(), rss_before);
    let rss_after_build_mb = resident_mb();
    let result = run_short_sa(&data, scenario, chains, seconds, seed);
    SaMemoryCheck {
        spacing,
        chains,
        candidates: data.candidates.len(),
        build_seconds,
        matrix_memory_mb,
        rss_after_build_mb,
        peak_rss_mb: peak_resident_mb(),
        scenario: scenario.id.clone(),
        serviced: result.serviced,
        run_seconds: result.seconds,
    }
}

fn run_short_sa(
    data: &crate::coverage::CoverageData,
    scenario: &Scenario,
    chains: usize,
    seconds: u64,
    seed: u64,
) -> SaScenarioProbe {
    let config = SimulatedAnnealingConfig {
        iterations: usize::MAX,
        proposal_samples: sa_proposal_samples(scenario.k),
        seed: Some(seed),
        chains,
        swap_interval: 0,
        ..Default::default()
    };
    let started = Instant::now();
    let (_, serviced) = select_annealed_antennas_until_with_progress(
        data,
        scenario.k,
        scenario.tau,
        &config,
        AnnealingTimedOptions {
            resume: None,
            seeds: None,
            preferred_start: data.candidates.len(),
            deadline: started + Duration::from_secs(seconds),
            progress: None,
        },
    );
    SaScenarioProbe {
        id: scenario.id.clone(),
        serviced,
        seconds: started.elapsed().as_secs_f64(),
    }
}

pub(super) fn sa_proposal_samples(k: usize) -> usize {
    if k >= 500 { 16 } else { 8 }
}

fn guarded_matrix_mb(estimated_bytes: usize, rss_before: f64) -> f64 {
    let estimated = estimated_bytes as f64 / (1024.0 * 1024.0);
    let rss_growth = (resident_mb() - rss_before).max(0.0);
    (estimated * MATRIX_MARGIN).max(rss_growth).max(1.0)
}

fn topology_for(matrix_memory_mb: f64, options: &SaProbeOptions) -> WorkerTopology {
    let memory_workers = ((options.node_memory_mb as f64 * NODE_MATRIX_FRACTION) / matrix_memory_mb)
        .floor() as usize;
    let core_workers = options.node_cores / options.chains_per_worker;
    let workers_per_node = memory_workers.min(core_workers).min(options.max_workers);
    WorkerTopology {
        workers_per_node,
        chains_per_worker: options.chains_per_worker,
        cpus_per_worker: options.chains_per_worker,
    }
}

fn recommend(rows: &[SaSpacingCost]) -> Option<f64> {
    let viable: Vec<_> = rows.iter().filter(|row| row.fits).collect();
    let scenario_count = viable.first().map_or(1, |row| row.scenarios.len()).max(1);
    let score = |row: &SaSpacingCost| {
        row.scenarios
            .iter()
            .map(|scenario| {
                let best = viable
                    .iter()
                    .flat_map(|candidate| &candidate.scenarios)
                    .filter(|candidate| candidate.id == scenario.id)
                    .map(|candidate| candidate.serviced)
                    .max()
                    .unwrap_or(1)
                    .max(1);
                scenario.serviced as f64 / best as f64
            })
            .sum::<f64>()
            / scenario_count as f64
    };
    let best = viable
        .iter()
        .map(|row| score(row))
        .fold(f64::NEG_INFINITY, f64::max);
    viable
        .iter()
        .filter(|row| score(row) + 0.002 >= best)
        .max_by(|a, b| a.spacing.total_cmp(&b.spacing))
        .map(|row| row.spacing)
}

fn vertex_count(instance: &Instance) -> usize {
    instance
        .buildings
        .iter()
        .map(|building| building.vertices.len())
        .sum()
}

fn same_spacing(a: f64, b: f64) -> bool {
    a.to_bits() == b.to_bits()
}

fn status_mb(key: &str) -> f64 {
    let Ok(status) = fs::read_to_string("/proc/self/status") else {
        return 0.0;
    };
    status
        .lines()
        .find_map(|line| {
            let rest = line.strip_prefix(key)?;
            let kb: f64 = rest.split_whitespace().next()?.parse().ok()?;
            Some(kb / 1024.0)
        })
        .unwrap_or(0.0)
}

fn resident_mb() -> f64 {
    status_mb("VmRSS:")
}

fn peak_resident_mb() -> f64 {
    status_mb("VmHWM:")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> SaProbeOptions {
        SaProbeOptions::default()
    }

    fn row(spacing: f64, memory: f64, scores: &[(&str, usize)]) -> SaSpacingCost {
        SaSpacingCost {
            spacing,
            candidates: 1,
            build_seconds: 1.0,
            matrix_memory_mb: memory,
            fits: true,
            scenarios: scores
                .iter()
                .map(|(id, serviced)| SaScenarioProbe {
                    id: (*id).into(),
                    serviced: *serviced,
                    seconds: 1.0,
                })
                .collect(),
        }
    }

    #[test]
    fn four_chains_and_five_workers_fill_a_twenty_core_node() {
        assert_eq!(
            topology_for(1_000.0, &options()),
            WorkerTopology {
                workers_per_node: 5,
                chains_per_worker: 4,
                cpus_per_worker: 4,
            }
        );
    }

    #[test]
    fn memory_reduces_worker_copies_without_reducing_chains() {
        let topology = topology_for(20_000.0, &options());
        assert_eq!(topology.workers_per_node, 2);
        assert_eq!(topology.chains_per_worker, 4);
    }

    #[test]
    fn near_tied_quality_prefers_the_coarser_spacing() {
        let rows = vec![
            row(16.0, 1.0, &[("a", 999), ("b", 1000)]),
            row(8.0, 2.0, &[("a", 1000), ("b", 1000)]),
        ];
        assert_eq!(recommend(&rows), Some(16.0));
    }

    #[test]
    fn portfolio_fills_cores_and_keeps_twenty_percent_for_non_matrix_memory() {
        let tiers = default_sa_tiers();
        validate_tiers(&tiers, 64 * 1024, 20);
        assert_eq!(tiers.iter().map(|tier| tier.nodes).sum::<usize>(), 200);
        assert!(tiers.iter().all(|tier| {
            tier.workers_per_node * tier.chains_per_worker == 20
                && tier.matrix_limit_mb * tier.workers_per_node as f64 <= 64.0 * 1024.0 * 0.8
        }));
    }

    #[test]
    fn smoke_uses_three_nodes_with_the_production_layout() {
        let production = default_sa_tiers();
        let smoke = smoke_sa_tiers();
        assert_eq!(smoke.len(), 1);
        for (full, small) in production.iter().zip(&smoke) {
            assert_eq!(small.nodes, 3);
            assert_eq!(small.id, full.id);
            assert_eq!(small.workers_per_node, full.workers_per_node);
            assert_eq!(small.chains_per_worker, full.chains_per_worker);
            assert_eq!(small.matrix_limit_mb, full.matrix_limit_mb);
        }
    }

    #[test]
    fn tier_selection_never_crosses_its_guard() {
        let points = vec![
            SaCalibrationPoint {
                spacing: 4.0,
                candidates: 1,
                build_seconds: 1.0,
                matrix_memory_mb: 4_000.0,
            },
            SaCalibrationPoint {
                spacing: 2.0,
                candidates: 2,
                build_seconds: 2.0,
                matrix_memory_mb: 9_000.0,
            },
            SaCalibrationPoint {
                spacing: 1.0,
                candidates: 3,
                build_seconds: 3.0,
                matrix_memory_mb: 20_000.0,
            },
        ];
        assert_eq!(select_calibration(&points, 8_192.0).unwrap().spacing, 4.0);
        assert_eq!(select_calibration(&points, 12_288.0).unwrap().spacing, 2.0);
        assert_eq!(select_calibration(&points, 24_576.0).unwrap().spacing, 1.0);
    }

    #[test]
    fn density_ladder_starts_at_the_probed_minimum_and_gets_coarser() {
        assert_eq!(
            density_ladder(0.5),
            vec![0.5, 1.0, 2.0, 4.0, 8.0, 16.0, 32.0]
        );
    }

    #[test]
    fn production_uses_the_three_coarsest_calibrated_spacings() {
        let probe = SaProbe {
            buildings: 0,
            vertices: 0,
            spacings: Vec::new(),
            recommended_spacing: 1.0,
            topology: WorkerTopology {
                workers_per_node: 1,
                chains_per_worker: 1,
                cpus_per_worker: 1,
            },
            node_memory_mb: 1,
            node_cores: 1,
            search_spacings: vec![1.0, 2.0, 4.0],
            calibration: Vec::new(),
            tiers: Vec::new(),
        };
        assert_eq!(probe.production_spacings(), vec![1.0, 2.0, 4.0]);
    }

    #[test]
    fn large_antenna_budgets_use_the_wider_proposal_batch() {
        assert_eq!(sa_proposal_samples(50), 8);
        assert_eq!(sa_proposal_samples(499), 8);
        assert_eq!(sa_proposal_samples(500), 16);
        assert_eq!(sa_proposal_samples(1000), 16);
    }
}

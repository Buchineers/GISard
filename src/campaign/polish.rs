use std::path::Path;

use framework::{Instance, Solution, evaluate, parse_antennas};

use crate::coverage::CoverageConfig;
use crate::solvers::{WindowCentres, WindowOptions, repair_windows};

use super::manifest::Manifest;
use super::queue::Queue;

pub const WINDOW_RADIUS: f64 = 60.0;

pub const MAX_WINDOWS: usize = 100;

pub struct PolishOptions {
    pub radius: f64,
    pub windows: Option<usize>,
    pub antennas_per_window: usize,
    pub time_limit: f64,
    pub rounds: usize,
    pub spacing: f64,
}

impl Default for PolishOptions {
    fn default() -> Self {
        PolishOptions {
            radius: WINDOW_RADIUS,
            windows: None,
            antennas_per_window: 2,
            time_limit: 60.0,
            rounds: 1,
            spacing: 8.0,
        }
    }
}

pub struct PolishResult {
    pub scenario: String,
    pub before: usize,
    pub after: usize,
    pub seconds: f64,
    pub kept: bool,
}

pub fn polish(
    campaign: &Path,
    instance: &Instance,
    manifest: &Manifest,
    options: &PolishOptions,
) -> Vec<PolishResult> {
    let queue = Queue::new(campaign);
    let mut out = Vec::new();

    for scenario in &manifest.scenarios {
        let Some(source) = super::harvest::best_solution(campaign, &scenario.id) else {
            continue;
        };
        let antennas = parse_antennas(source.to_str().expect("non-utf8 solution path"));
        let mut scoped = instance.clone();
        scoped.k = scenario.k;
        scoped.tau = scenario.tau;

        let solution = Solution { antennas };
        let before = evaluate(&scoped, &solution).score;

        let started = std::time::Instant::now();
        let mut repaired = solution;
        let mut after = before;
        for centres in [WindowCentres::Antenna, WindowCentres::OpenBuilding] {
            let window = WindowOptions {
                antennas: options.antennas_per_window,
                windows: options.windows.unwrap_or(scenario.k.min(MAX_WINDOWS)),
                time_limit: options.time_limit,
                max_radius: Some(options.radius),
                centres,
                coverage: CoverageConfig {
                    candidate_spacing: options.spacing,
                    ..Default::default()
                },
                ..Default::default()
            };
            let (candidate, _) = repair_windows(&scoped, &repaired, &window, options.rounds);
            let score = evaluate(&scoped, &candidate).score;
            if score > after {
                after = score;
                repaired = candidate;
            }
        }
        let seconds = started.elapsed().as_secs_f64();

        let kept = after > before;
        if kept {
            let _ = queue.record_candidate(
                &scenario.id,
                after,
                &format!("polish{}", options.rounds),
                &repaired.antennas,
            );
        }
        out.push(PolishResult {
            scenario: scenario.id.clone(),
            before,
            after,
            seconds,
            kept,
        });
    }
    out
}

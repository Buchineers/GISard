
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use framework::{Instance, Point, Solution, evaluate, parse_antennas};
use serde::{Deserialize, Serialize};

use crate::coverage::CoverageConfig;
use crate::solvers::{
    Shard, WindowCentres, WindowOptions, WindowResult, apply_window_results, certify_windows,
};

use super::events::{CampaignEvent, EventLog, now_ms};
use super::manifest::{Manifest, Scenario};
use super::polish::{PolishOptions, WINDOW_RADIUS};
use super::queue::Queue;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ShardCentres {
    Antenna,
    OpenBuilding,
    #[default]
    Both,
}

impl ShardCentres {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "antenna" => Some(ShardCentres::Antenna),
            "open" | "open-building" => Some(ShardCentres::OpenBuilding),
            "both" => Some(ShardCentres::Both),
            _ => None,
        }
    }

    fn passes(self) -> Vec<WindowCentres> {
        match self {
            ShardCentres::Antenna => vec![WindowCentres::Antenna],
            ShardCentres::OpenBuilding => vec![WindowCentres::OpenBuilding],
            ShardCentres::Both => vec![WindowCentres::Antenna, WindowCentres::OpenBuilding],
        }
    }
}

pub struct ShardOptions {
    pub radius: f64,
    pub windows: Option<usize>,
    pub antennas_per_window: usize,
    pub time_limit: f64,
    pub spacing: f64,
    pub centres: ShardCentres,
}

impl Default for ShardOptions {
    fn default() -> Self {
        let polish = PolishOptions::default();
        ShardOptions {
            radius: WINDOW_RADIUS,
            windows: None,
            antennas_per_window: polish.antennas_per_window,
            time_limit: polish.time_limit,
            spacing: polish.spacing,
            centres: ShardCentres::default(),
        }
    }
}

#[derive(Serialize, Deserialize)]
struct WireResult {
    affected_buildings: Vec<u32>,
    decisive: usize,
    candidates: usize,
    incumbent: usize,
    best: usize,
    proved: bool,
    improvement: Option<Vec<(f64, f64)>>,
    freed: Vec<usize>,
}

impl WireResult {
    fn from(result: &WindowResult) -> Self {
        WireResult {
            affected_buildings: result.affected_buildings.clone(),
            decisive: result.decisive,
            candidates: result.candidates,
            incumbent: result.incumbent,
            best: result.best,
            proved: result.proved,
            improvement: result
                .improvement
                .as_ref()
                .map(|points| points.iter().map(|p| (p.x, p.y)).collect()),
            freed: result.freed.clone(),
        }
    }

    fn into_result(self) -> WindowResult {
        WindowResult {
            affected_buildings: self.affected_buildings,
            decisive: self.decisive,
            candidates: self.candidates,
            incumbent: self.incumbent,
            best: self.best,
            proved: self.proved,
            improvement: self
                .improvement
                .map(|points| points.into_iter().map(|(x, y)| Point { x, y }).collect()),
            freed: self.freed,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct ShardFile {
    scenario: String,
    shard: usize,
    shards: usize,
    incumbent: String,
    incumbent_score: usize,
    windows: usize,
    seconds: f64,
    results: Vec<WireResult>,
}

fn fingerprint(antennas: &[Point]) -> String {
    let mut hasher = blake3::Hasher::new();
    for a in antennas {
        hasher.update(format!("{:?},{:?}\n", a.x, a.y).as_bytes());
    }
    hasher.finalize().to_hex()[..16].to_string()
}

fn scoped(instance: &Instance, scenario: &Scenario) -> Instance {
    let mut scoped = instance.clone();
    scoped.k = scenario.k;
    scoped.tau = scenario.tau;
    scoped
}

fn scenario_of<'a>(manifest: &'a Manifest, id: &str) -> &'a Scenario {
    manifest
        .scenarios
        .iter()
        .find(|s| s.id == id)
        .unwrap_or_else(|| panic!("no scenario {id} in manifest"))
}

pub fn incumbent(campaign: &Path, scenario: &str) -> Option<Solution> {
    let source = super::harvest::best_solution(campaign, scenario)?;
    let antennas = parse_antennas(source.to_str().expect("non-utf8 solution path"));
    Some(Solution { antennas })
}

pub fn certify_shard(
    campaign: &Path,
    instance: &Instance,
    manifest: &Manifest,
    scenario_id: &str,
    shard: Shard,
    options: &ShardOptions,
    out: &Path,
) -> io::Result<usize> {
    let scenario = scenario_of(manifest, scenario_id);
    let scoped = scoped(instance, scenario);
    let solution = incumbent(campaign, scenario_id)
        .unwrap_or_else(|| panic!("no incumbent solution for {scenario_id}"));
    let incumbent_score = evaluate(&scoped, &solution).score;

    let started = std::time::Instant::now();
    let mut results = Vec::new();
    for centres in options.centres.passes() {
        let window = WindowOptions {
            antennas: options.antennas_per_window,
            windows: options.windows.unwrap_or(scenario.k),
            time_limit: options.time_limit,
            max_radius: Some(options.radius),
            centres,
            shard: Some(shard),
            coverage: CoverageConfig {
                candidate_spacing: options.spacing,
                ..Default::default()
            },
            ..Default::default()
        };
        results.extend(certify_windows(&scoped, &solution, &window).results);
    }
    let seconds = started.elapsed().as_secs_f64();

    let file = ShardFile {
        scenario: scenario_id.to_string(),
        shard: shard.index,
        shards: shard.count,
        incumbent: fingerprint(&solution.antennas),
        incumbent_score,
        windows: results.len(),
        seconds,
        results: results.iter().map(WireResult::from).collect(),
    };
    if let Some(parent) = out.parent() {
        fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_vec(&file).expect("shard file serializes");
    let temporary = out.with_extension(format!("{}.tmp", std::process::id()));
    fs::write(&temporary, &body)?;
    fs::rename(&temporary, out)?;
    Ok(results.len())
}

pub struct MergeReport {
    pub scenario: String,
    pub shards: usize,
    pub windows: usize,
    pub proved: usize,
    pub improving: usize,
    pub applied: usize,
    pub before: usize,
    pub after: usize,
    pub kept: bool,
}

pub fn merge_shards(
    campaign: &Path,
    instance: &Instance,
    manifest: &Manifest,
    scenario_id: &str,
    dir: &Path,
) -> io::Result<MergeReport> {
    let scenario = scenario_of(manifest, scenario_id);
    let scoped = scoped(instance, scenario);
    let Some(solution) = incumbent(campaign, scenario_id) else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("no incumbent solution for {scenario_id}"),
        ));
    };
    let expected = fingerprint(&solution.antennas);

    let mut files: Vec<PathBuf> = fs::read_dir(dir)?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|e| e == "json"))
        .collect();
    files.sort();

    let mut results: Vec<WindowResult> = Vec::new();
    let mut shards = 0;
    let mut seen: Vec<usize> = Vec::new();
    let mut declared = 0;
    for path in &files {
        let body = fs::read(path)?;
        let file: ShardFile =
            serde_json::from_slice(&body).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        if file.scenario != scenario_id {
            continue;
        }
        assert_eq!(
            file.incumbent,
            expected,
            "{}: certified against a different incumbent ({} vs {expected}) — \
             the incumbent moved under the sweep, so these verdicts cannot be merged",
            path.display(),
            file.incumbent,
        );
        declared = declared.max(file.shards);
        seen.push(file.shard);
        shards += 1;
        results.extend(file.results.into_iter().map(WireResult::into_result));
    }
    if shards == 0 {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("no shard files in {}", dir.display()),
        ));
    }
    seen.sort_unstable();
    seen.dedup();
    if seen.len() != declared {
        eprintln!(
            "warning: {scenario_id} merged {} of {declared} shards — \
             missing {:?}; the sweep is incomplete but the merge is still sound",
            seen.len(),
            (0..declared).filter(|i| !seen.contains(i)).collect::<Vec<_>>(),
        );
    }

    let windows = results.len();
    let proved = results.iter().filter(|r| r.proved).count();
    let improving = results.iter().filter(|r| r.improved()).count();
    let before = evaluate(&scoped, &solution).score;
    let (merged, applied) = apply_window_results(&scoped, &solution, &results);
    let after = evaluate(&scoped, &merged).score;

    let kept = after > before;
    if kept {
        let queue = Queue::new(campaign);
        let _ = queue.record_candidate(scenario_id, after, "polish-sharded", &merged.antennas);
    }

    if let Ok(log) = EventLog::open(campaign, "polish-merge") {
        log.record(&CampaignEvent {
            unix_ms: now_ms(),
            kind: "polish_merge".to_string(),
            worker: "polish-merge".to_string(),
            trial: None,
            scenario: Some(scenario_id.to_string()),
            score: Some(after),
            rung: None,
            spacing: None,
            detail: Some(format!(
                "before={before} after={after} gain={} shards={shards} windows={windows} \
                 proved={proved} improving={improving} applied={applied} kept={kept}",
                after as i64 - before as i64,
            )),
        });
    }

    Ok(MergeReport {
        scenario: scenario_id.to_string(),
        shards,
        windows,
        proved,
        improving,
        applied,
        before,
        after,
        kept,
    })
}

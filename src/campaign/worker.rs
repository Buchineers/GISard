use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use framework::{Instance, Point};

use crate::coverage::{CoverageData, build_coverage};
use crate::solvers::{
    AnnealingProgress, AnnealingTimedOptions, LnsProgress, SimulatedAnnealingConfig,
    annealing_seed_indices, select_annealed_antennas_until_with_progress,
    select_lns_antennas_resuming, select_lns_antennas_with_progress,
};

use super::manifest::{Manifest, Scenario};
use super::policy::{Policy, Proposal};
use super::queue::Queue;
use super::trial::{Trial, TrialResult};
use super::{CampaignEvent, EventLog};

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before 1970")
        .as_secs()
}

pub fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before 1970")
        .as_millis() as u64
}

pub struct Outcome {
    pub antennas: Vec<Point>,
    pub serviced: usize,
}

pub type Checkpoint = Arc<dyn Fn(usize, &[Point]) + Send + Sync>;

pub trait TrialRunner {
    fn run(
        &mut self,
        trial: &Trial,
        scenario: &Scenario,
        resume: Option<&[Point]>,
        checkpoint: &Checkpoint,
    ) -> Outcome;

    fn estimate(&self, trial: &Trial, scenario: &Scenario) -> f64;

    fn cached_spacing(&self) -> Option<f64> {
        None
    }
}

#[derive(Debug, Default)]
pub struct WorkerStats {
    pub completed: usize,
    pub claims_lost: usize,
    pub skipped: usize,
    pub idle_polls: usize,
    pub stopped_for_deadline: bool,
}

pub const CLAIM_LEASE_SECONDS: u64 = 2 * 3600;

const REFRESH_SECONDS: u64 = 30;

const IDLE_POLL_SECONDS: u64 = 5;

pub const HEARTBEAT_SECONDS: u64 = 30;

pub const HEARTBEAT_STALE_SECONDS: u64 = 3 * HEARTBEAT_SECONDS;

const HEARTBEAT_EXTENSION: &str = "heartbeat";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkerState {
    Starting,
    Running,
    Stopped,
}

impl WorkerState {
    fn label(self) -> &'static str {
        match self {
            WorkerState::Starting => "starting",
            WorkerState::Running => "running",
            WorkerState::Stopped => "stopped",
        }
    }

    fn parse(label: &str) -> Option<Self> {
        match label {
            "starting" => Some(WorkerState::Starting),
            "running" => Some(WorkerState::Running),
            "stopped" => Some(WorkerState::Stopped),
            _ => None,
        }
    }
}

pub struct WorkerHeartbeat {
    stop: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
    path: PathBuf,
    temporary: PathBuf,
    worker: String,
    thread: Option<JoinHandle<()>>,
}

impl WorkerHeartbeat {
    pub fn start(root: &Path, worker: &str) -> Option<Self> {
        let dir = root.join("heartbeats");
        if let Err(error) = fs::create_dir_all(&dir) {
            eprintln!("worker {worker}: cannot create heartbeat directory: {error}");
            return None;
        }
        let process = [
            std::env::var("SLURM_JOB_ID").ok(),
            std::env::var("SLURM_ARRAY_TASK_ID").ok(),
            std::env::var("SLURM_PROCID").ok(),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("-");
        let process = if process.is_empty() {
            std::process::id().to_string()
        } else {
            process
        };
        let stem = format!("{}--{process}", sanitize(worker));
        let path = dir.join(format!("{stem}.{HEARTBEAT_EXTENSION}"));
        let temporary = dir.join(format!(".{stem}.tmp"));
        let worker = worker.to_string();
        let running = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        write_heartbeat(&path, &temporary, &worker, WorkerState::Starting);
        let (thread_stop, thread_running) = (stop.clone(), running.clone());
        let (thread_path, thread_temporary, thread_worker) =
            (path.clone(), temporary.clone(), worker.clone());
        let thread = std::thread::spawn(move || {
            while !thread_stop.load(Ordering::Relaxed) {
                std::thread::park_timeout(Duration::from_secs(HEARTBEAT_SECONDS));
                if thread_stop.load(Ordering::Relaxed) {
                    break;
                }
                let state = if thread_running.load(Ordering::Relaxed) {
                    WorkerState::Running
                } else {
                    WorkerState::Starting
                };
                write_heartbeat(&thread_path, &thread_temporary, &thread_worker, state);
            }
            write_heartbeat(
                &thread_path,
                &thread_temporary,
                &thread_worker,
                WorkerState::Stopped,
            );
        });
        Some(Self {
            stop,
            running,
            path,
            temporary,
            worker,
            thread: Some(thread),
        })
    }

    pub fn mark_running(&self) {
        self.running.store(true, Ordering::Relaxed);
        write_heartbeat(
            &self.path,
            &self.temporary,
            &self.worker,
            WorkerState::Running,
        );
    }
}

impl Drop for WorkerHeartbeat {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            let _ = thread.join();
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveWorker {
    pub worker: String,
    pub state: WorkerState,
    pub age: u64,
}

pub fn live_workers(root: &Path, now: u64) -> Vec<LiveWorker> {
    let mut newest: HashMap<String, (u64, LiveWorker)> = HashMap::new();
    for entry in fs::read_dir(root.join("heartbeats"))
        .into_iter()
        .flatten()
        .flatten()
    {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some(HEARTBEAT_EXTENSION) {
            continue;
        }
        let Ok(body) = fs::read_to_string(&path) else {
            continue;
        };
        let mut fields = body.trim().split('\t');
        let (Some(unix), Some(worker), Some(_pid), Some(state)) = (
            fields.next().and_then(|value| value.parse::<u64>().ok()),
            fields.next(),
            fields.next(),
            fields.next().and_then(WorkerState::parse),
        ) else {
            continue;
        };
        let age = now.saturating_sub(unix);
        if age > HEARTBEAT_STALE_SECONDS || state == WorkerState::Stopped {
            continue;
        }
        let live = LiveWorker {
            worker: worker.to_string(),
            state,
            age,
        };
        newest
            .entry(live.worker.clone())
            .and_modify(|(seen, current)| {
                if unix > *seen {
                    *seen = unix;
                    *current = live.clone();
                }
            })
            .or_insert((unix, live));
    }
    let mut workers: Vec<_> = newest.into_values().map(|(_, live)| live).collect();
    workers.sort_unstable_by(|a, b| a.worker.cmp(&b.worker));
    workers
}

fn sanitize(worker: &str) -> String {
    worker
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn write_heartbeat(path: &Path, temporary: &Path, worker: &str, state: WorkerState) {
    let body = format!(
        "{}\t{}\t{}\t{}\n",
        now_unix(),
        worker,
        std::process::id(),
        state.label()
    );
    if fs::write(temporary, body).is_ok() {
        let _ = fs::rename(temporary, path);
    }
}

pub fn work(
    root: &Path,
    worker: &str,
    runner: &mut dyn TrialRunner,
    policy: &dyn Policy,
    heartbeat: Option<WorkerHeartbeat>,
) -> WorkerStats {
    let mut manifest = Manifest::load(&root.join("manifest.json"));
    manifest.activate_clock(root);
    let heartbeat = heartbeat.or_else(|| WorkerHeartbeat::start(root, worker));
    if let Some(heartbeat) = &heartbeat {
        heartbeat.mark_running();
    }
    let queue = Arc::new(Queue::new(root));
    let events = match EventLog::open(root, worker) {
        Ok(events) => Some(Arc::new(events)),
        Err(error) => {
            eprintln!("worker {worker}: cannot open campaign event log: {error}");
            None
        }
    };
    let mut stats = WorkerStats::default();
    if let Some(events) = &events {
        events.record(&CampaignEvent::worker(
            "worker_start",
            worker,
            format!("pid={}", std::process::id()),
        ));
    }
    let mut seen: HashSet<String> = HashSet::new();
    let mut results = queue.results_since(&mut seen);
    let mut refreshed = Instant::now();
    let mut avoid: HashSet<String> = HashSet::new();
    loop {
        if policy.finished(&manifest, &results) {
            break;
        }
        if refreshed.elapsed().as_secs() >= REFRESH_SECONDS {
            results.extend(queue.results_since(&mut seen));
            refreshed = Instant::now();
        }

        let Some(Proposal { trial, .. }) =
            policy.propose(&manifest, &results, &avoid, worker, runner.cached_spacing())
        else {
            if policy.finished(&manifest, &results) {
                break;
            }
            stats.idle_polls += 1;
            avoid.clear();
            std::thread::sleep(std::time::Duration::from_secs(IDLE_POLL_SECONDS));
            results.extend(queue.results_since(&mut seen));
            refreshed = Instant::now();
            continue;
        };
        let scenario = manifest
            .scenarios
            .iter()
            .find(|s| s.id == trial.scenario)
            .expect("the policy proposed an unknown scenario")
            .clone();

        if !fits_before_deadline(&manifest, runner.estimate(&trial, &scenario)) {
            stats.stopped_for_deadline = true;
            break;
        }

        let lease = CLAIM_LEASE_SECONDS.max(runner.estimate(&trial, &scenario).ceil() as u64 + 900);
        if !queue.try_claim(&trial, worker, lease) {
            avoid.insert(trial.id);
            stats.claims_lost += 1;
            continue;
        }

        if let Some(events) = &events {
            events.record(&CampaignEvent::trial("trial_start", worker, &trial, None));
        }

        let high_water = Arc::new(AtomicUsize::new(
            queue.best_score(&trial.scenario).unwrap_or(0),
        ));
        let checkpoint: Checkpoint = {
            let (queue, high_water, events) = (queue.clone(), high_water.clone(), events.clone());
            let (scenario_id, trial_id) = (trial.scenario.clone(), trial.id.clone());
            let (trial, worker) = (trial.clone(), worker.to_string());
            Arc::new(move |serviced: usize, antennas: &[Point]| {
                if high_water.fetch_max(serviced, Ordering::Relaxed) < serviced
                    && queue
                        .record_candidate(&scenario_id, serviced, &trial_id, antennas)
                        .is_ok_and(|retained| retained)
                    && let Some(events) = &events
                {
                    events.record(&CampaignEvent::trial(
                        "checkpoint",
                        &worker,
                        &trial,
                        Some(serviced),
                    ));
                }
            })
        };

        let resume = trial
            .resume_from
            .as_deref()
            .and_then(|id| queue.load_solution(&trial.scenario, id));

        let started = Instant::now();
        let outcome = runner.run(&trial, &scenario, resume.as_deref(), &checkpoint);
        let seconds = started.elapsed().as_secs_f64();

        checkpoint(outcome.serviced, &outcome.antennas);
        let _ = queue.store_solution(&trial.scenario, &trial.id, &outcome.antennas);
        let result = TrialResult {
            trial,
            serviced: outcome.serviced,
            seconds,
            worker: worker.to_string(),
            completed_unix_ms: now_unix_ms(),
        };
        let completed = queue.complete(&result).is_ok();
        if let Some(events) = &events {
            let mut event = CampaignEvent::trial(
                if completed {
                    "trial_complete"
                } else {
                    "trial_record_failed"
                },
                worker,
                &result.trial,
                Some(result.serviced),
            );
            event.detail = Some(format!("wall={seconds:.3}s"));
            events.record(&event);
        }
        avoid.insert(result.trial.id.clone());
        seen.insert(result.trial.id.clone());
        results.push(result);
        stats.completed += 1;
    }
    if let Some(events) = &events {
        events.record(&CampaignEvent::worker(
            "worker_stop",
            worker,
            format!(
                "completed={} claims_lost={} skipped={} idle_polls={} deadline={}",
                stats.completed,
                stats.claims_lost,
                stats.skipped,
                stats.idle_polls,
                stats.stopped_for_deadline
            ),
        ));
    }
    stats
}

fn fits_before_deadline(manifest: &Manifest, estimate: f64) -> bool {
    if manifest.deadline_unix == 0 {
        return true;
    }
    let stop_at = manifest
        .deadline_unix
        .saturating_sub(manifest.reserve_seconds);
    now_unix() + estimate.ceil() as u64 <= stop_at
}

pub struct LnsRunner {
    instance: Instance,
    cached: Option<(f64, CoverageData)>,
    per_iteration: f64,
}

impl LnsRunner {
    pub fn new(instance: Instance) -> Self {
        LnsRunner {
            instance,
            cached: None,
            per_iteration: 1.0,
        }
    }

    fn coverage(&mut self, spacing: f64) -> &CoverageData {
        if self.cached.as_ref().is_none_or(|&(s, _)| s != spacing) {
            let config = crate::coverage::CoverageConfig {
                candidate_spacing: spacing,
                ..Default::default()
            };
            let data = build_coverage(&self.instance, &config);
            self.cached = Some((spacing, data));
        }
        &self.cached.as_ref().expect("just built").1
    }
}

impl TrialRunner for LnsRunner {
    fn run(
        &mut self,
        trial: &Trial,
        scenario: &Scenario,
        resume: Option<&[Point]>,
        checkpoint: &Checkpoint,
    ) -> Outcome {
        let options = trial
            .config
            .options(trial.seed, trial.remaining_iterations());
        let (k, tau) = (scenario.k, scenario.tau);
        let started = Instant::now();
        let data = self.coverage(trial.config.spacing);

        let checkpoint = checkpoint.clone();
        let report: Arc<dyn Fn(LnsProgress) + Send + Sync> = Arc::new(move |p: LnsProgress| {
            if !p.antennas.is_empty() {
                checkpoint(p.serviced, &p.antennas);
            }
        });
        let antennas = match resume {
            Some(points) if !points.is_empty() => {
                select_lns_antennas_resuming(data, k, tau, &options, points, Some(report))
            }
            _ => select_lns_antennas_with_progress(data, k, tau, &options, Some(report)),
        };

        let mut instance = self.instance.clone();
        instance.k = k;
        instance.tau = tau;
        let solution = framework::Solution {
            antennas: antennas.clone(),
        };
        let serviced = framework::evaluate(&instance, &solution).score;

        self.per_iteration = started.elapsed().as_secs_f64() / trial.remaining_iterations() as f64;
        Outcome { antennas, serviced }
    }

    fn estimate(&self, trial: &Trial, _scenario: &Scenario) -> f64 {
        let build = if self
            .cached
            .as_ref()
            .is_none_or(|&(s, _)| s != trial.config.spacing)
        {
            120.0
        } else {
            0.0
        };
        build + self.per_iteration * trial.iterations as f64 * trial.config.restarts.max(1) as f64
    }

    fn cached_spacing(&self) -> Option<f64> {
        self.cached.as_ref().map(|&(s, _)| s)
    }
}

pub struct SaRunner {
    instance: Instance,
    cached: Option<(f64, CoverageData)>,
    seed_cache: HashMap<String, Vec<Vec<usize>>>,
    chains: usize,
    build_seconds: f64,
}

impl SaRunner {
    pub fn new(instance: Instance, chains: usize) -> Self {
        SaRunner {
            instance,
            cached: None,
            seed_cache: HashMap::new(),
            chains: chains.max(1),
            build_seconds: 120.0,
        }
    }

    pub fn with_build_estimate(mut self, seconds: f64) -> Self {
        self.build_seconds = seconds.max(0.0);
        self
    }

    fn coverage(&mut self, spacing: f64) -> &CoverageData {
        if self.cached.as_ref().is_none_or(|&(s, _)| s != spacing) {
            let config = crate::coverage::CoverageConfig {
                candidate_spacing: spacing,
                ..Default::default()
            };
            self.cached = Some((spacing, build_coverage(&self.instance, &config)));
            self.seed_cache.clear();
        }
        &self.cached.as_ref().expect("just built").1
    }
}

impl TrialRunner for SaRunner {
    fn run(
        &mut self,
        trial: &Trial,
        scenario: &Scenario,
        resume: Option<&[Point]>,
        checkpoint: &Checkpoint,
    ) -> Outcome {
        let budget = Duration::from_secs(trial.remaining_iterations() as u64);
        let config = SimulatedAnnealingConfig {
            iterations: usize::MAX,
            proposal_samples: super::sa_probe::sa_proposal_samples(scenario.k),
            pair_probability: trial.config.sa_pair_probability,
            target_probability: trial.config.sa_target_probability,
            seed: Some(trial.seed),
            chains: self.chains,
            swap_interval: 0,
            ..Default::default()
        };
        let checkpoint = checkpoint.clone();
        let report = Arc::new(move |p: AnnealingProgress| {
            if !p.antennas.is_empty() {
                checkpoint(p.serviced, &p.antennas);
            }
        });
        self.coverage(trial.config.spacing);
        if resume.is_none() && !self.seed_cache.contains_key(&scenario.id) {
            let data = &self.cached.as_ref().expect("coverage was built").1;
            let seeds = annealing_seed_indices(data, scenario.k, scenario.tau);
            self.seed_cache.insert(scenario.id.clone(), seeds);
        }
        let seeds = resume
            .is_none()
            .then(|| self.seed_cache.get(&scenario.id))
            .flatten()
            .map(Vec::as_slice);
        let data = &self.cached.as_ref().expect("coverage was built").1;
        let deadline = Instant::now() + budget;
        let (antennas, serviced) = select_annealed_antennas_until_with_progress(
            data,
            scenario.k,
            scenario.tau,
            &config,
            AnnealingTimedOptions {
                resume,
                seeds,
                preferred_start: data.candidates.len(),
                deadline,
                progress: Some(report),
            },
        );
        Outcome { antennas, serviced }
    }

    fn estimate(&self, trial: &Trial, _scenario: &Scenario) -> f64 {
        let build = if self
            .cached
            .as_ref()
            .is_none_or(|&(s, _)| s != trial.config.spacing)
        {
            self.build_seconds
        } else {
            0.0
        };
        build + trial.remaining_iterations() as f64
    }

    fn cached_spacing(&self) -> Option<f64> {
        self.cached.as_ref().map(|&(s, _)| s)
    }
}

#[derive(Default)]
pub struct FakeRunner {
    pub seconds: f64,
}

impl TrialRunner for FakeRunner {
    fn run(
        &mut self,
        trial: &Trial,
        scenario: &Scenario,
        _resume: Option<&[Point]>,
        checkpoint: &Checkpoint,
    ) -> Outcome {
        if self.seconds > 0.0 {
            std::thread::sleep(std::time::Duration::from_secs_f64(self.seconds));
        }
        let c = &trial.config;
        let destroy_penalty = (c.destroy as f64 - 8.0).abs() * 12.0;
        let rcl_penalty = (c.rcl as f64 - 16.0).abs() * 6.0;
        let budget = (trial.iterations as f64).sqrt() * 40.0;
        let noise = (trial.seed % 17) as f64;
        let serviced =
            (scenario.k as f64 * 3.0 + budget + noise - destroy_penalty - rcl_penalty).max(0.0);
        let antennas = vec![Point::new(0.0, 0.0); scenario.k];
        checkpoint(serviced as usize / 2, &antennas);
        Outcome {
            antennas,
            serviced: serviced as usize,
        }
    }

    fn estimate(&self, _trial: &Trial, _scenario: &Scenario) -> f64 {
        self.seconds
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU32;

    use super::super::manifest::Manifest;
    use super::super::policy::BaselinePolicy;
    use super::*;

    fn campaign_dir(scenarios: Vec<Scenario>) -> std::path::PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let root = std::env::temp_dir().join(format!(
            "gisard-worker-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        Queue::new(&root).init().unwrap();
        Manifest {
            dataset: "unused.geojson".into(),
            scenarios,
            start_unix: 0,
            deadline_unix: 0,
            reserve_seconds: 0,
            duration_seconds: 0,
            spacings: vec![8.0],
            buildings: 0,
        }
        .save(&root.join("manifest.json"));
        root
    }

    fn heartbeat_dir() -> std::path::PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let root = std::env::temp_dir().join(format!(
            "gisard-heartbeat-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("heartbeats")).unwrap();
        root
    }

    fn stamp(root: &Path, file: &str, unix: u64, worker: &str, pid: u32, state: &str) {
        fs::write(
            root.join("heartbeats").join(file),
            format!("{unix}\t{worker}\t{pid}\t{state}\n"),
        )
        .unwrap();
    }

    #[test]
    fn live_workers_counts_workers_not_files() {
        let root = heartbeat_dir();
        let now = 10_000;
        stamp(&root, "w0--1.heartbeat", now - 5, "w0", 1, "running");
        stamp(&root, ".w1--2.tmp", now - 1, "w1", 2, "running");
        stamp(&root, "w1--2.heartbeat", now - 5, "w1", 2, "running");
        stamp(&root, "w2--3.heartbeat", now - 80, "w2", 3, "running");
        stamp(&root, "w2--4.heartbeat", now - 2, "w2", 4, "starting");
        stamp(&root, "w3--5.heartbeat", now - 600, "w3", 5, "running");
        stamp(&root, "w4--6.heartbeat", now - 1, "w4", 6, "stopped");

        let live = live_workers(&root, now);
        let ids: Vec<&str> = live.iter().map(|w| w.worker.as_str()).collect();
        assert_eq!(ids, ["w0", "w1", "w2"]);
        assert_eq!(
            live[2].state,
            WorkerState::Starting,
            "the newest stamp wins, not the first file read"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_heartbeat_reports_startup_then_running() {
        let root = heartbeat_dir();
        let heartbeat = WorkerHeartbeat::start(&root, "w0").expect("heartbeat");
        let live = live_workers(&root, now_unix());
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].state, WorkerState::Starting);

        heartbeat.mark_running();
        assert_eq!(
            live_workers(&root, now_unix())[0].state,
            WorkerState::Running
        );

        drop(heartbeat);
        assert!(
            live_workers(&root, now_unix()).is_empty(),
            "a clean exit must retire the worker immediately, not after the stale window"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn progress_reaches_the_candidate_pool_before_the_trial_ends() {
        let scenario = Scenario::new(0.5, 4);
        let root = campaign_dir(vec![scenario.clone()]);
        let policy = BaselinePolicy {
            max_trials: 1,
            ..Default::default()
        };
        let stats = work(&root, "w0", &mut FakeRunner::default(), &policy, None);
        assert_eq!(stats.completed, 1);

        let candidates: Vec<String> =
            std::fs::read_dir(root.join("best").join(&scenario.id).join("cand"))
                .unwrap()
                .flatten()
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect();
        assert_eq!(
            candidates.len(),
            2,
            "expected a mid-trial checkpoint and a final one, got {candidates:?}"
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn checkpoints_below_the_known_best_are_not_written() {
        let scenario = Scenario::new(0.5, 4);
        let root = campaign_dir(vec![scenario.clone()]);
        let queue = Queue::new(&root);
        queue
            .record_candidate(&scenario.id, 100_000, "earlier", &[Point::new(0.0, 0.0)])
            .unwrap();

        let policy = BaselinePolicy {
            max_trials: 1,
            ..Default::default()
        };
        work(&root, "w0", &mut FakeRunner::default(), &policy, None);

        let count = std::fs::read_dir(root.join("best").join(&scenario.id).join("cand"))
            .unwrap()
            .count();
        assert_eq!(count, 1, "only the pre-existing better candidate remains");
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn a_transient_lull_does_not_retire_a_worker() {
        struct Stubborn {
            calls: std::sync::atomic::AtomicUsize,
        }
        impl Policy for Stubborn {
            fn propose(
                &self,
                manifest: &Manifest,
                _results: &[TrialResult],
                _avoid: &HashSet<String>,
                _worker: &str,
                _cached: Option<f64>,
            ) -> Option<Proposal> {
                if self.calls.fetch_add(1, Ordering::Relaxed) == 0 {
                    return None;
                }
                Some(Proposal {
                    trial: Trial::new(
                        &manifest.scenarios[0].id,
                        super::super::trial::TrialConfig::default(),
                        0,
                        20,
                        7,
                    ),
                    priority: 0,
                })
            }
            fn finished(&self, _manifest: &Manifest, results: &[TrialResult]) -> bool {
                !results.is_empty()
            }
        }

        let scenario = Scenario::new(0.5, 4);
        let root = campaign_dir(vec![scenario]);
        let policy = Stubborn {
            calls: std::sync::atomic::AtomicUsize::new(0),
        };
        let stats = work(&root, "w0", &mut FakeRunner::default(), &policy, None);
        assert_eq!(stats.idle_polls, 1, "it waited rather than leaving");
        assert_eq!(stats.completed, 1, "and then did the work that appeared");
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn a_worker_keeps_working_through_the_early_phases() {
        use super::super::asha::AshaPolicy;

        let root = campaign_dir(vec![Scenario::new(0.25, 4), Scenario::new(0.5, 4)]);
        let mut manifest = Manifest::load(&root.join("manifest.json"));
        manifest.start_unix = now_unix() - 10;
        manifest.deadline_unix = now_unix() + 990;
        manifest.save(&root.join("manifest.json"));

        let policy = AshaPolicy {
            max_trials: 25,
            ..Default::default()
        };
        let stats = work(&root, "w0", &mut FakeRunner::default(), &policy, None);
        assert_eq!(
            stats.completed, 25,
            "the worker stopped early instead of working to its cap"
        );
        assert_eq!(stats.idle_polls, 0, "and never had to idle");
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn a_worker_stops_at_the_freeze() {
        use super::super::asha::AshaPolicy;

        let root = campaign_dir(vec![Scenario::new(0.5, 4)]);
        let mut manifest = Manifest::load(&root.join("manifest.json"));
        manifest.start_unix = now_unix() - 990;
        manifest.deadline_unix = now_unix() + 10;
        manifest.save(&root.join("manifest.json"));

        let stats = work(
            &root,
            "w0",
            &mut FakeRunner::default(),
            &AshaPolicy::default(),
            None,
        );
        assert_eq!(stats.completed, 0);
        assert_eq!(stats.idle_polls, 0, "the freeze must stop, not idle-poll");
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn a_worker_stops_when_the_deadline_leaves_no_room() {
        let scenario = Scenario::new(0.5, 4);
        let root = campaign_dir(vec![scenario]);
        let mut manifest = Manifest::load(&root.join("manifest.json"));
        manifest.deadline_unix = now_unix() + 10;
        manifest.reserve_seconds = 600;
        manifest.save(&root.join("manifest.json"));

        let stats = work(
            &root,
            "w0",
            &mut FakeRunner { seconds: 5.0 },
            &BaselinePolicy::default(),
            None,
        );
        assert_eq!(stats.completed, 0);
        assert!(
            stats.stopped_for_deadline,
            "must not start work it cannot finish"
        );
        std::fs::remove_dir_all(root).ok();
    }
}

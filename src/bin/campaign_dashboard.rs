use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use gisard::campaign::{
    CampaignEvent, EventReader, LiveWorker, Manifest, Queue, TimedResult, WorkerState,
    live_workers, now_unix, read_events,
};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{BarChart, Block, Borders, Cell, Paragraph, Row, Table, Wrap};
use ratatui::{DefaultTerminal, Frame};

const EVENT_TAIL: usize = 2_000;
const CLAIM_RECONCILE: Duration = Duration::from_secs(60);

#[derive(Clone)]
struct ActiveTrial {
    worker: String,
    scenario: String,
    rung: usize,
    spacing: f64,
}

#[derive(Clone, Default)]
struct SlurmState {
    running: usize,
    pending: usize,
    other: usize,
    expected: usize,
    polish: String,
    harvest: String,
    error: Option<String>,
    tasks: BTreeMap<String, String>,
}

#[derive(Clone)]
struct ScenarioState {
    id: String,
    best: Option<usize>,
    gain_hour: Option<usize>,
    completed: usize,
    completed_hour: usize,
    running: usize,
    wall_share: f64,
    score_age: Option<u64>,
    stages: String,
}

#[derive(Clone)]
struct DensityState {
    spacing: f64,
    completed: usize,
    running: usize,
    promoted: usize,
    best_sum: usize,
}

#[derive(Debug, PartialEq, Clone, Copy)]
struct Gain {
    ago: u64,
    delta: u64,
}

#[derive(Debug, PartialEq, Default)]
struct Gains {
    improvements: Vec<Gain>,
    total: u64,
    peak: u64,
    stale: Option<u64>,
}

fn gains(points: &[(u64, usize)], now: u64) -> Gains {
    let mut out = Gains::default();
    let mut previous: Option<usize> = None;
    for &(unix, score) in points {
        if let Some(before) = previous
            && score > before
        {
            let delta = (score - before) as u64;
            out.total += delta;
            out.peak = out.peak.max(delta);
            out.improvements.push(Gain {
                ago: now.saturating_sub(unix),
                delta,
            });
        }
        previous = Some(score);
    }
    out.stale = out.improvements.last().map(|gain| gain.ago);
    out.improvements.reverse();
    out
}

fn short_duration(seconds: u64) -> String {
    if seconds >= 86_400 {
        format!("{}d", seconds / 86_400)
    } else if seconds >= 3600 {
        format!("{}h", seconds / 3600)
    } else if seconds >= 60 {
        format!("{}m", seconds / 60)
    } else {
        format!("{seconds}s")
    }
}

fn phase_spans(elapsed: u64, remaining: u64, reserve: u64) -> Vec<(f64, &'static str)> {
    let search = elapsed.saturating_add(remaining);
    let total = search.saturating_add(reserve).max(1) as f64;
    let explore = search as f64 * gisard::campaign::EXPLORE_END;
    let polish_from = (gisard::campaign::SA_BUDGETS[0]
        + gisard::campaign::STARTUP_ALLOWANCE_SECONDS) as f64;
    let exploit_end = (search as f64 - polish_from).max(explore);
    vec![
        (explore / total, "explore"),
        ((exploit_end - explore) / total, "exploit"),
        ((search as f64 - exploit_end) / total, "polish"),
        (reserve as f64 / total, "reserve"),
    ]
}

#[derive(Clone)]
struct Snapshot {
    captured_unix: u64,
    fetch_ms: u128,
    elapsed: u64,
    remaining: u64,
    reserve: u64,
    completed: usize,
    running: usize,
    worker_ids: usize,
    live_workers: usize,
    starting_workers: usize,
    expected_workers: usize,
    recent_five: usize,
    recent_hour: usize,
    throughput: f64,
    event_count: usize,
    scenarios: Vec<ScenarioState>,
    densities: Vec<DensityState>,
    events: Vec<CampaignEvent>,
    histories: HashMap<String, Vec<(u64, usize)>>,
    alerts: Vec<String>,
    slurm: SlurmState,
}

struct Monitor {
    root: PathBuf,
    manifest: Manifest,
    queue: Queue,
    seen: HashSet<String>,
    timed: Vec<TimedResult>,
    event_reader: EventReader,
    events: VecDeque<CampaignEvent>,
    event_count: usize,
    histories: HashMap<String, Vec<(u64, usize)>>,
    active: HashMap<String, ActiveTrial>,
    last_claim_scan: Instant,
    seen_workers: BTreeMap<String, WorkerState>,
    seen_tasks: BTreeMap<String, String>,
    first_scan: bool,
    seed_scores: Vec<CampaignEvent>,
    expected_nodes: usize,
    expected_workers: usize,
}

impl Monitor {
    fn new(root: PathBuf) -> Self {
        let manifest = Manifest::load(&root.join("manifest.json"));
        let expected_nodes = expected_nodes(&root.join("sa-portfolio.tsv"));
        let expected_workers = expected_workers(&root.join("sa-portfolio.tsv"));
        let queue = Queue::new(&root);
        let mut event_reader = EventReader::default();
        event_reader.follow_from_end(&root);
        let seed_scores = read_events(&root);
        Monitor {
            queue,
            root,
            manifest,
            seen: HashSet::new(),
            timed: Vec::new(),
            event_reader,
            events: VecDeque::new(),
            event_count: 0,
            histories: HashMap::new(),
            active: HashMap::new(),
            last_claim_scan: Instant::now(),
            seen_workers: BTreeMap::new(),
            seen_tasks: BTreeMap::new(),
            first_scan: true,
            seed_scores,
            expected_nodes,
            expected_workers,
        }
    }

    fn collect(&mut self) -> Snapshot {
        let started = Instant::now();
        if let Some(clock) = self.manifest.clock(&self.root) {
            self.manifest.start_unix = clock.start_unix;
            self.manifest.deadline_unix = clock.deadline_unix;
        }
        let new_results = self.queue.timed_results_since(&mut self.seen);
        for timed in &new_results {
            self.active.remove(&timed.result.trial.id);
        }

        let new_events = self.event_reader.read_new(&self.root);
        self.event_count += new_events.len();
        for event in &new_events {
            match event.kind.as_str() {
                "trial_start" => {
                    if let (Some(trial_id), Some(scenario), Some(rung), Some(spacing)) = (
                        event.trial.clone(),
                        event.scenario.clone(),
                        event.rung,
                        event.spacing,
                    ) {
                        self.active.insert(
                            trial_id.clone(),
                            ActiveTrial {
                                worker: event.worker.clone(),
                                scenario,
                                rung,
                                spacing,
                            },
                        );
                    }
                }
                "trial_complete" | "trial_record_failed" => {
                    if let Some(trial_id) = &event.trial {
                        self.active.remove(trial_id);
                    }
                }
                "worker_stop" => {
                    self.active.retain(|_, trial| trial.worker != event.worker);
                }
                _ => {}
            }
            self.events.push_back(event.clone());
        }
        while self.events.len() > EVENT_TAIL {
            self.events.pop_front();
        }

        let seed = std::mem::take(&mut self.seed_scores);
        self.record_scores(score_points(&new_results, seed.iter().chain(&new_events)));
        self.timed.extend(new_results);
        self.timed
            .sort_unstable_by_key(|timed| timed.completed_unix);

        let active: Vec<_> = self.active.values().cloned().collect();
        let now = now_unix();
        let elapsed = now.saturating_sub(self.manifest.start_unix);
        let stop_at = self
            .manifest
            .deadline_unix
            .saturating_sub(self.manifest.reserve_seconds);
        let remaining = stop_at.saturating_sub(now);
        let trial_wall: f64 = self.timed.iter().map(|timed| timed.result.seconds).sum();
        let worker_ids = self
            .timed
            .iter()
            .map(|timed| timed.result.worker.as_str())
            .chain(active.iter().map(|trial| trial.worker.as_str()))
            .collect::<HashSet<&str>>()
            .len();
        let recent_five = completed_since(&self.timed, now.saturating_sub(300));
        let recent_hour = completed_since(&self.timed, now.saturating_sub(3600));
        let recent_window = elapsed.clamp(1, 3600);
        let throughput = recent_hour as f64 * 3600.0 / recent_window as f64;
        let live = live_workers(&self.root, now);
        let starting_workers = live
            .iter()
            .filter(|worker| worker.state == WorkerState::Starting)
            .count();
        let live_workers = live.len();

        let scenarios: Vec<ScenarioState> = self
            .manifest
            .scenarios
            .iter()
            .map(|scenario| {
                scenario_state(
                    &self.queue,
                    &self.timed,
                    &active,
                    &scenario.id,
                    now,
                    trial_wall,
                )
            })
            .collect();
        for scenario in &scenarios {
            if let Some(best) = scenario.best {
                self.histories
                    .entry(scenario.id.clone())
                    .or_insert_with(|| vec![(now, best)]);
            }
        }
        let densities = density_states(&self.timed, &active);
        let mut alerts = Vec::new();
        let claiming_workers: HashSet<&str> =
            active.iter().map(|trial| trial.worker.as_str()).collect();
        if claiming_workers.len() < active.len() {
            alerts.push(format!(
                "{} active claims share only {} worker ids",
                active.len(),
                claiming_workers.len()
            ));
        }
        let failures = self
            .events
            .iter()
            .filter(|event| event.kind.ends_with("_failed"))
            .count();
        if failures > 0 {
            alerts.push(format!("{failures} failed lifecycle events"));
        }
        let missing: Vec<_> = self
            .manifest
            .scenarios
            .iter()
            .filter(|scenario| self.queue.best_score(&scenario.id).is_none())
            .map(|scenario| scenario.id.as_str())
            .collect();
        if !missing.is_empty() {
            alerts.push(format!("not yet submittable: {}", missing.join(", ")));
        }

        let slurm = slurm_state(&launch_ids(&self.root.join("launch.env")), self.expected_nodes);
        self.note_lifecycle(&live, &slurm);
        if let Some(error) = &slurm.error {
            alerts.push(error.clone());
        }
        if slurm.expected > 0
            && slurm.running + slurm.pending + slurm.other < slurm.expected
            && remaining > 0
        {
            alerts.push(format!(
                "only {} of {} production nodes are visible in Slurm",
                slurm.running + slurm.pending + slurm.other,
                slurm.expected
            ));
        }
        if self.expected_workers > 0
            && elapsed > 120
            && remaining > 0
            && live_workers < self.expected_workers
        {
            alerts.push(format!(
                "only {live_workers} of {} worker processes have a fresh heartbeat{}",
                self.expected_workers,
                if starting_workers > 0 {
                    format!(" ({starting_workers} still loading)")
                } else {
                    String::new()
                }
            ));
        }
        Snapshot {
            captured_unix: now,
            fetch_ms: started.elapsed().as_millis(),
            elapsed,
            remaining,
            reserve: self.manifest.reserve_seconds,
            completed: self.timed.len(),
            running: active.len(),
            worker_ids,
            live_workers,
            starting_workers,
            expected_workers: self.expected_workers,
            recent_five,
            recent_hour,
            throughput,
            event_count: self.event_count,
            scenarios,
            densities,
            events: self.events.iter().cloned().collect(),
            histories: self.histories.clone(),
            alerts,
            slurm,
        }
    }

    fn note_lifecycle(&mut self, live: &[LiveWorker], slurm: &SlurmState) {
        let workers: BTreeMap<String, WorkerState> = live
            .iter()
            .map(|worker| (worker.worker.clone(), worker.state))
            .collect();
        for event in lifecycle_events(
            self.first_scan,
            &self.seen_workers,
            &workers,
            &self.seen_tasks,
            slurm,
        ) {
            self.event_count += 1;
            self.events.push_back(event);
        }
        self.seen_workers = workers;
        self.seen_tasks = slurm.tasks.clone();
        self.first_scan = false;
        while self.events.len() > EVENT_TAIL {
            self.events.pop_front();
        }
    }

    fn record_scores(&mut self, points: Vec<(u64, String, usize)>) {
        for (unix, scenario, score) in points {
            let history = self.histories.entry(scenario).or_default();
            if history.last().is_none_or(|(_, best)| score > *best) {
                history.push((unix, score));
            }
        }
    }

    fn reconcile_claims(&mut self) {
        self.active = active_trials(self.queue.running_claims());
        self.last_claim_scan = Instant::now();
    }
}

fn scenario_state(
    queue: &Queue,
    timed: &[TimedResult],
    active: &[ActiveTrial],
    id: &str,
    now: u64,
    trial_wall: f64,
) -> ScenarioState {
    let mine: Vec<_> = timed
        .iter()
        .filter(|timed| timed.result.trial.scenario == id)
        .collect();
    let running: Vec<_> = active.iter().filter(|trial| trial.scenario == id).collect();
    let best_update = queue.best_score_update(id);
    let best = best_update
        .map(|(score, _)| score)
        .or_else(|| mine.iter().map(|timed| timed.result.serviced).max());
    let first = mine.first().map(|timed| timed.result.serviced).unwrap_or(0);
    let before_hour = mine
        .iter()
        .filter(|timed| timed.completed_unix < now.saturating_sub(3600))
        .map(|timed| timed.result.serviced)
        .max()
        .unwrap_or(first);
    let gain_hour = if mine.is_empty() {
        None
    } else if best_update.is_some_and(|(_, updated)| updated >= now.saturating_sub(3600)) {
        Some(best.unwrap_or(before_hour).saturating_sub(before_hour))
    } else {
        Some(0)
    };
    let seconds: f64 = mine.iter().map(|timed| timed.result.seconds).sum();
    let mut stages: BTreeMap<usize, (usize, usize)> = BTreeMap::new();
    for timed in &mine {
        stages.entry(timed.result.trial.rung).or_default().0 += 1;
    }
    for trial in &running {
        stages.entry(trial.rung).or_default().1 += 1;
    }
    let stages = stages
        .into_iter()
        .map(|(rung, (done, active))| format!("r{rung} {done}+{active}"))
        .collect::<Vec<_>>()
        .join(" ");
    ScenarioState {
        id: id.to_string(),
        best,
        gain_hour,
        completed: mine.len(),
        completed_hour: mine
            .iter()
            .filter(|timed| timed.completed_unix >= now.saturating_sub(3600))
            .count(),
        running: running.len(),
        wall_share: if trial_wall > 0.0 {
            100.0 * seconds / trial_wall
        } else {
            0.0
        },
        score_age: best_update.map(|(_, updated)| now.saturating_sub(updated)),
        stages,
    }
}

fn density_states(timed: &[TimedResult], active: &[ActiveTrial]) -> Vec<DensityState> {
    let mut groups: BTreeMap<u64, DensityState> = BTreeMap::new();
    let mut best: HashMap<(u64, &str), usize> = HashMap::new();
    for timed in timed {
        let spacing = timed.result.trial.config.spacing;
        let bits = spacing.to_bits();
        let state = groups.entry(bits).or_insert(DensityState {
            spacing,
            completed: 0,
            running: 0,
            promoted: 0,
            best_sum: 0,
        });
        state.completed += 1;
        state.promoted += usize::from(timed.result.trial.rung > 0);
        best.entry((bits, timed.result.trial.scenario.as_str()))
            .and_modify(|score| *score = (*score).max(timed.result.serviced))
            .or_insert(timed.result.serviced);
    }
    for trial in active {
        let spacing = trial.spacing;
        groups
            .entry(spacing.to_bits())
            .or_insert(DensityState {
                spacing,
                completed: 0,
                running: 0,
                promoted: 0,
                best_sum: 0,
            })
            .running += 1;
    }
    for ((bits, _), score) in best {
        if let Some(state) = groups.get_mut(&bits) {
            state.best_sum += score;
        }
    }
    let mut states: Vec<_> = groups.into_values().collect();
    states.sort_by(|a, b| a.spacing.total_cmp(&b.spacing));
    states
}

fn active_trials(claims: Vec<gisard::campaign::RunningClaim>) -> HashMap<String, ActiveTrial> {
    claims
        .into_iter()
        .filter_map(|claim| {
            let trial = claim.trial?;
            Some((
                claim.trial_id.clone(),
                ActiveTrial {
                    worker: claim.worker,
                    scenario: trial.scenario,
                    rung: trial.rung,
                    spacing: trial.config.spacing,
                },
            ))
        })
        .collect()
}

fn completed_since(timed: &[TimedResult], cutoff: u64) -> usize {
    timed
        .iter()
        .filter(|timed| timed.completed_unix >= cutoff)
        .count()
}

#[derive(Clone, Default)]
struct LaunchIds {
    production: String,
    polish: String,
    harvest: String,
}

fn launch_ids(path: &Path) -> LaunchIds {
    let mut ids = LaunchIds::default();
    let Ok(body) = std::fs::read_to_string(path) else {
        return ids;
    };
    for line in body.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim_matches('\'').to_string();
        match key {
            "CAMPAIGN_JOB_IDS" => ids.production = value,
            "POLISH_JOB_ID" => ids.polish = value,
            "HARVEST_JOB_ID" => ids.harvest = value,
            _ => {}
        }
    }
    ids
}

fn expected_nodes(path: &Path) -> usize {
    std::fs::read_to_string(path)
        .ok()
        .map(|body| {
            body.lines()
                .skip(1)
                .filter_map(|line| line.split('\t').nth(2)?.parse::<usize>().ok())
                .sum()
        })
        .unwrap_or(0)
}

fn expected_workers(path: &Path) -> usize {
    fs::read_to_string(path)
        .ok()
        .map(|body| {
            body.lines()
                .skip(1)
                .filter_map(|line| {
                    let mut fields = line.split('\t');
                    let _tier = fields.next()?;
                    let _nominal = fields.next()?;
                    let nodes = fields.next()?.parse::<usize>().ok()?;
                    let workers = fields.next()?.parse::<usize>().ok()?;
                    Some(nodes * workers)
                })
                .sum()
        })
        .unwrap_or(0)
}

fn score_points<'a>(
    results: &[TimedResult],
    events: impl IntoIterator<Item = &'a CampaignEvent>,
) -> Vec<(u64, String, usize)> {
    let mut points: Vec<(u64, usize, String)> = results
        .iter()
        .map(|timed| {
            let unix_ms = match timed.result.completed_unix_ms {
                0 => timed.completed_unix * 1000,
                ms => ms,
            };
            (
                unix_ms,
                timed.result.serviced,
                timed.result.trial.scenario.clone(),
            )
        })
        .chain(
            events
                .into_iter()
                .filter(|event| event.kind == "checkpoint")
                .filter_map(|event| Some((event.unix_ms, event.score?, event.scenario.clone()?))),
        )
        .collect();
    points.sort_unstable_by_key(|(unix_ms, score, _)| (*unix_ms, *score));
    points
        .into_iter()
        .map(|(unix_ms, score, scenario)| (unix_ms / 1000, scenario, score))
        .collect()
}

fn lifecycle_events(
    first_scan: bool,
    seen_workers: &BTreeMap<String, WorkerState>,
    workers: &BTreeMap<String, WorkerState>,
    seen_tasks: &BTreeMap<String, String>,
    slurm: &SlurmState,
) -> Vec<CampaignEvent> {
    let mut out = Vec::new();
    let mut push = |kind: &str, worker: &str, detail: String| {
        out.push(CampaignEvent::worker(kind, worker, detail));
    };
    if first_scan {
        if !workers.is_empty() {
            let running = workers
                .values()
                .filter(|state| **state == WorkerState::Running)
                .count();
            push(
                "workers_live",
                "dashboard",
                format!(
                    "{} already up: {running} running, {} loading",
                    workers.len(),
                    workers.len() - running
                ),
            );
        }
        if !slurm.tasks.is_empty() {
            push(
                "jobs_live",
                "slurm",
                format!(
                    "{} array tasks already queued, {} running",
                    slurm.tasks.len(),
                    slurm.running
                ),
            );
        }
        return out;
    }
    for (task, status) in &slurm.tasks {
        match seen_tasks.get(task) {
            Some(before) if before == status => {}
            _ if matches!(status.as_str(), "RUNNING" | "COMPLETING") => {
                push("job_start", "slurm", format!("{task} {status}"));
            }
            None => push("job_queued", "slurm", format!("{task} {status}")),
            Some(_) => push("job_state", "slurm", format!("{task} {status}")),
        }
    }
    for task in seen_tasks.keys() {
        if !slurm.tasks.contains_key(task) {
            push("job_end", "slurm", format!("{task} left the queue"));
        }
    }
    for (worker, state) in workers {
        match seen_workers.get(worker) {
            Some(before) if before == state => {}
            _ if *state == WorkerState::Running => {
                push("worker_live", worker, "trial loop running".into());
            }
            _ => push("worker_live", worker, "loading dataset".into()),
        }
    }
    for worker in seen_workers.keys() {
        if !workers.contains_key(worker) {
            push("worker_lost", worker, "heartbeat went stale".into());
        }
    }
    out
}

struct SlurmJob<'a> {
    task: &'a str,
    job: &'a str,
    status: &'a str,
    start: &'a str,
    reason: &'a str,
    name: &'a str,
    reservation: &'a str,
}

const SLURM_FORMAT: &str = "%i|%F|%T|%S|%R|%j|%v";

fn parse_slurm_row(line: &str) -> Option<SlurmJob<'_>> {
    let mut fields = line.trim().splitn(7, '|');
    Some(SlurmJob {
        task: fields.next()?,
        job: fields.next()?,
        status: fields.next()?,
        start: fields.next()?,
        reason: fields.next()?,
        name: fields.next()?,
        reservation: fields.next()?,
    })
}

fn is_production(
    job: &SlurmJob<'_>,
    recorded: &HashSet<&str>,
    signature: Option<(&str, &str)>,
) -> bool {
    recorded.contains(job.job)
        || signature
            .is_some_and(|(name, reservation)| job.name == name && job.reservation == reservation)
}

fn slurm_state(launch: &LaunchIds, expected: usize) -> SlurmState {
    let mut state = SlurmState {
        expected,
        ..Default::default()
    };
    state.harvest = if launch.harvest.is_empty() {
        "not submitted".into()
    } else {
        "finished or absent".into()
    };
    state.polish = if launch.polish.is_empty() {
        "not submitted".into()
    } else {
        "finished or absent".into()
    };
    if launch.production.is_empty() && launch.polish.is_empty() && launch.harvest.is_empty() {
        return state;
    }
    let user = env::var("USER").unwrap_or_default();
    let output = match Command::new("squeue")
        .args(["-r", "-h", "-u", &user, "-o", SLURM_FORMAT])
        .output()
    {
        Ok(output) if output.status.success() => output,
        Ok(output) => {
            state.error = Some(String::from_utf8_lossy(&output.stderr).trim().to_string());
            return state;
        }
        Err(error) => {
            state.error = Some(format!("squeue: {error}"));
            return state;
        }
    };
    let body = String::from_utf8_lossy(&output.stdout);
    let rows: Vec<SlurmJob<'_>> = body.lines().filter_map(parse_slurm_row).collect();

    let recorded: HashSet<&str> = launch
        .production
        .split(',')
        .filter(|id| !id.is_empty())
        .collect();
    let signature = rows
        .iter()
        .find(|job| recorded.contains(job.job))
        .map(|job| (job.name, job.reservation));

    for job in &rows {
        if is_production(job, &recorded, signature) {
            state.tasks.insert(job.task.into(), job.status.into());
            match job.status {
                "RUNNING" | "COMPLETING" => state.running += 1,
                "PENDING" | "CONFIGURING" => state.pending += 1,
                _ => state.other += 1,
            }
        } else if job.job == launch.polish {
            state.polish = format!("{} {} {}", job.status, job.start, job.reason);
        } else if job.job == launch.harvest {
            state.harvest = format!("{} {} {}", job.status, job.start, job.reason);
        }
    }
    state
}

enum Control {
    Refresh,
    Stop,
}

struct App {
    root: PathBuf,
    snapshot: Option<Snapshot>,
    deltas: HashMap<String, isize>,
    selected: usize,
    event_scroll: usize,
    show_checkpoints: bool,
    density_focus: bool,
    paused: bool,
    status: String,
}

impl App {
    fn new(root: PathBuf) -> Self {
        App {
            root,
            snapshot: None,
            deltas: HashMap::new(),
            selected: 0,
            event_scroll: 0,
            show_checkpoints: false,
            density_focus: false,
            paused: false,
            status: "loading first snapshot…".into(),
        }
    }

    fn accept(&mut self, next: Snapshot) {
        if self.paused {
            return;
        }
        self.deltas.clear();
        if let Some(previous) = &self.snapshot {
            let old: HashMap<_, _> = previous
                .scenarios
                .iter()
                .map(|scenario| (scenario.id.as_str(), scenario.best))
                .collect();
            for scenario in &next.scenarios {
                if let (Some(before), Some(after)) = (
                    old.get(scenario.id.as_str()).copied().flatten(),
                    scenario.best,
                ) {
                    self.deltas
                        .insert(scenario.id.clone(), after as isize - before as isize);
                }
            }
        }
        self.status = format!("updated in {} ms", next.fetch_ms);
        self.selected = self.selected.min(next.scenarios.len().saturating_sub(1));
        self.snapshot = Some(next);
    }

    fn draw(&self, frame: &mut Frame<'_>) {
        let area = frame.area();
        let compact = area.height < 32;
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(if compact {
                [
                    Constraint::Length(3),
                    Constraint::Length(4),
                    Constraint::Length(4),
                    Constraint::Length(9),
                    Constraint::Length(8),
                    Constraint::Min(3),
                    Constraint::Length(1),
                ]
            } else {
                [
                    Constraint::Length(3),
                    Constraint::Length(4),
                    Constraint::Length(4),
                    Constraint::Length(13),
                    Constraint::Length(12),
                    Constraint::Min(4),
                    Constraint::Length(1),
                ]
            })
            .split(area);
        self.draw_header(frame, chunks[0]);
        self.draw_summary(frame, chunks[1]);
        self.draw_phases(frame, chunks[2]);
        if area.width >= 120 {
            let main = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(72), Constraint::Percentage(28)])
                .split(chunks[3]);
            self.draw_scenarios(frame, main[0]);
            self.draw_densities(frame, main[1]);
        } else if self.density_focus {
            self.draw_densities(frame, chunks[3]);
        } else {
            self.draw_scenarios(frame, chunks[3]);
        }
        self.draw_gains(frame, chunks[4]);
        self.draw_events(frame, chunks[5]);
        self.draw_footer(frame, chunks[6]);
    }

    fn draw_header(&self, frame: &mut Frame<'_>, area: Rect) {
        let title = self
            .root
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("campaign");
        let text = if let Some(snapshot) = &self.snapshot {
            Line::from(vec![
                Span::styled("GISard  ", Style::default().fg(Color::Cyan).bold()),
                Span::styled(title, Style::default().bold()),
                Span::raw(format!(
                    "   snapshot {} ago   {}",
                    human_duration(now_unix().saturating_sub(snapshot.captured_unix)),
                    self.status
                )),
            ])
        } else {
            Line::from(format!("GISard  {title}  {}", self.status))
        };
        frame.render_widget(
            Paragraph::new(text).block(Block::default().borders(Borders::ALL).title(" Campaign ")),
            area,
        );
    }

    fn draw_summary(&self, frame: &mut Frame<'_>, area: Rect) {
        let Some(snapshot) = &self.snapshot else {
            frame.render_widget(Paragraph::new("waiting for filesystem and Slurm…"), area);
            return;
        };
        let alert = snapshot
            .alerts
            .first()
            .map_or("healthy".to_string(), |alert| format!("ALERT: {alert}"));
        let style = if snapshot.alerts.is_empty() {
            Style::default().fg(Color::Green)
        } else {
            Style::default().fg(Color::Red).bold()
        };
        let starting = if snapshot.starting_workers > 0 {
            format!(" ({} loading)", snapshot.starting_workers)
        } else {
            String::new()
        };
        let line = Line::from(format!(
            "nodes {}/{} run, {} queued  |  workers {}/{} live{starting}  |  trials {} done, {} active, {} ids  |  {:.1}/h  |  5m {}  1h {}",
            snapshot.slurm.running,
            snapshot.slurm.expected,
            snapshot.slurm.pending,
            snapshot.live_workers,
            snapshot.expected_workers,
            snapshot.completed,
            snapshot.running,
            snapshot.worker_ids,
            snapshot.throughput,
            snapshot.recent_five,
            snapshot.recent_hour,
        ));
        let detail = Line::from(vec![
            Span::raw(format!(
                "elapsed {}  |  search stops in {}  |  reserve {}  |  polish {}  |  harvest {}  |  events {}  |  ",
                human_duration(snapshot.elapsed),
                human_duration(snapshot.remaining),
                human_duration(snapshot.reserve),
                snapshot.slurm.polish,
                snapshot.slurm.harvest,
                snapshot.event_count,
            )),
            Span::styled(alert, style),
        ]);
        frame.render_widget(
            Paragraph::new(vec![line, detail]).block(Block::default().borders(Borders::ALL)),
            area,
        );
    }

    fn draw_scenarios(&self, frame: &mut Frame<'_>, area: Rect) {
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        let capacity = area.height.saturating_sub(3) as usize;
        let start = self
            .selected
            .saturating_add(1)
            .saturating_sub(capacity.max(1));
        let visible = snapshot
            .scenarios
            .iter()
            .enumerate()
            .skip(start)
            .take(capacity);
        let selected = self.selected;
        let compact = area.width < 110;
        let rows: Vec<Row<'_>> = visible
            .map(|(index, scenario)| {
                let style = if index == self.selected {
                    Style::default().bg(Color::DarkGray).fg(Color::White)
                } else {
                    Style::default()
                };
                let mut cells = vec![
                    Cell::from(scenario.id.clone()),
                    Cell::from(display_score(scenario.best)),
                    Cell::from(display_delta(self.deltas.get(&scenario.id).copied())),
                    Cell::from(
                        scenario
                            .gain_hour
                            .map_or("-".into(), |gain| format!("+{gain}")),
                    ),
                    Cell::from(scenario.completed.to_string()),
                    Cell::from(scenario.running.to_string()),
                    Cell::from(scenario.score_age.map_or("never".into(), human_duration)),
                    Cell::from(scenario.stages.clone()),
                ];
                if !compact {
                    cells.insert(5, Cell::from(scenario.completed_hour.to_string()));
                    cells.insert(7, Cell::from(format!("{:.1}", scenario.wall_share)));
                }
                Row::new(cells).style(style)
            })
            .collect();
        let header = if compact {
            Row::new([
                "scenario",
                "best",
                "Δrefresh",
                "Δ1h",
                "done",
                "active",
                "score age",
                "stages done+active",
            ])
        } else {
            Row::new([
                "scenario",
                "best",
                "Δrefresh",
                "Δ1h",
                "done",
                "1h",
                "active",
                "wall%",
                "score age",
                "stages done+active",
            ])
        }
        .style(Style::default().fg(Color::Cyan).bold());
        let narrow_widths = [
            Constraint::Length(12),
            Constraint::Length(7),
            Constraint::Length(9),
            Constraint::Length(6),
            Constraint::Length(5),
            Constraint::Length(6),
            Constraint::Length(9),
            Constraint::Min(14),
        ];
        let wide_widths = [
            Constraint::Length(12),
            Constraint::Length(7),
            Constraint::Length(9),
            Constraint::Length(6),
            Constraint::Length(5),
            Constraint::Length(4),
            Constraint::Length(6),
            Constraint::Length(6),
            Constraint::Length(9),
            Constraint::Min(14),
        ];
        let table = if compact {
            Table::new(rows, narrow_widths.as_slice())
        } else {
            Table::new(rows, wide_widths.as_slice())
        }
        .header(header)
        .column_spacing(1)
        .block(Block::default().borders(Borders::ALL).title(format!(
            " Scenarios ({}/{}) ",
            selected.saturating_add(1),
            snapshot.scenarios.len()
        )));
        frame.render_widget(table, area);
    }

    fn draw_densities(&self, frame: &mut Frame<'_>, area: Rect) {
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        let header = Row::new(["spacing", "done", "run", "prom", "Σbest"])
            .style(Style::default().fg(Color::Cyan).bold());
        let rows = snapshot.densities.iter().map(|density| {
            Row::new([
                format!("{:.5}", density.spacing),
                density.completed.to_string(),
                density.running.to_string(),
                density.promoted.to_string(),
                density.best_sum.to_string(),
            ])
        });
        let table = Table::new(
            rows,
            [
                Constraint::Length(10),
                Constraint::Length(5),
                Constraint::Length(4),
                Constraint::Length(5),
                Constraint::Min(6),
            ],
        )
        .header(header)
        .column_spacing(1)
        .block(Block::default().borders(Borders::ALL).title(" Densities "));
        frame.render_widget(table, area);
    }

    fn draw_gains(&self, frame: &mut Frame<'_>, area: Rect) {
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        let Some(scenario) = snapshot.scenarios.get(self.selected) else {
            return;
        };
        let points = snapshot
            .histories
            .get(&scenario.id)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let g = gains(points, snapshot.captured_unix);

        let split = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(40), Constraint::Length(26)])
            .split(area);

        let stale = match g.stale {
            Some(seconds) => format!("last {} ago", human_duration(seconds)),
            None => "no gain yet".to_string(),
        };
        let steps = g.improvements.len();
        let title = format!(
            " Gains: {}  best {}  +{} over {} step{}  biggest +{}  {} ",
            scenario.id,
            scenario.best.unwrap_or(0),
            g.total,
            steps,
            if steps == 1 { "" } else { "s" },
            g.peak,
            stale,
        );

        if g.improvements.is_empty() {
            frame.render_widget(
                Paragraph::new("no improvements retained yet")
                    .block(Block::default().borders(Borders::ALL).title(title)),
                split[0],
            );
        } else {
            let inner = split[0].width.saturating_sub(2).max(1) as usize;
            let fit = (inner / 5).max(1);
            let shown = g.improvements.len().min(fit);
            let bar_width = (inner.saturating_sub(shown - 1) / shown).max(1);
            let mut window: Vec<Gain> = g.improvements.iter().copied().take(shown).collect();
            window.reverse();
            let mut previous = String::new();
            let labels: Vec<String> = window
                .iter()
                .map(|gain| {
                    let text = short_duration(gain.ago);
                    if text == previous {
                        String::new()
                    } else {
                        previous = text.clone();
                        text
                    }
                })
                .collect();
            let data: Vec<(&str, u64)> = window
                .iter()
                .zip(&labels)
                .map(|(gain, label)| (label.as_str(), gain.delta))
                .collect();
            let footer = if shown < g.improvements.len() {
                format!(" newest {shown} of {} improvements, oldest left ", g.improvements.len())
            } else {
                " oldest left, newest right; labels are age ".to_string()
            };
            frame.render_widget(
                BarChart::default()
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(title)
                            .title_bottom(footer),
                    )
                    .data(&data)
                    .bar_width(bar_width as u16)
                    .bar_gap(1)
                    .bar_style(Style::default().fg(Color::Green))
                    .value_style(Style::default().fg(Color::Black).bg(Color::Green))
                    .label_style(Style::default().fg(Color::DarkGray)),
                split[0],
            );
        }

        let mut rows: Vec<Line> = Vec::new();
        if g.improvements.is_empty() {
            rows.push(Line::from(Span::styled(
                "  no improvements yet",
                Style::default().fg(Color::DarkGray),
            )));
        }
        for gain in g
            .improvements
            .iter()
            .take(split[1].height.saturating_sub(2) as usize)
        {
            rows.push(Line::from(vec![
                Span::styled(
                    format!("  {:>+5}", gain.delta as i64),
                    Style::default().fg(Color::Green).bold(),
                ),
                Span::styled(
                    format!("   {:>7} ago", human_duration(gain.ago)),
                    Style::default().fg(Color::Gray),
                ),
            ]));
        }
        frame.render_widget(
            Paragraph::new(rows).block(Block::default().borders(Borders::ALL).title(" Recent ")),
            split[1],
        );
    }

    fn draw_phases(&self, frame: &mut Frame<'_>, area: Rect) {
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        let spans = phase_spans(snapshot.elapsed, snapshot.remaining, snapshot.reserve);
        let total = snapshot.elapsed + snapshot.remaining + snapshot.reserve;
        let width = area.width.saturating_sub(2) as usize;
        let done = if total == 0 {
            0
        } else {
            (width as f64 * snapshot.elapsed as f64 / total as f64).round() as usize
        };
        let colours = [Color::Cyan, Color::Green, Color::Yellow, Color::Magenta];

        let mut widths = Vec::with_capacity(spans.len());
        let mut drawn = 0usize;
        for (i, (fraction, _)) in spans.iter().enumerate() {
            let cells = if i + 1 == spans.len() {
                width.saturating_sub(drawn)
            } else {
                ((width as f64 * fraction).round() as usize).min(width - drawn)
            };
            widths.push(cells);
            drawn += cells;
        }

        let mut bar: Vec<Span> = Vec::new();
        let mut labels: Vec<Span> = Vec::new();
        let mut cursor = 0usize;
        for (i, ((_, name), cells)) in spans.iter().zip(&widths).enumerate() {
            let colour = Style::default().fg(colours[i]);
            for c in cursor..cursor + cells {
                bar.push(if c < done {
                    Span::styled("█", colour)
                } else if c == done {
                    Span::styled("▌", Style::default().fg(Color::White).bold())
                } else {
                    Span::styled("░", colour)
                });
            }
            let text = if *cells > name.len() {
                format!("{name:<width$}", width = cells)
            } else {
                " ".repeat(*cells)
            };
            let style = if cursor <= done && done < cursor + cells {
                Style::default().fg(colours[i]).bold()
            } else {
                Style::default().fg(Color::DarkGray)
            };
            labels.push(Span::styled(text, style));
            cursor += cells;
        }

        let title = format!(
            " Clock: {} elapsed, search stops in {}, then {} reserve ",
            human_duration(snapshot.elapsed),
            human_duration(snapshot.remaining),
            human_duration(snapshot.reserve),
        );
        frame.render_widget(
            Paragraph::new(vec![Line::from(bar), Line::from(labels)])
                .block(Block::default().borders(Borders::ALL).title(title)),
            area,
        );
    }

    fn draw_events(&self, frame: &mut Frame<'_>, area: Rect) {
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        let filtered: Vec<_> = snapshot
            .events
            .iter()
            .filter(|event| self.show_checkpoints || event.kind != "checkpoint")
            .collect();
        let height = area.height.saturating_sub(2) as usize;
        let end = filtered.len().saturating_sub(self.event_scroll);
        let start = end.saturating_sub(height);
        let lines: Vec<_> = filtered[start..end]
            .iter()
            .map(|event| event_line(event, snapshot.captured_unix))
            .collect();
        let title = format!(
            " Events: {} since open{} ",
            snapshot.event_count,
            if self.show_checkpoints {
                ""
            } else {
                ", checkpoints hidden"
            }
        );
        frame.render_widget(
            Paragraph::new(lines)
                .block(Block::default().borders(Borders::ALL).title(title))
                .wrap(Wrap { trim: true }),
            area,
        );
    }

    fn draw_footer(&self, frame: &mut Frame<'_>, area: Rect) {
        let text = if self.paused {
            "PAUSED  q quit  p resume  r refresh  c checkpoints  d scenarios/densities  ↑/↓ select  PgUp/PgDn events"
        } else {
            "q quit  p pause  r refresh  c checkpoints  d scenarios/densities  ↑/↓ select  |  rung: 15m → 1h → 4h"
        };
        frame.render_widget(
            Paragraph::new(text).style(Style::default().fg(Color::DarkGray)),
            area,
        );
    }
}

fn event_line(event: &&CampaignEvent, now: u64) -> Line<'static> {
    let age = human_duration(now.saturating_sub(event.unix_ms / 1000));
    let scenario = event.scenario.as_deref().unwrap_or("-");
    let score = event
        .score
        .map_or(String::new(), |score| format!(" score {score}"));
    let stage = event.rung.map_or(String::new(), |rung| format!(" r{rung}"));
    let detail = event
        .detail
        .as_deref()
        .map_or(String::new(), |detail| format!(" {detail}"));
    Line::from(vec![
        Span::styled(format!("{age:>8} "), Style::default().fg(Color::DarkGray)),
        Span::styled(format!("{:<16}", event.kind), event_style(&event.kind)),
        Span::raw(format!(
            " {scenario:<12} {:<18}{stage}{score}{detail}",
            event.worker
        )),
    ])
}

fn event_style(kind: &str) -> Style {
    match kind {
        "checkpoint" => Style::default().fg(Color::Green),
        "trial_record_failed" => Style::default().fg(Color::Red).bold(),
        "worker_lost" | "job_end" => Style::default().fg(Color::Red),
        "trial_start" => Style::default().fg(Color::Blue),
        "job_start" | "worker_live" | "workers_live" | "jobs_live" => {
            Style::default().fg(Color::Yellow).bold()
        }
        "job_queued" | "job_state" => Style::default().fg(Color::Yellow),
        "worker_start" | "worker_stop" => Style::default().fg(Color::Cyan),
        _ => Style::default(),
    }
}

fn display_score(score: Option<usize>) -> String {
    score.map_or("-".into(), |score| score.to_string())
}

fn display_delta(delta: Option<isize>) -> String {
    delta.map_or("-".into(), |delta| format!("{delta:+}"))
}

fn human_duration(seconds: u64) -> String {
    let hours = seconds / 3600;
    let minutes = seconds % 3600 / 60;
    let seconds = seconds % 60;
    if hours > 0 {
        format!("{hours}h{minutes:02}m")
    } else if minutes > 0 {
        format!("{minutes}m{seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

fn monitor_thread(root: PathBuf, interval: Duration, tx: Sender<Snapshot>, rx: Receiver<Control>) {
    let mut monitor = Monitor::new(root);
    if tx.send(monitor.collect()).is_err() {
        return;
    }
    monitor.reconcile_claims();
    loop {
        if tx.send(monitor.collect()).is_err() {
            break;
        }
        match rx.recv_timeout(interval) {
            Ok(Control::Refresh) | Err(mpsc::RecvTimeoutError::Timeout) => {
                if monitor.last_claim_scan.elapsed() >= CLAIM_RECONCILE {
                    monitor.reconcile_claims();
                }
            }
            Ok(Control::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn run(terminal: &mut DefaultTerminal, root: PathBuf, interval: Duration) -> io::Result<()> {
    let (snapshot_tx, snapshot_rx) = mpsc::channel();
    let (control_tx, control_rx) = mpsc::channel();
    let monitor_root = root.clone();
    let handle =
        thread::spawn(move || monitor_thread(monitor_root, interval, snapshot_tx, control_rx));
    let mut app = App::new(root);
    let mut quit = false;
    while !quit {
        while let Ok(snapshot) = snapshot_rx.try_recv() {
            app.accept(snapshot);
        }
        terminal.draw(|frame| app.draw(frame))?;
        if event::poll(Duration::from_millis(200))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => quit = true,
                KeyCode::Char('r') => {
                    let _ = control_tx.send(Control::Refresh);
                    app.status = "refresh requested…".into();
                }
                KeyCode::Char('p') => {
                    app.paused = !app.paused;
                    app.status = if app.paused {
                        "paused".into()
                    } else {
                        "resumed; refresh requested…".into()
                    };
                    if !app.paused {
                        let _ = control_tx.send(Control::Refresh);
                    }
                }
                KeyCode::Char('c') => app.show_checkpoints = !app.show_checkpoints,
                KeyCode::Char('d') => app.density_focus = !app.density_focus,
                KeyCode::Down | KeyCode::Char('j') => {
                    let count = app
                        .snapshot
                        .as_ref()
                        .map_or(0, |snapshot| snapshot.scenarios.len());
                    app.selected = (app.selected + 1).min(count.saturating_sub(1));
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    app.selected = app.selected.saturating_sub(1);
                }
                KeyCode::PageUp => app.event_scroll = app.event_scroll.saturating_add(10),
                KeyCode::PageDown => app.event_scroll = app.event_scroll.saturating_sub(10),
                _ => {}
            }
        }
    }
    let _ = control_tx.send(Control::Stop);
    let _ = handle.join();
    Ok(())
}

fn usage() -> ! {
    eprintln!("usage: campaign_dashboard <campaign-dir> [--refresh <seconds>]");
    std::process::exit(2);
}

fn main() -> io::Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    let Some(root) = args.first() else {
        usage();
    };
    let mut refresh = 10u64;
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--refresh" => {
                refresh = args
                    .get(index + 1)
                    .unwrap_or_else(|| usage())
                    .parse()
                    .expect("invalid refresh interval");
                assert!(refresh > 0, "refresh interval must be positive");
                index += 2;
            }
            _ => usage(),
        }
    }
    let root = PathBuf::from(root);
    assert!(
        root.join("manifest.json").is_file(),
        "campaign manifest is missing"
    );
    ratatui::run(|terminal| run(terminal, root, Duration::from_secs(refresh)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    #[test]
    fn gains_list_improvements_newest_first_and_ignore_the_opening_score() {
        let g = gains(&[(100, 1000), (150, 1010), (380, 1013), (400, 1013)], 500);
        assert_eq!(g.total, 13, "1000 is the baseline, not a gain of 1000");
        assert_eq!(g.peak, 10, "the biggest single step, not a bucket sum");
        assert_eq!(
            g.improvements,
            vec![Gain { ago: 120, delta: 3 }, Gain { ago: 350, delta: 10 }]
        );
        assert_eq!(g.stale, Some(120));
    }

    #[test]
    fn a_scenario_that_never_improved_is_reported_as_such() {
        let g = gains(&[(100, 42)], 200);
        assert_eq!(g.total, 0);
        assert_eq!(g.peak, 0);
        assert_eq!(g.stale, None);
        assert!(g.improvements.is_empty());
        assert!(gains(&[], 200).improvements.is_empty());
    }

    #[test]
    fn short_durations_fit_under_a_bar() {
        for seconds in [0, 59, 60, 3599, 3600, 86_399, 86_400, 9_999_999] {
            let text = short_duration(seconds);
            assert!(text.len() <= 4, "{seconds}s rendered as {text:?}");
        }
        assert_eq!(short_duration(45), "45s");
        assert_eq!(short_duration(600), "10m");
        assert_eq!(short_duration(7200), "2h");
        assert_eq!(short_duration(180_000), "2d");
    }

    const SQUEUE_SAMPLE: &str = "\
40142824|40142824|PENDING|N/A|(Dependency)|gisard-harvest|gis-cup-2026
40142722|40142722|PENDING|N/A|(Dependency)|gisard-polish|gis-cup-2026
40142721_0|40142721|RUNNING|2026-08-15T00:10:35|cstd01-080|gisard-sa-asha|gis-cup-2026
40142721_1|40142721|RUNNING|2026-08-15T00:10:35|cstd01-081|gisard-sa-asha|gis-cup-2026
40142923_200|40142923|RUNNING|2026-08-15T00:47:11|cstd02-044|gisard-sa-asha|gis-cup-2026
40142931_206|40142931|RUNNING|2026-08-15T01:40:53|cstd01-087|gisard-sa-asha|gis-cup-2026
40142931_207|40142931|PENDING|N/A|(Resources)|gisard-sa-asha|gis-cup-2026
40199999_0|40199999|RUNNING|2026-08-15T01:00:00|cstd03-001|someone-else|(null)";

    fn classify(recorded: &str) -> (usize, usize, usize) {
        let rows: Vec<SlurmJob<'_>> = SQUEUE_SAMPLE.lines().filter_map(parse_slurm_row).collect();
        let recorded: HashSet<&str> = recorded.split(',').filter(|id| !id.is_empty()).collect();
        let signature = rows
            .iter()
            .find(|job| recorded.contains(job.job))
            .map(|job| (job.name, job.reservation));
        let (mut running, mut pending, mut other) = (0, 0, 0);
        for job in &rows {
            if is_production(job, &recorded, signature) {
                match job.status {
                    "RUNNING" | "COMPLETING" => running += 1,
                    "PENDING" | "CONFIGURING" => pending += 1,
                    _ => other += 1,
                }
            }
        }
        (running, pending, other)
    }

    #[test]
    fn production_jobs_submitted_after_launch_are_still_counted() {
        assert_eq!(classify("40142721"), (4, 1, 0));
        assert_eq!(classify("40142721,40142923,40142931"), (4, 1, 0));
    }

    #[test]
    fn discovery_does_not_capture_unrelated_jobs() {
        let (running, pending, other) = classify("40142721");
        assert_eq!(
            running + pending + other,
            5,
            "5 production tasks; polish, harvest and the stranger are not production"
        );
    }

    #[test]
    fn no_recorded_ids_means_no_signature() {
        assert_eq!(classify(""), (0, 0, 0));
    }

    fn checkpoint(unix: u64, scenario: &str, score: usize) -> CampaignEvent {
        CampaignEvent {
            unix_ms: unix * 1000,
            kind: "checkpoint".into(),
            worker: "w".into(),
            trial: None,
            scenario: Some(scenario.into()),
            score: Some(score),
            rung: None,
            spacing: None,
            detail: None,
        }
    }

    #[test]
    fn score_points_merge_both_sources_in_time_order() {
        let events = vec![
            checkpoint(30, "a", 105),
            checkpoint(70, "a", 115),
            checkpoint(50, "b", 900),
            CampaignEvent {
                score: None,
                ..checkpoint(60, "a", 0)
            },
            CampaignEvent {
                kind: "trial_start".into(),
                ..checkpoint(65, "a", 999)
            },
        ];
        let points = score_points(&[], &events);
        assert_eq!(
            points,
            vec![
                (30, "a".to_string(), 105),
                (50, "b".to_string(), 900),
                (70, "a".to_string(), 115),
            ],
            "oldest first, across scenarios, checkpoints only"
        );
    }

    #[test]
    fn score_points_are_deterministic_when_timestamps_collide() {
        let mut events = vec![
            checkpoint(0, "a", 130),
            checkpoint(0, "a", 110),
            checkpoint(0, "a", 120),
        ];
        events[0].unix_ms = 5_000;
        events[1].unix_ms = 5_400;
        events[2].unix_ms = 5_400;
        let forward = score_points(&[], &events);
        events.reverse();
        let reversed = score_points(&[], &events);
        assert_eq!(forward, reversed, "input order must not change the result");
        assert_eq!(
            forward,
            vec![
                (5, "a".to_string(), 130),
                (5, "a".to_string(), 110),
                (5, "a".to_string(), 120),
            ],
            "millisecond order first, then score for the exact tie"
        );
    }

    #[test]
    fn a_checkpoint_between_two_results_survives_the_running_max() {
        let points = {
            let mut merged = vec![
                (10, "a".to_string(), 100),
                (50, "a".to_string(), 130),
                (30, "a".to_string(), 120),
                (40, "a".to_string(), 110),
            ];
            merged.sort_unstable_by_key(|(unix, _, _)| *unix);
            merged
        };
        let mut history: Vec<(u64, usize)> = Vec::new();
        for (unix, _, score) in points {
            if history.last().is_none_or(|(_, best)| score > *best) {
                history.push((unix, score));
            }
        }
        assert_eq!(
            history,
            vec![(10, 100), (30, 120), (50, 130)],
            "the 30s improvement must survive; the 40s regression must not"
        );
        let g = gains(&history, 60);
        assert_eq!(g.total, 30, "100 is the baseline; +20 then +10");
        assert_eq!(g.improvements.len(), 2);
    }

    fn tasks(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(task, status)| (task.to_string(), status.to_string()))
            .collect()
    }

    fn kinds(events: &[CampaignEvent]) -> Vec<(&str, &str)> {
        events
            .iter()
            .map(|event| (event.kind.as_str(), event.detail.as_deref().unwrap_or("")))
            .collect()
    }

    #[test]
    fn the_first_lifecycle_scan_summarises() {
        let workers = BTreeMap::from([
            ("w0".to_string(), WorkerState::Running),
            ("w1".to_string(), WorkerState::Starting),
        ]);
        let slurm = SlurmState {
            running: 1,
            tasks: tasks(&[("9_0", "RUNNING"), ("9_1", "PENDING")]),
            ..Default::default()
        };
        let events = lifecycle_events(true, &BTreeMap::new(), &workers, &BTreeMap::new(), &slurm);
        assert_eq!(
            kinds(&events),
            [
                ("workers_live", "2 already up: 1 running, 1 loading"),
                ("jobs_live", "2 array tasks already queued, 1 running"),
            ]
        );
    }

    #[test]
    fn starts_are_logged_once_and_losses_are_logged() {
        let before_tasks = tasks(&[("9_0", "PENDING"), ("9_1", "RUNNING")]);
        let before_workers = BTreeMap::from([
            ("w0".to_string(), WorkerState::Starting),
            ("w9".to_string(), WorkerState::Running),
        ]);
        let slurm = SlurmState {
            tasks: tasks(&[
                ("9_0", "RUNNING"),
                ("9_1", "RUNNING"),
                ("9_2", "RUNNING"),
                ("9_3", "PENDING"),
            ]),
            ..Default::default()
        };
        let mut before_tasks = before_tasks;
        before_tasks.insert("9_4".into(), "RUNNING".into());
        let workers = BTreeMap::from([
            ("w0".to_string(), WorkerState::Running),
            ("w1".to_string(), WorkerState::Starting),
        ]);

        let events = lifecycle_events(false, &before_workers, &workers, &before_tasks, &slurm);
        assert_eq!(
            kinds(&events),
            [
                ("job_start", "9_0 RUNNING"),
                ("job_start", "9_2 RUNNING"),
                ("job_queued", "9_3 PENDING"),
                ("job_end", "9_4 left the queue"),
                ("worker_live", "trial loop running"),
                ("worker_live", "loading dataset"),
                ("worker_lost", "heartbeat went stale"),
            ]
        );
        assert_eq!(events[4].worker, "w0");
        assert_eq!(events[6].worker, "w9");

        assert!(lifecycle_events(false, &workers, &workers, &slurm.tasks, &slurm).is_empty());
    }

    #[test]
    fn phase_bands_sum_to_the_whole_window() {
        let spans = phase_spans(3600, 3600 * 10, 3600);
        let total: f64 = spans.iter().map(|(f, _)| f).sum();
        assert!((total - 1.0).abs() < 1e-9, "bands summed to {total}");
        assert_eq!(
            spans.iter().map(|(_, n)| *n).collect::<Vec<_>>(),
            ["explore", "exploit", "polish", "reserve"]
        );
    }

    #[test]
    fn a_nearly_finished_window_has_no_negative_bands() {
        let spans = phase_spans(600, 60, 3600);
        assert!(spans.iter().all(|(f, _)| *f >= 0.0), "{spans:?}");
        let total: f64 = spans.iter().map(|(f, _)| f).sum();
        assert!((total - 1.0).abs() < 1e-9);
    }

    #[test]
    #[ignore = "needs a campaign directory in GISARD_DASHBOARD_DIR"]
    fn render_a_real_campaign() {
        let dir = std::env::var("GISARD_DASHBOARD_DIR")
            .expect("set GISARD_DASHBOARD_DIR to a campaign directory");
        let (width, height): (u16, u16) = std::env::var("GISARD_DASHBOARD_SIZE")
            .ok()
            .and_then(|v| {
                let (w, h) = v.split_once('x')?;
                Some((w.parse().ok()?, h.parse().ok()?))
            })
            .unwrap_or((160, 46));
        let mut monitor = Monitor::new(PathBuf::from(&dir));
        let mut app = App::new(PathBuf::from(&dir));
        app.accept(monitor.collect());
        app.selected = std::env::var("GISARD_DASHBOARD_SELECT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        for row in 0..height {
            let line: String = (0..width)
                .map(|col| buffer.cell((col, row)).map_or(' ', |c| c.symbol().chars().next().unwrap_or(' ')))
                .collect();
            println!("{}", line.trim_end());
        }
    }

    #[test]
    fn full_dashboard_renders_at_a_normal_terminal_size() {
        let mut app = App::new(PathBuf::from("campaign-test"));
        app.accept(Snapshot {
            captured_unix: now_unix(),
            fetch_ms: 12,
            elapsed: 900,
            remaining: 7200,
            reserve: 3600,
            completed: 12,
            running: 5,
            worker_ids: 5,
            live_workers: 5,
            starting_workers: 0,
            expected_workers: 5,
            recent_five: 2,
            recent_hour: 12,
            throughput: 48.0,
            event_count: 3,
            scenarios: vec![ScenarioState {
                id: "t025-k50".into(),
                best: Some(1234),
                gain_hour: Some(7),
                completed: 12,
                completed_hour: 12,
                running: 5,
                wall_share: 100.0,
                score_age: Some(3),
                stages: "r0 12+5".into(),
            }],
            densities: vec![DensityState {
                spacing: 0.5,
                completed: 12,
                running: 5,
                promoted: 1,
                best_sum: 1234,
            }],
            events: vec![CampaignEvent {
                unix_ms: now_unix() * 1000,
                kind: "trial_complete".into(),
                worker: "worker-1".into(),
                trial: Some("trial".into()),
                scenario: Some("t025-k50".into()),
                score: Some(1234),
                rung: Some(0),
                spacing: Some(0.5),
                detail: None,
            }],
            histories: HashMap::from([("t025-k50".into(), vec![(1, 1200), (2, 1234)])]),
            alerts: Vec::new(),
            slurm: SlurmState {
                running: 1,
                expected: 1,
                polish: "PENDING".into(),
                harvest: "PENDING".into(),
                ..Default::default()
            },
        });
        for (width, height) in [(160, 42), (80, 24)] {
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal.draw(|frame| app.draw(frame)).unwrap();
            if width == 160 {
                let buffer = terminal.backend().buffer();
                let row_at = |y: u16| -> String {
                    (0..width)
                        .map(|col| {
                            buffer
                                .cell((col, y))
                                .map_or(' ', |c| c.symbol().chars().next().unwrap_or(' '))
                        })
                        .collect()
                };
                let row = (0..height)
                    .map(row_at)
                    .find(|line| line.contains('\u{2588}') && line.contains('\u{2591}'))
                    .expect("no clock bar row rendered");
                let bar = row.trim_matches(['\u{250c}', '\u{2502}', '\u{2510}', '\u{2514}', '\u{2518}']);
                assert_eq!(bar.chars().count(), width as usize - 2, "clock row: {row:?}");
                assert!(
                    bar.chars().all(|c| matches!(c, '\u{2588}' | '\u{2591}' | '\u{258c}')),
                    "clock row has non-bar cells: {bar:?}"
                );
                assert!(bar.contains('\u{2588}') && bar.contains('\u{2591}'), "no progress split");
            }
            }
    }
}

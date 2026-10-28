use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use framework::Point;
use serde::{Deserialize, Serialize};

use super::trial::{Trial, TrialResult};

pub struct Queue {
    root: PathBuf,
}

const CANDIDATES_KEPT: usize = 8;

#[derive(Clone, Debug)]
pub struct RunningClaim {
    pub trial_id: String,
    pub worker: String,
    pub trial: Option<Trial>,
    pub claimed_unix: u64,
    pub age_seconds: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TimedResult {
    pub result: TrialResult,
    pub completed_unix: u64,
}

#[derive(Serialize, Deserialize)]
struct RunningMarker {
    worker: String,
    trial: Trial,
    claimed_unix: u64,
}

impl Queue {
    pub fn new(root: &Path) -> Self {
        Queue {
            root: root.to_path_buf(),
        }
    }

    pub fn init(&self) -> io::Result<()> {
        for dir in [
            "queue/running",
            "results",
            "best",
            "solutions",
            "logs",
            "events/workers",
        ] {
            fs::create_dir_all(self.root.join(dir))?;
        }
        Ok(())
    }

    pub fn try_claim(&self, trial: &Trial, worker: &str, lease_seconds: u64) -> bool {
        if self.result_path(trial).exists() {
            return false;
        }
        let marker = self.marker_path(&trial.id);
        let body = serde_json::to_string(&RunningMarker {
            worker: worker.to_string(),
            trial: trial.clone(),
            claimed_unix: unix_time(SystemTime::now()),
        })
        .expect("running marker is serializable");
        match create_exclusive(&marker, &body) {
            Ok(()) => true,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                if !is_stale(&marker, lease_seconds) {
                    return false;
                }
                let _ = fs::remove_file(&marker);
                create_exclusive(&marker, &body).is_ok()
            }
            Err(_) => false,
        }
    }

    pub fn is_finished(&self, trial: &Trial) -> bool {
        self.result_path(trial).exists()
    }

    pub fn release(&self, trial: &Trial) {
        let _ = fs::remove_file(self.marker_path(&trial.id));
    }

    pub fn complete(&self, result: &TrialResult) -> io::Result<()> {
        let dir = self.root.join("results").join(&result.trial.scenario);
        let written = fs::create_dir_all(&dir).and_then(|()| {
            let body = serde_json::to_string(result).expect("a result is serializable");
            write_atomic(
                &dir.join(format!("{}.json", result.trial.id)),
                body.as_bytes(),
            )
        });
        let _ = fs::remove_file(self.marker_path(&result.trial.id));
        written
    }

    pub fn results(&self) -> Vec<TrialResult> {
        self.results_since(&mut HashSet::new())
    }

    pub fn timed_results(&self) -> Vec<TimedResult> {
        self.timed_results_since(&mut HashSet::new())
    }

    pub fn timed_results_since(&self, seen: &mut HashSet<String>) -> Vec<TimedResult> {
        let mut out = Vec::new();
        let scenarios = fs::read_dir(self.root.join("results"))
            .into_iter()
            .flatten();
        for scenario in scenarios.flatten() {
            for entry in fs::read_dir(scenario.path())
                .into_iter()
                .flatten()
                .flatten()
            {
                let path = entry.path();
                let Some(id) = path.file_stem().and_then(|name| name.to_str()) else {
                    continue;
                };
                if seen.contains(id) {
                    continue;
                }
                let Ok(body) = fs::read_to_string(&path) else {
                    continue;
                };
                let Ok(result) = serde_json::from_str::<TrialResult>(&body) else {
                    continue;
                };
                let completed_unix = fs::metadata(&path)
                    .and_then(|metadata| metadata.modified())
                    .map(unix_time)
                    .unwrap_or(0);
                out.push(TimedResult {
                    result,
                    completed_unix,
                });
                seen.insert(id.to_string());
            }
        }
        out.sort_unstable_by_key(|timed| timed.completed_unix);
        out
    }

    pub fn running_claims(&self) -> Vec<RunningClaim> {
        let now = unix_time(SystemTime::now());
        let mut claims = Vec::new();
        for entry in fs::read_dir(self.root.join("queue/running"))
            .into_iter()
            .flatten()
            .flatten()
        {
            let path = entry.path();
            let trial_id = entry.file_name().to_string_lossy().to_string();
            let body = fs::read_to_string(&path).unwrap_or_default();
            let parsed = serde_json::from_str::<RunningMarker>(&body).ok();
            let modified = fs::metadata(&path)
                .and_then(|metadata| metadata.modified())
                .map(unix_time)
                .unwrap_or(now);
            let claimed_unix = parsed
                .as_ref()
                .map_or(modified, |marker| marker.claimed_unix);
            claims.push(RunningClaim {
                trial_id,
                worker: parsed
                    .as_ref()
                    .map_or_else(|| body.trim().to_string(), |marker| marker.worker.clone()),
                trial: parsed.map(|marker| marker.trial),
                claimed_unix,
                age_seconds: now.saturating_sub(claimed_unix),
            });
        }
        claims.sort_unstable_by(|a, b| a.trial_id.cmp(&b.trial_id));
        claims
    }

    pub fn results_since(&self, seen: &mut HashSet<String>) -> Vec<TrialResult> {
        let mut out = Vec::new();
        let scenarios = fs::read_dir(self.root.join("results"))
            .into_iter()
            .flatten();
        for scenario in scenarios.flatten() {
            for entry in fs::read_dir(scenario.path())
                .into_iter()
                .flatten()
                .flatten()
            {
                let path = entry.path();
                let Some(id) = path.file_stem().and_then(|n| n.to_str()) else {
                    continue;
                };
                if seen.contains(id) {
                    continue;
                }
                if let Ok(body) = fs::read_to_string(&path)
                    && let Ok(result) = serde_json::from_str::<TrialResult>(&body)
                {
                    seen.insert(id.to_string());
                    out.push(result);
                }
            }
        }
        out
    }

    pub fn record_candidate(
        &self,
        scenario: &str,
        serviced: usize,
        trial: &str,
        antennas: &[Point],
    ) -> io::Result<bool> {
        let dir = self.root.join("best").join(scenario).join("cand");
        fs::create_dir_all(&dir)?;
        let name = format!("{serviced:010}-{trial}.txt");
        let path = dir.join(name);
        if path.exists() {
            return Ok(false);
        }
        if candidate_cutoff(&dir, CANDIDATES_KEPT)?.is_some_and(|cutoff| serviced < cutoff) {
            return Ok(false);
        }
        let mut body = String::new();
        for a in antennas {
            body.push_str(&format!("{:?},{:?}\n", a.x, a.y));
        }
        write_atomic(&path, body.as_bytes())?;
        prune(&dir, CANDIDATES_KEPT);
        Ok(path.exists())
    }

    pub fn store_solution(
        &self,
        scenario: &str,
        trial: &str,
        antennas: &[Point],
    ) -> io::Result<()> {
        let dir = self.root.join("solutions").join(scenario);
        fs::create_dir_all(&dir)?;
        let mut body = String::new();
        for a in antennas {
            body.push_str(&format!("{:?},{:?}\n", a.x, a.y));
        }
        write_atomic(&dir.join(format!("{trial}.txt")), body.as_bytes())
    }

    pub fn load_solution(&self, scenario: &str, trial: &str) -> Option<Vec<Point>> {
        let path = self
            .root
            .join("solutions")
            .join(scenario)
            .join(format!("{trial}.txt"));
        let body = fs::read_to_string(path).ok()?;
        let points: Vec<Point> = body
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| {
                let (x, y) = l.split_once(',')?;
                Some(Point::new(x.trim().parse().ok()?, y.trim().parse().ok()?))
            })
            .collect();
        (!points.is_empty()).then_some(points)
    }

    pub fn best_score(&self, scenario: &str) -> Option<usize> {
        fs::read_dir(self.root.join("best").join(scenario).join("cand"))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let path = e.path();
                let name = path.file_stem()?.to_str()?;
                name.split('-').next()?.parse().ok()
            })
            .max()
    }

    pub fn best_score_update(&self, scenario: &str) -> Option<(usize, u64)> {
        fs::read_dir(self.root.join("best").join(scenario).join("cand"))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| {
                let path = entry.path();
                let name = path.file_stem()?.to_str()?;
                let score = name.split('-').next()?.parse().ok()?;
                let updated = fs::metadata(&path)
                    .and_then(|metadata| metadata.modified())
                    .map(unix_time)
                    .unwrap_or(0);
                Some((score, updated))
            })
            .max_by_key(|&(score, updated)| (score, updated))
    }

    pub fn candidate_count(&self, scenario: &str) -> usize {
        fs::read_dir(self.root.join("best").join(scenario).join("cand"))
            .into_iter()
            .flatten()
            .flatten()
            .count()
    }

    fn marker_path(&self, id: &str) -> PathBuf {
        self.root.join("queue/running").join(id)
    }

    fn result_path(&self, trial: &Trial) -> PathBuf {
        self.root
            .join("results")
            .join(&trial.scenario)
            .join(format!("{}.json", trial.id))
    }
}

fn write_atomic(path: &Path, body: &[u8]) -> io::Result<()> {
    let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
    fs::write(&temporary, body)?;
    fs::rename(&temporary, path)
}

fn candidate_cutoff(dir: &Path, keep: usize) -> io::Result<Option<usize>> {
    let mut scores: Vec<usize> = fs::read_dir(dir)?
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let name = path.file_stem()?.to_str()?;
            path.extension()
                .is_some_and(|extension| extension == "txt")
                .then(|| name.split('-').next()?.parse().ok())?
        })
        .collect();
    if scores.len() < keep {
        return Ok(None);
    }
    scores.sort_unstable_by(|a, b| b.cmp(a));
    Ok(scores.get(keep - 1).copied())
}

fn prune(dir: &Path, keep: usize) {
    let mut names: Vec<String> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .filter(|n| n.ends_with(".txt"))
        .collect();
    if names.len() <= keep {
        return;
    }
    names.sort_unstable();
    for name in &names[..names.len() - keep] {
        let _ = fs::remove_file(dir.join(name));
    }
}

fn create_exclusive(path: &Path, body: &str) -> io::Result<()> {
    use std::io::Write;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(body.as_bytes())
}

fn is_stale(path: &Path, lease_seconds: u64) -> bool {
    let Ok(modified) = fs::metadata(path).and_then(|m| m.modified()) else {
        return false;
    };
    let age = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|now| now.as_secs())
        .unwrap_or(0)
        .saturating_sub(
            modified
                .duration_since(UNIX_EPOCH)
                .map(|t| t.as_secs())
                .unwrap_or(0),
        );
    age >= lease_seconds
}

fn unix_time(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::super::trial::TrialConfig;
    use super::*;

    fn queue() -> (Queue, PathBuf) {
        static N: AtomicU32 = AtomicU32::new(0);
        let root = std::env::temp_dir().join(format!(
            "gisard-queue-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let queue = Queue::new(&root);
        queue.init().unwrap();
        (queue, root)
    }

    fn trial(seed: u64) -> Trial {
        Trial::new("t050-k100", TrialConfig::default(), 0, 20, seed)
    }

    fn finish(queue: &Queue, trial: &Trial, worker: &str, serviced: usize) {
        queue
            .complete(&TrialResult {
                trial: trial.clone(),
                serviced,
                seconds: 1.0,
                worker: worker.into(),
                completed_unix_ms: 0,
            })
            .unwrap();
    }

    #[test]
    fn exactly_one_worker_wins_a_claim() {
        let (queue, root) = queue();
        let trial = trial(1);
        let winners = ["a", "b", "c", "d"]
            .iter()
            .filter(|w| queue.try_claim(&trial, w, 3600))
            .count();
        assert_eq!(winners, 1, "a claim must be exclusive");
        let claims = queue.running_claims();
        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0].trial.as_ref().unwrap().id, trial.id);
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn a_finished_trial_is_never_handed_out_again() {
        let (queue, root) = queue();
        let trial = trial(2);
        assert!(queue.try_claim(&trial, "w1", 3600));
        finish(&queue, &trial, "w1", 1601);

        assert!(queue.is_finished(&trial));
        assert!(
            !queue.try_claim(&trial, "w2", 3600),
            "a completed trial must never be reissued"
        );
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn a_claimed_trial_is_always_marked_or_recorded() {
        let (queue, root) = queue();
        let trial = trial(3);
        assert!(queue.try_claim(&trial, "w1", 3600));
        assert!(root.join("queue/running").join(&trial.id).exists());
        assert!(!queue.is_finished(&trial));

        finish(&queue, &trial, "w1", 10);
        assert!(!root.join("queue/running").join(&trial.id).exists());
        assert!(queue.is_finished(&trial));
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn an_in_flight_trial_is_reserved_until_its_lease_expires() {
        let (queue, root) = queue();
        let trial = trial(4);
        assert!(queue.try_claim(&trial, "w1", 3600));
        assert!(!queue.try_claim(&trial, "w2", 3600), "w1 holds it");
        assert!(
            queue.try_claim(&trial, "w2", 0),
            "an expired lease returns a dead node's work to the pool"
        );
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn releasing_returns_a_trial_to_the_pool() {
        let (queue, root) = queue();
        let trial = trial(5);
        assert!(queue.try_claim(&trial, "w1", 3600));
        queue.release(&trial);
        assert!(queue.try_claim(&trial, "w2", 3600));
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn completing_records_the_result() {
        let (queue, root) = queue();
        let trial = trial(6);
        assert!(queue.try_claim(&trial, "w1", 3600));
        finish(&queue, &trial, "w1", 1601);

        let results = queue.results();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].serviced, 1601);
        assert_eq!(results[0].worker, "w1");
        let timed = queue.timed_results();
        assert_eq!(timed.len(), 1);
        assert!(timed[0].completed_unix > 0);
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn the_index_is_read_incrementally() {
        let (queue, root) = queue();
        let mut seen = HashSet::new();

        for seed in 0..5 {
            let t = trial(seed);
            queue.try_claim(&t, "w1", 3600);
            finish(&queue, &t, "w1", 100 + seed as usize);
        }
        let first = queue.results_since(&mut seen);
        assert_eq!(first.len(), 5);
        assert_eq!(seen.len(), 5);

        assert!(
            queue.results_since(&mut seen).is_empty(),
            "an unchanged index must yield nothing to re-read"
        );

        let t = trial(99);
        queue.try_claim(&t, "w1", 3600);
        finish(&queue, &t, "w1", 999);
        let second = queue.results_since(&mut seen);
        assert_eq!(second.len(), 1, "only the new result");
        assert_eq!(second[0].serviced, 999);
        assert_eq!(seen.len(), 6);
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn reading_the_whole_index_still_works() {
        let (queue, root) = queue();
        for seed in 0..3 {
            let t = trial(seed);
            queue.try_claim(&t, "w1", 3600);
            finish(&queue, &t, "w1", 10);
        }
        assert_eq!(queue.results().len(), 3);
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn a_stored_solution_round_trips_exactly() {
        let (queue, root) = queue();
        let pts = vec![
            Point::new(0.1 + 0.2, 3768312.3970084996),
            Point::new(-1.0e-17, f64::MAX),
        ];
        queue.store_solution("s", "abc", &pts).unwrap();
        let back = queue.load_solution("s", "abc").expect("stored");
        assert_eq!(back.len(), pts.len());
        for (a, b) in pts.iter().zip(&back) {
            assert_eq!(a.x.to_bits(), b.x.to_bits());
            assert_eq!(a.y.to_bits(), b.y.to_bits());
        }
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn a_missing_solution_is_not_an_error() {
        let (queue, root) = queue();
        assert!(queue.load_solution("s", "never-written").is_none());
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn best_is_the_highest_scoring_candidate_filename() {
        let (queue, root) = queue();
        let a = [Point::new(1.0, 2.0)];
        queue.record_candidate("s", 900, "aaa", &a).unwrap();
        queue.record_candidate("s", 1601, "bbb", &a).unwrap();
        queue.record_candidate("s", 1200, "ccc", &a).unwrap();

        assert_eq!(queue.best_score("s"), Some(1601));
        assert_eq!(queue.best_score("absent"), None);
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn the_candidate_pool_stays_bounded() {
        let (queue, root) = queue();
        let a = [Point::new(1.0, 2.0)];
        for score in 1..40 {
            queue.record_candidate("s", score, "t", &a).unwrap();
        }
        let kept = fs::read_dir(root.join("best/s/cand")).unwrap().count();
        assert_eq!(kept, CANDIDATES_KEPT, "checkpoints must not accumulate");
        assert_eq!(queue.best_score("s"), Some(39), "the best is never pruned");
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn candidates_below_a_full_pool_cutoff_are_not_written() {
        let (queue, root) = queue();
        let a = [Point::new(1.0, 2.0)];
        for score in 100..100 + CANDIDATES_KEPT {
            assert!(
                queue
                    .record_candidate("s", score, &format!("seed-{score}"), &a)
                    .unwrap()
            );
        }

        assert!(!queue.record_candidate("s", 99, "low", &a).unwrap());
        assert!(!root.join("best/s/cand/0000000099-low.txt").exists());
        assert_eq!(queue.candidate_count("s"), CANDIDATES_KEPT);
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn a_cutoff_tie_may_replace_an_equal_score_alternative() {
        let (queue, root) = queue();
        let a = [Point::new(1.0, 2.0)];
        for score in 100..100 + CANDIDATES_KEPT {
            queue
                .record_candidate("s", score, &format!("seed-{score}"), &a)
                .unwrap();
        }

        assert!(queue.record_candidate("s", 100, "zzz", &a).unwrap());
        assert!(root.join("best/s/cand/0000000100-zzz.txt").exists());
        assert_eq!(queue.candidate_count("s"), CANDIDATES_KEPT);
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn publishing_the_same_candidate_twice_is_not_a_new_checkpoint() {
        let (queue, root) = queue();
        let a = [Point::new(1.0, 2.0)];
        assert!(queue.record_candidate("s", 10, "same", &a).unwrap());
        assert!(!queue.record_candidate("s", 10, "same", &a).unwrap());
        assert_eq!(queue.candidate_count("s"), 1);
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn candidates_keep_full_coordinate_precision() {
        let (queue, root) = queue();
        let p = Point::new(0.1 + 0.2, 3768312.3970084996);
        queue.record_candidate("s", 1, "aaa", &[p]).unwrap();
        let body = fs::read_to_string(root.join("best/s/cand/0000000001-aaa.txt")).unwrap();
        let parsed: Vec<f64> = body.trim().split(',').map(|f| f.parse().unwrap()).collect();
        assert_eq!(parsed[0].to_bits(), p.x.to_bits());
        assert_eq!(parsed[1].to_bits(), p.y.to_bits());
        fs::remove_dir_all(root).ok();
    }
}
